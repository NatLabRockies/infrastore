//! OpenAPI-row JSON serde for the two association catalogs.
//!
//! `time_series_associations` and `supplemental_attribute_associations` are
//! the store's own tables; this module is the only place that knows how a row
//! of either one maps to and from the wire spelling SiennaSchemas defines
//! (`TimeSeries/*.json`, `Core/Associations/SupplementalAttributeAssociation.json`),
//! through the generated types of the `power-openapi-models` SDK. Everything else in the crate —
//! the catalog schema, the metadata query, the association tables — is
//! untouched by this module; it only maps rows [`crate::store::Store`]
//! already produces into JSON, and JSON back into the calls that already
//! exist for writing.
//!
//! # Wire contract
//!
//! A time-series association row carries the fields every type shares —
//! `owner_id`, `owner_type`, `owner_category`, `time_series_type`, `name`,
//! `features` (a *plain* scalar map — int, float, bool, or string values, never
//! the store's internally-tagged [`crate::types::metadata::FeatureValue`] form),
//! `uri` (the schema's locator for the dense data, unique within one store, no
//! required format — infrastore fills it with [`crate::hash::hash_hex`] of the
//! row's own `data_hash`, never a caller-supplied value), `element_type`,
//! `element_shape` — plus optional descriptive fields (`units`,
//! `quantity_kind`, `unit_system`, `component_field`, `application_data`) and
//! the optional `data_hash` (the same hex string as `uri`, exported because
//! the schema also carries it as its own field) that are *omitted* from the
//! JSON object when unset, never written as `null`. `unit_system` maps the
//! store's snake_case internal spelling to the schema's `NATURAL_UNITS` /
//! `COMPONENT_BASE`; omitted (not merely absent) means *unspecified*, which is
//! a different thing from natural units, so the two must stay distinguishable
//! through the round trip.
//!
//! Three fields reached the schema after the rest (`timestamps_uri`, below, is
//! the third): `time_reference`,
//! the catalog spelling of [`TimeReference`] (`"utc"`, `"zoneless"`, an offset,
//! or a zone name), and `array_shape`, the stored array's full native shape —
//! `[length, *element_shape]` in the catalog's own terms, where the schema's
//! `element_shape` is only the per-step trailing shape (see
//! [`wire_element_shape`]). Both exist so that a row imported from a document
//! is *identical* to the row that was exported: the forecast layouts are
//! conventions the caller owns, not rules the store enforces, so the native
//! shape cannot be reconstructed from `horizon`/`count`/`percentiles` and has
//! to travel. Neither is required, so a producer predating them writes rows
//! without them; an import that finds `array_shape` absent falls back to the
//! schema fields, which is exact for the static types and a best effort for
//! forecasts.
//!
//! A time-series row also carries **`association_id`**, the wire spelling of
//! [`TimeSeriesMetadata::id`], required by the schema on all six types.
//! Carrying it is what makes the round trip *preserve* the references a
//! document holds rather than merely reproduce its rows. The
//! supplemental-attribute wire form carries no id.
//!
//! A `NonSequentialTimeSeries` carries one field no other type has:
//! **`timestamps_uri`**, the locator for its explicit time axis, filled from
//! the axis's own [`crate::hash::timestamps_hash`] exactly as `uri` is filled
//! from the array's. It is a *locator* rather than the vector because the axis
//! is shared — a cohort on one axis names it once each, where inlining the
//! timestamps would repeat the whole vector per row — and it cannot be left out
//! and inferred, because arrays are content-addressed: two irregular series
//! with byte-identical values on *different* axes share one stored array, and
//! only the catalog's `timestamps_hash` tells them apart. The axis ships in the
//! array file beside the arrays, so [`import_ts_rows`] resolves the locator
//! against the store and hands the vector to the ordinary insert path.
//!
//! On top of the shared fields, each of the six [`TimeSeriesType`] values adds
//! its own geometry fields — see [`ts_row_to_wire`] — and every field that
//! does not apply to a row's type is absent from that row's object, never
//! present as `null`. The five types that carry `interval_alignment` always
//! write the schema default `PERIOD_ENDING`, the only value the catalog stores.
//!
//! `initial_timestamp` is RFC3339 UTC, floored to millisecond precision (see
//! [`floor_to_millis`]) — the store's own precision floor
//! (`crate::timestamps` module docs), and all the OpenAPI wire form promises.
//! `resolution` / `horizon` / `interval` are [`Period::to_iso8601`]'s canonical
//! spelling, which is not always the "seconds" form a hand-written fixture
//! might guess (`PT3600S` canonicalizes to `PT1H`).
//!
//! # Export sort order
//!
//! [`export_ts_rows`] sorts the array by the identity tuple `(owner_id,
//! owner_category code, time_series_type code, name, resolution, interval,
//! features)`, compared as **typed** values — see [`SortKey`] for why that
//! matters and how periods and features participate in a total order despite
//! not being numeric.

use chrono::{DateTime, Utc};
use power_openapi_models::infrastructure_core::SupplementalAttributeAssociation as WireSaRow;
use power_openapi_models::timeseries::{self as wire, TimeSeriesAssociation};
use serde_json::{Map, Value};

use crate::error::{Result, TimeSeriesError};
use crate::metadata::{SupplementalAttributeAssociation, SupplementalAttributeFilter};
use crate::store::{ListFilter, Store};
use crate::types::id::TimeSeriesId;
use crate::types::metadata::{
    FeatureValue, Features, OwnerCategory, TimeSeriesMetadata, UnitSystem,
};
use crate::types::period::Period;
use crate::types::time_reference::TimeReference;
use crate::types::time_series::TimeSeriesType;

mod schema;

// ============================================================================
// Time-series associations: export
// ============================================================================

/// The identity tuple export sorts by: `(owner_id, owner_category code,
/// time_series_type code, name, resolution, interval, features)`, compared as
/// **typed** values rather than their string spellings.
///
/// This is deliberate, not cosmetic: the catalog's own hashing philosophy
/// (`crate::hash::features_hash` digests a [`FeatureValue`] by its bits, never
/// by a stringified rendering) extends to the export order. Comparing
/// `owner_id` as a string would sort `10` before `2`; comparing a
/// [`FeatureValue::Int`] `1` against a [`FeatureValue::Str`] `"1"` by their
/// JSON spelling would collide two values the catalog treats as distinct.
///
/// `resolution` / `interval` compare by their canonical ISO-8601 string
/// (`Period::to_iso8601`): a legal, deterministic total order, even though it
/// is not magnitude-monotonic (`"P1D"` sorts before `"PT2H"` lexically, which
/// is backwards numerically). The order only has to be deterministic, not
/// numeric, and the canonical string is exactly what [`Period`]'s `Eq`/`Hash`
/// already key on, so two periods that the catalog treats as equal always
/// produce equal sort keys.
///
/// `features` piggybacks on [`Features`]'s own `Ord` (a `BTreeMap` ordered by
/// key, then by [`FeatureValue`]'s `Ord` — kind first, then value, floats by
/// bit pattern) rather than re-deriving a comparison here.
type SortKey = (
    i64,
    i64,
    i64,
    String,
    Option<String>,
    Option<String>,
    Features,
);

fn sort_key(meta: &TimeSeriesMetadata) -> SortKey {
    (
        meta.owner_id,
        meta.owner_category.code(),
        meta.time_series_type.code(),
        meta.name.clone(),
        meta.resolution.map(|p| p.to_iso8601()),
        meta.interval.map(|p| p.to_iso8601()),
        meta.features.clone(),
    )
}

/// A plain scalar map: `{"scenario": "high_load", "year": 2030}`, never the
/// store's internally-tagged [`FeatureValue`] form. The OpenAPI document's
/// spelling of a series' features.
pub fn features_to_plain(features: &Features) -> Map<String, Value> {
    let mut out = Map::with_capacity(features.len());
    for (key, value) in features {
        let json = match value {
            FeatureValue::Int(i) => Value::from(*i),
            FeatureValue::Float(f) => Value::from(*f),
            FeatureValue::Bool(b) => Value::from(*b),
            FeatureValue::Str(s) => Value::from(s.clone()),
        };
        out.insert(key.clone(), json);
    }
    out
}

/// The fields every one of the six row types shares, in the SDK's types.
///
/// The six generated row structs cannot share a type, so [`wire_row`] spreads
/// this into whichever one a row is, and [`shared_of`] gathers it back.
struct Shared {
    association_id: i64,
    owner_id: i64,
    owner_type: String,
    owner_category: wire::OwnerCategory,
    name: String,
    features: wire::TimeSeriesFeatures,
    uri: String,
    data_hash: Option<String>,
    element_type: String,
    element_shape: Vec<i64>,
    array_shape: Option<Vec<i64>>,
    units: Option<String>,
    quantity_kind: Option<String>,
    unit_system: Option<wire::UnitSystem>,
    time_reference: Option<String>,
    component_field: Option<String>,
    application_data: Option<String>,
}

/// Build the `$variant` row from a [`Shared`] plus the type's own fields.
macro_rules! wire_row {
    ($variant:ident, $shared:expr, { $($field:ident: $value:expr),* $(,)? }) => {{
        let s: Shared = $shared;
        TimeSeriesAssociation::$variant(wire::$variant {
            association_id: s.association_id,
            owner_id: s.owner_id,
            owner_type: s.owner_type,
            owner_category: s.owner_category,
            time_series_type: Default::default(),
            name: s.name,
            features: s.features,
            uri: s.uri,
            data_hash: s.data_hash,
            element_type: s.element_type,
            element_shape: s.element_shape,
            array_shape: s.array_shape,
            units: s.units,
            quantity_kind: s.quantity_kind,
            unit_system: s.unit_system,
            time_reference: s.time_reference,
            component_field: s.component_field,
            application_data: s.application_data,
            $($field: $value,)*
        })
    }};
}

/// The inverse of [`wire_row`]: move the shared fields out of one row.
macro_rules! shared_of {
    ($row:ident) => {
        Shared {
            association_id: $row.association_id,
            owner_id: $row.owner_id,
            owner_type: $row.owner_type,
            owner_category: $row.owner_category,
            name: $row.name,
            features: $row.features,
            uri: $row.uri,
            data_hash: $row.data_hash,
            element_type: $row.element_type,
            element_shape: $row.element_shape,
            array_shape: $row.array_shape,
            units: $row.units,
            quantity_kind: $row.quantity_kind,
            unit_system: $row.unit_system,
            time_reference: $row.time_reference,
            component_field: $row.component_field,
            application_data: $row.application_data,
        }
    };
}

fn owner_category_wire(category: OwnerCategory) -> wire::OwnerCategory {
    match category {
        OwnerCategory::Component => wire::OwnerCategory::Component,
        OwnerCategory::SupplementalAttribute => wire::OwnerCategory::SupplementalAttribute,
    }
}

fn owner_category_from_wire(category: wire::OwnerCategory) -> OwnerCategory {
    match category {
        wire::OwnerCategory::Component => OwnerCategory::Component,
        wire::OwnerCategory::SupplementalAttribute => OwnerCategory::SupplementalAttribute,
    }
}

fn unit_system_wire(system: UnitSystem) -> wire::UnitSystem {
    match system {
        UnitSystem::NaturalUnits => wire::UnitSystem::NaturalUnits,
        UnitSystem::ComponentBase => wire::UnitSystem::ComponentBase,
    }
}

fn unit_system_from_wire(system: wire::UnitSystem) -> UnitSystem {
    match system {
        wire::UnitSystem::NaturalUnits => UnitSystem::NaturalUnits,
        wire::UnitSystem::ComponentBase => UnitSystem::ComponentBase,
    }
}

fn features_wire(features: &Features) -> wire::TimeSeriesFeatures {
    let additional_properties = features
        .iter()
        .map(|(key, value)| {
            let value = match value {
                FeatureValue::Int(i) => wire::TimeSeriesFeatureValue::Integer(*i),
                FeatureValue::Float(f) => wire::TimeSeriesFeatureValue::Number(*f),
                FeatureValue::Bool(b) => wire::TimeSeriesFeatureValue::Boolean(*b),
                FeatureValue::Str(s) => wire::TimeSeriesFeatureValue::String(s.clone()),
            };
            (key.clone(), value)
        })
        .collect();
    wire::TimeSeriesFeatures {
        additional_properties,
    }
}

fn features_from_wire(features: wire::TimeSeriesFeatures) -> Features {
    features
        .additional_properties
        .into_iter()
        .map(|(key, value)| {
            let value = match value {
                wire::TimeSeriesFeatureValue::Integer(i) => FeatureValue::Int(i),
                wire::TimeSeriesFeatureValue::Number(f) => FeatureValue::Float(f),
                wire::TimeSeriesFeatureValue::Boolean(b) => FeatureValue::Bool(b),
                wire::TimeSeriesFeatureValue::String(s) => FeatureValue::Str(s),
            };
            (key, value)
        })
        .collect()
}

/// Floored to millisecond precision: the write path refuses a finer instant,
/// but reads stay permissive for an artifact written before that rule, so any
/// finer component such a catalog holds is dropped rather than surfaced. The
/// SDK then writes a whole second with no fractional part and anything else
/// with exactly three digits.
fn floor_to_millis(ts: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(ts.timestamp_millis())
        .expect("a timestamp's own millisecond count is always representable")
}

/// The true per-step trailing shape (the wire contract's `element_shape`), derived from the
/// catalog's own `element_shape` column.
///
/// [`crate::types::array::TypedArray::element_shape`] — the source of
/// [`TimeSeriesMetadata::element_shape`] — is always `shape[1..]`: everything
/// after the array's very first axis. That is exactly the per-step shape for
/// a static series ([`TimeSeriesType::leading_dims`] `== 1`), but a forecast
/// stacks more axes in front of the per-step dims — windows
/// (`Deterministic`/`DeterministicSingleTimeSeries`, 2 leading dims) or
/// percentiles/scenarios plus windows (`Probabilistic`/`Scenarios`, 3) — so
/// `shape[1..]` for those still carries `count` (and, for the 3-leading-dim
/// types, `horizon` too) in front of the real per-step shape. This strips the
/// extra `leading_dims - 1` axes the catalog's own field leaves in, which is
/// exactly what a static series needs zero of.
fn wire_element_shape(meta: &TimeSeriesMetadata) -> &[usize] {
    let extra_leading = meta.time_series_type.leading_dims().saturating_sub(1);
    meta.element_shape.get(extra_leading..).unwrap_or(&[])
}

fn dims_wire(dims: &[usize]) -> Vec<i64> {
    dims.iter().map(|&d| d as i64).collect()
}

/// A field the row's type requires. A catalog row always has it, so its
/// absence is a malformed row, not an optional field.
fn required<T>(value: Option<T>, field: &str, meta: &TimeSeriesMetadata) -> Result<T> {
    value.ok_or_else(|| {
        TimeSeriesError::InvalidParameter(format!(
            "cannot export series {} ('{}'): a {} row requires {field}, and this one has none",
            meta.id.map_or(-1, |id| id.0),
            meta.name,
            meta.time_series_type.as_str(),
        ))
    })
}

fn shared_wire(meta: &TimeSeriesMetadata) -> Result<Shared> {
    let hash_hex = crate::hash::hash_hex(&meta.data_hash);
    // The native shape, so the import reproduces the catalog row exactly
    // (module docs). `length` is the array's first axis for every type.
    let array_shape = meta.length.map(|length| {
        let mut shape = vec![length as i64];
        shape.extend(dims_wire(&meta.element_shape));
        shape
    });
    Ok(Shared {
        association_id: required(meta.id, "an association_id", meta)?.get(),
        owner_id: meta.owner_id,
        owner_type: meta.owner_type.clone(),
        owner_category: owner_category_wire(meta.owner_category),
        name: meta.name.clone(),
        features: features_wire(&meta.features),
        uri: hash_hex.clone(),
        data_hash: Some(hash_hex),
        element_type: meta.element_type.to_string(),
        element_shape: dims_wire(wire_element_shape(meta)),
        array_shape,
        units: meta.units.clone(),
        quantity_kind: meta.quantity_kind.clone(),
        unit_system: meta.unit_system.map(unit_system_wire),
        time_reference: meta
            .time_reference
            .as_ref()
            .map(TimeReference::as_storage_string),
        component_field: meta.component_field.clone(),
        application_data: meta.application_data.clone(),
    })
}

/// Map one catalog row to its OpenAPI wire row. `uri` and `data_hash` are
/// both derived from the row's own `data_hash` via [`crate::hash::hash_hex`]
/// — infrastore never accepts a caller-supplied locator for its own rows.
fn ts_row_to_wire(meta: &TimeSeriesMetadata) -> Result<TimeSeriesAssociation> {
    let shared = shared_wire(meta)?;
    let initial_timestamp =
        || required(meta.initial_timestamp, "initial_timestamp", meta).map(floor_to_millis);
    let period =
        |value: Option<Period>, field| required(value, field, meta).map(|p| p.to_iso8601());
    let length = || required(meta.length, "length", meta).map(|l| l as i64);
    let count = || required(meta.count, "count", meta).map(|c| c as i64);

    macro_rules! forecast_row {
        ($variant:ident { $($field:ident: $value:expr),* $(,)? }) => {
            wire_row!($variant, shared, {
                interval_alignment: wire::IntervalAlignment::PeriodEnding,
                initial_timestamp: initial_timestamp()?,
                resolution: period(meta.resolution, "resolution")?,
                horizon: period(meta.horizon, "horizon")?,
                interval: period(meta.interval, "interval")?,
                count: count()?,
                $($field: $value,)*
            })
        };
    }

    Ok(match meta.time_series_type {
        TimeSeriesType::SingleTimeSeries => wire_row!(SingleTimeSeries, shared, {
            interval_alignment: wire::IntervalAlignment::PeriodEnding,
            initial_timestamp: initial_timestamp()?,
            resolution: period(meta.resolution, "resolution")?,
            length: length()?,
        }),
        // The axis locator, filled from the axis's own content hash exactly as
        // `uri` is filled from the array's.
        TimeSeriesType::NonSequentialTimeSeries => wire_row!(NonSequentialTimeSeries, shared, {
            timestamps_uri: Some(crate::hash::hash_hex(&crate::hash::timestamps_hash(
                required(meta.timestamps.as_deref(), "timestamps", meta)?,
            ))),
            length: length()?,
        }),
        // An infrastore-local extension with no schema; [`export_ts_rows`]
        // drops these rows before this mapping runs.
        TimeSeriesType::PersistentTimeSeries => {
            return Err(TimeSeriesError::InvalidParameter(
                "a PersistentTimeSeries has no OpenAPI row".into(),
            ));
        }
        TimeSeriesType::Deterministic => forecast_row!(Deterministic {}),
        TimeSeriesType::DeterministicSingleTimeSeries => {
            forecast_row!(DeterministicSingleTimeSeries {})
        }
        TimeSeriesType::Probabilistic => forecast_row!(Probabilistic {
            percentiles: required(meta.percentiles.clone(), "percentiles", meta)?,
        }),
        TimeSeriesType::Scenarios => forecast_row!(Scenarios {
            scenario_count: length()?,
        }),
    })
}

/// Export `time_series_associations` as a sorted OpenAPI-row JSON array. Pure
/// mapping over rows [`Store::list_metadata`] already produced — see the
/// module docs for the wire contract and sort order.
///
/// # `PersistentTimeSeries` does not travel
///
/// The wire contract is a `oneOf` over six canonical Sienna types and has no
/// schema for a seventh, so the document has no way to spell one. The type is
/// therefore **omitted**: a mixed store still exports its six-type rows, and
/// the rows this drops are exactly the rows no import — this store's included —
/// would take back.
///
/// A filter that names the type *itself* is a different request, and is
/// refused rather than answered with an empty array: a caller asking for those
/// rows specifically has asked for something this format cannot express, and
/// silence would read as "the store holds none".
fn export_ts_rows(store: &Store, filter: &ListFilter) -> Result<String> {
    if filter.time_series_type == Some(TimeSeriesType::PersistentTimeSeries) {
        return Err(TimeSeriesError::InvalidParameter(
            "cannot export PersistentTimeSeries as OpenAPI rows: the wire contract is a \
             oneOf over the six canonical types and has no schema for this one, which is an \
             infrastore-local extension. An unfiltered export omits these rows and emits the \
             rest"
                .into(),
        ));
    }
    let rows = store.list_with_timestamps(filter.clone())?;
    // Persistent rows are dropped first, so they are never checked. The store
    // keeps an infinite float feature, but JSON cannot spell one: serde_json
    // would write `null`, which the import then refuses. Refuse the export
    // instead of emitting a document that cannot come back.
    let rows: Vec<_> = rows
        .iter()
        .filter(|meta| meta.time_series_type != TimeSeriesType::PersistentTimeSeries)
        .collect();
    for meta in &rows {
        if let Some((key, _)) = meta
            .features
            .iter()
            .find(|(_, v)| matches!(v, FeatureValue::Float(f) if !f.is_finite()))
        {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "cannot export series {} ('{}'): feature {key:?} is not finite, which JSON \
                 cannot represent",
                meta.id.map_or(-1, |id| id.0),
                meta.name
            )));
        }
    }
    let mut keyed = rows
        .iter()
        .map(|meta| Ok((sort_key(meta), ts_row_to_wire(meta)?)))
        .collect::<Result<Vec<_>>>()?;
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let array: Vec<TimeSeriesAssociation> = keyed.into_iter().map(|(_, row)| row).collect();
    serde_json::to_string(&array).map_err(Into::into)
}

fn wire_err(msg: impl Into<String>) -> crate::error::TimeSeriesError {
    crate::error::TimeSeriesError::InvalidParameter(msg.into())
}

fn parse_period(s: &str, field: &str) -> Result<Period> {
    Period::from_iso8601(s)
        .map_err(|e| wire_err(format!("{field} {s:?} is not an ISO-8601 duration: {e}")))
}

/// The inverse of [`features_to_plain`]: a plain scalar map back into the
/// store's tagged feature values. Errors on a value that is not a JSON scalar.
pub fn features_from_plain(map: &Map<String, Value>) -> Result<Features> {
    let mut out = Features::new();
    for (key, value) in map {
        let feature = match value {
            Value::Bool(b) => FeatureValue::Bool(*b),
            // Integers before floats: JSON has one number type, and an integer
            // feature round-tripped as a float would be a different value to
            // the catalog, which hashes floats by their bits.
            Value::Number(n) if n.is_i64() => FeatureValue::Int(n.as_i64().unwrap()),
            Value::Number(n) => FeatureValue::Float(n.as_f64().ok_or_else(|| {
                wire_err(format!(
                    "feature {key:?} holds a number the store cannot store"
                ))
            })?),
            Value::String(s) => FeatureValue::Str(s.clone()),
            other => {
                return Err(wire_err(format!(
                    "feature {key:?} is {other}, but a feature value must be an int, float, \
                     bool, or string"
                )));
            }
        };
        out.insert(key.clone(), feature);
    }
    Ok(out)
}

/// One decoded row, plus the time axis its `timestamps_uri` named.
///
/// The axis travels beside the row rather than in it because only the store can
/// turn a locator into a vector, and [`ts_row_from_wire`] has no store.
/// `None` for every type but `NonSequentialTimeSeries`, which is the only one
/// with an explicit axis.
struct DecodedTsRow {
    meta: TimeSeriesMetadata,
    time_axis: Option<[u8; 32]>,
}

/// The fields whose presence depends on a row's type.
#[derive(Default)]
struct Timing {
    initial_timestamp: Option<DateTime<Utc>>,
    resolution: Option<String>,
    horizon: Option<String>,
    interval: Option<String>,
    length: Option<i64>,
    count: Option<i64>,
    percentiles: Option<Vec<f64>>,
    interval_alignment: Option<wire::IntervalAlignment>,
    timestamps_uri: Option<String>,
}

/// The 32-byte hash of the time axis a `NonSequentialTimeSeries` names.
///
/// Required there, and required to be a hash: unlike `uri` — which the
/// schema leaves as a free-form locator, and whose value this crate can
/// fall back to `data_hash` for — there is no second field carrying the
/// axis. A locator this store cannot resolve is a row it cannot file.
fn resolve_time_axis(name: &str, timestamps_uri: Option<&str>) -> Result<[u8; 32]> {
    let Some(locator) = timestamps_uri else {
        return Err(wire_err(format!(
            "row '{name}': a NonSequentialTimeSeries carries its time axis as timestamps_uri, \
             and this row has none — without it the document does not say which of the \
             store's axes the row sits on",
        )));
    };
    crate::hash::hash_from_hex(locator).ok_or_else(|| {
        wire_err(format!(
            "row '{name}': timestamps_uri {locator:?} is not a 64-character hex hash, and this \
             import resolves time axes by content hash",
        ))
    })
}

/// The 32-byte array hash a row names.
///
/// `data_hash` first, `uri` second: the schema treats the former as the
/// content hash and the latter as an opaque locator, so preferring the
/// declared hash is right even though this crate writes both the same.
fn resolve_hash(name: &str, data_hash: Option<&str>, uri: &str) -> Result<[u8; 32]> {
    if let Some(value) = data_hash {
        return crate::hash::hash_from_hex(value).ok_or_else(|| {
            wire_err(format!(
                "row '{name}': data_hash {value:?} is not a 64-character hex hash"
            ))
        });
    }
    crate::hash::hash_from_hex(uri).ok_or_else(|| {
        wire_err(format!(
            "row '{name}': neither data_hash nor uri names a stored array; uri {uri:?} is not \
             a hash, and this import resolves arrays by content hash",
        ))
    })
}

fn to_usize(value: i64, field: &str, name: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| wire_err(format!("row '{name}': {field} {value} is negative")))
}

fn dims_from_wire(dims: &[i64], field: &str, name: &str) -> Result<Vec<usize>> {
    dims.iter().map(|&d| to_usize(d, field, name)).collect()
}

fn ts_row_from_wire(row: TimeSeriesAssociation) -> Result<DecodedTsRow> {
    let (ts_type, shared, timing) = match row {
        TimeSeriesAssociation::SingleTimeSeries(r) => (
            TimeSeriesType::SingleTimeSeries,
            shared_of!(r),
            Timing {
                initial_timestamp: Some(r.initial_timestamp),
                resolution: Some(r.resolution),
                length: Some(r.length),
                interval_alignment: Some(r.interval_alignment),
                ..Timing::default()
            },
        ),
        TimeSeriesAssociation::NonSequentialTimeSeries(r) => (
            TimeSeriesType::NonSequentialTimeSeries,
            shared_of!(r),
            Timing {
                length: Some(r.length),
                timestamps_uri: r.timestamps_uri,
                ..Timing::default()
            },
        ),
        TimeSeriesAssociation::Deterministic(r) => (
            TimeSeriesType::Deterministic,
            shared_of!(r),
            Timing {
                initial_timestamp: Some(r.initial_timestamp),
                resolution: Some(r.resolution),
                horizon: Some(r.horizon),
                interval: Some(r.interval),
                count: Some(r.count),
                interval_alignment: Some(r.interval_alignment),
                ..Timing::default()
            },
        ),
        TimeSeriesAssociation::DeterministicSingleTimeSeries(r) => (
            TimeSeriesType::DeterministicSingleTimeSeries,
            shared_of!(r),
            Timing {
                initial_timestamp: Some(r.initial_timestamp),
                resolution: Some(r.resolution),
                horizon: Some(r.horizon),
                interval: Some(r.interval),
                count: Some(r.count),
                interval_alignment: Some(r.interval_alignment),
                ..Timing::default()
            },
        ),
        TimeSeriesAssociation::Probabilistic(r) => (
            TimeSeriesType::Probabilistic,
            shared_of!(r),
            Timing {
                initial_timestamp: Some(r.initial_timestamp),
                resolution: Some(r.resolution),
                horizon: Some(r.horizon),
                interval: Some(r.interval),
                count: Some(r.count),
                percentiles: Some(r.percentiles),
                interval_alignment: Some(r.interval_alignment),
                ..Timing::default()
            },
        ),
        // A `Scenarios` row spells its array length `scenario_count`.
        TimeSeriesAssociation::Scenarios(r) => (
            TimeSeriesType::Scenarios,
            shared_of!(r),
            Timing {
                initial_timestamp: Some(r.initial_timestamp),
                resolution: Some(r.resolution),
                horizon: Some(r.horizon),
                interval: Some(r.interval),
                count: Some(r.count),
                length: Some(r.scenario_count),
                interval_alignment: Some(r.interval_alignment),
                ..Timing::default()
            },
        ),
    };
    let name = shared.name;

    if let Some(a) = timing
        .interval_alignment
        .filter(|a| *a != wire::IntervalAlignment::PeriodEnding)
    {
        return Err(wire_err(format!(
            "row '{name}': interval_alignment {a} is not storable; the store records only the \
             default PERIOD_ENDING"
        )));
    }
    let data_hash = resolve_hash(&name, shared.data_hash.as_deref(), &shared.uri)?;
    let time_axis = match ts_type {
        TimeSeriesType::NonSequentialTimeSeries => {
            Some(resolve_time_axis(&name, timing.timestamps_uri.as_deref())?)
        }
        _ => None,
    };
    let element_type = crate::types::element_type::ElementType::parse(&shared.element_type)
        .ok_or_else(|| wire_err(format!("unknown element_type {:?}", shared.element_type)))?;
    let parse_optional_period =
        |value: Option<String>, field| value.map(|p| parse_period(&p, field)).transpose();
    let time_reference = shared
        .time_reference
        .map(|t| {
            TimeReference::parse(&t)
                .map_err(|e| wire_err(format!("row '{name}': time_reference: {e}")))
        })
        .transpose()?;
    // The native shape when the document carries it; the schema's own fields
    // otherwise (module docs).
    let (length, element_shape) = match shared.array_shape {
        Some(shape) => {
            let Some((&length, element_shape)) = shape.split_first() else {
                return Err(wire_err(format!(
                    "row '{name}': array_shape must have at least the time axis"
                )));
            };
            (
                Some(to_usize(length, "array_shape", &name)?),
                dims_from_wire(element_shape, "array_shape", &name)?,
            )
        }
        None => (
            timing
                .length
                .map(|l| to_usize(l, "length", &name))
                .transpose()?,
            dims_from_wire(&shared.element_shape, "element_shape", &name)?,
        ),
    };
    Ok(DecodedTsRow {
        time_axis,
        meta: TimeSeriesMetadata {
            owner_id: shared.owner_id,
            owner_type: shared.owner_type,
            owner_category: owner_category_from_wire(shared.owner_category),
            time_series_type: ts_type,
            data_hash,
            initial_timestamp: timing.initial_timestamp,
            resolution: parse_optional_period(timing.resolution, "resolution")?,
            length,
            horizon: parse_optional_period(timing.horizon, "horizon")?,
            interval: parse_optional_period(timing.interval, "interval")?,
            count: timing
                .count
                .map(|c| to_usize(c, "count", &name))
                .transpose()?,
            timestamps: None,
            features: features_from_wire(shared.features),
            units: shared.units,
            quantity_kind: shared.quantity_kind,
            unit_system: shared.unit_system.map(unit_system_from_wire),
            time_reference,
            component_field: shared.component_field,
            percentiles: timing.percentiles,
            element_type,
            element_shape,
            application_data: shared.application_data,
            id: Some(TimeSeriesId(shared.association_id)),
            name,
        },
    })
}

/// Parse a JSON array of OpenAPI time-series rows and insert them verbatim.
///
/// Every row is checked against the SiennaSchemas contract
/// ([`schema::check_time_series_row`]) *before* it is decoded, so a document
/// that drifted is reported in the schema's own terms — which field, on which
/// row — rather than as whatever the generated type happens to notice first.
/// The schema is also the stronger of the two: it knows the bounds and the
/// reserved feature names, where the type only knows names and Rust types.
///
/// Rows only: the values are not on the wire and are never reconstructed here.
/// See [`Store::import_association_rows`] for the invariants each row is
/// checked against before anything is written.
fn import_ts_rows(store: &mut Store, json: &str) -> Result<usize> {
    let decoded = decode_rows(json, schema::check_time_series_row, ts_row_from_wire)?;
    // The one field the document locates rather than carries. Resolved here
    // because this is the first point that has both the locator and the store,
    // and setting `timestamps` is what makes the ordinary insert path hash the
    // axis into `timestamps_hash` — so an imported irregular row is filed
    // exactly as an added one is.
    let mut rows = Vec::with_capacity(decoded.len());
    for DecodedTsRow {
        mut meta,
        time_axis,
    } in decoded
    {
        if let Some(axis) = time_axis {
            meta.timestamps = Some(store.timestamps_for(&axis).map_err(|_| {
                wire_err(format!(
                    "row '{}': timestamps_uri names time axis {}, which this store does not \
                     hold — the axis arrives with the artifact, like the arrays",
                    meta.name,
                    crate::hash::hash_hex(&axis),
                ))
            })?);
        }
        rows.push(meta);
    }
    store.import_association_rows(rows)
}

/// Validate each element of a JSON array against `check`, then decode it with
/// `convert`.
///
/// The two halves stay in this order for every table: the schema is the
/// published contract and the generated type is a weaker reading of it, so a
/// document is judged against the contract first.
fn decode_rows<Raw, Out>(
    json: &str,
    check: fn(&Value, usize) -> Result<()>,
    convert: fn(Raw) -> Result<Out>,
) -> Result<Vec<Out>>
where
    Raw: serde::de::DeserializeOwned,
{
    let values: Vec<Value> = serde_json::from_str(json)?;
    let mut out = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        check(&value, index)?;
        // Parsing to `Value` first loses serde_json's line/column, and the
        // schema check does not check formats, so a malformed timestamp reaches
        // this call as the first thing that rejects it. Without the index it
        // would name no row.
        let raw: Raw =
            serde_json::from_value(value).map_err(|e| wire_err(format!("row {index}: {e}")))?;
        out.push(convert(raw)?);
    }
    Ok(out)
}

// ============================================================================
// Supplemental-attribute associations
// ============================================================================

/// Export `supplemental_attribute_associations` as a JSON array sorted by
/// `(component_id, attribute_id)`.
///
/// The `id` is deliberately absent — nothing references an attachment — so ids
/// do not survive an export/import cycle here; the importer assigns fresh ones.
fn export_sa_rows(store: &Store) -> Result<String> {
    let mut rows =
        store.list_supplemental_attribute_associations(&SupplementalAttributeFilter::default())?;
    rows.sort_by_key(|row| (row.component_id, row.attribute_id));
    let wire: Vec<WireSaRow> = rows
        .into_iter()
        .map(|row| WireSaRow {
            component_id: row.component_id,
            component_type: row.component_type,
            attribute_id: row.attribute_id,
            attribute_type: row.attribute_type,
        })
        .collect();
    serde_json::to_string(&wire).map_err(Into::into)
}

/// Bulk-ingest a JSON array of SA-association rows through the existing
/// all-or-nothing insert path (one savepoint;
/// [`TimeSeriesError::DuplicateAssociation`] propagates and rolls the whole
/// batch back). Returns the number of rows inserted.
fn import_sa_rows(store: &mut Store, json: &str) -> Result<usize> {
    let associations: Vec<SupplementalAttributeAssociation> = decode_rows(
        json,
        schema::check_supplemental_attribute_row,
        |row: WireSaRow| {
            Ok(SupplementalAttributeAssociation {
                component_id: row.component_id,
                component_type: row.component_type,
                attribute_id: row.attribute_id,
                attribute_type: row.attribute_type,
                // The wire form carries no id, so an import is always assigned one.
                id: None,
            })
        },
    )?;
    // The public import surface reports a count, not ids: the wire form carries
    // no id, so the ones assigned here are this store's own and mean nothing to
    // the document that was imported.
    store
        .add_supplemental_attribute_associations(associations)
        .map(|ids| ids.len())
}

// ============================================================================
// `Store` public API
// ============================================================================

impl Store {
    /// Export `time_series_associations` matching `filter` as a JSON array of
    /// OpenAPI rows, each carrying `uri` and `data_hash` computed from the
    /// row's own content hash. See the module docs for the wire contract and
    /// sort order.
    ///
    /// The document holds only the six types the wire contract defines.
    /// `PersistentTimeSeries` is an infrastore-local extension with no schema
    /// to be spelled under, so its rows are omitted — and a `filter` naming
    /// that type is refused outright rather than answered with an empty array.
    /// Ask the catalog what an export leaves behind with
    /// `list_metadata(ListFilter::new().time_series_type(TimeSeriesType::PersistentTimeSeries))`.
    pub fn export_time_series_associations_openapi(&self, filter: &ListFilter) -> Result<String> {
        export_ts_rows(self, filter)
    }

    /// Ingest a JSON array of OpenAPI time-series rows in one all-or-nothing
    /// transaction, returning the number inserted. The import half of the round
    /// trip whose export is
    /// [`Self::export_time_series_associations_openapi`].
    ///
    /// Rows only. The document carries locators, never values, so every row
    /// must name an array this store already holds — the arrays arrive with
    /// the artifact. Each row keeps the `id` it carries, which is the point:
    /// an import that assigned fresh ids would leave every reference in the
    /// document pointing at the wrong series.
    ///
    /// See [`Self::import_association_rows`] for what is validated, including
    /// the time axis an irregular row has to name. A `PersistentTimeSeries`
    /// row is refused before any of that, by the schema check: no export
    /// writes one, and the wire contract has no schema to check one against.
    pub fn import_time_series_associations_openapi(&mut self, json: &str) -> Result<usize> {
        import_ts_rows(self, json)
    }

    /// Export the whole `supplemental_attribute_associations` table as a JSON
    /// array, sorted by `(component_id, attribute_id)`.
    pub fn export_supplemental_attribute_associations_openapi(&self) -> Result<String> {
        export_sa_rows(self)
    }

    /// Bulk-ingest a JSON array of supplemental-attribute association rows in
    /// one all-or-nothing transaction, returning the number inserted. This is
    /// the import half of the round trip whose export is
    /// [`Self::export_supplemental_attribute_associations_openapi`].
    pub fn import_supplemental_attribute_associations_openapi(
        &mut self,
        json: &str,
    ) -> Result<usize> {
        import_sa_rows(self, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts_row_json(extra: &str) -> String {
        format!(
            r#"[
              {{
                "association_id": 1,
                "owner_id": 7,
                "owner_type": "ThermalStandard",
                "owner_category": "Component",
                "time_series_type": "SingleTimeSeries",
                "name": "max_active_power",
                "features": {{}},
                "uri": "6044a282e6ea37af7982ca551b8d5dca932fe4fb696bf9cfa14e7316e2693286",
                "element_type": "f64",
                "element_shape": [],
                "initial_timestamp": "2030-01-01T00:00:00Z",
                "resolution": "PT1H",
                "length": 24
              }},
              {{
                "association_id": 2,
                "owner_id": 8,
                "owner_type": "ThermalStandard",
                "owner_category": "Component",
                "time_series_type": "SingleTimeSeries",
                "name": "min_active_power",
                "features": {{}},
                "uri": "b1946ac92492d2347c6235b4d2611184a1b8a2e5f0b6d5ba9b7b8c4d3e2f1a09",
                "element_type": "f64",
                "element_shape": [],
                "initial_timestamp": "2030-01-01T00:00:00Z",
                "resolution": "PT1H",
                "length": 24{extra}
              }}
            ]"#
        )
    }

    #[test]
    fn an_unknown_field_names_the_row_it_is_in() {
        let json = ts_row_json(",\n                \"owner_typ\": \"Typo\"");
        let Err(err) = decode_rows(&json, schema::check_time_series_row, ts_row_from_wire) else {
            panic!("the closed row schema rejects the typo");
        };
        let message = err.to_string();
        assert!(message.contains("row 1"), "{message}");
        assert!(message.contains("owner_typ"), "{message}");
    }

    #[test]
    fn a_clean_document_decodes_every_row() {
        let Ok(rows) = decode_rows(
            &ts_row_json(""),
            schema::check_time_series_row,
            ts_row_from_wire,
        ) else {
            panic!("both rows conform");
        };
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn interval_alignment_decodes_only_as_the_default() {
        let decode = |alignment: &str| {
            decode_rows(
                &ts_row_json(&format!(",\n\"interval_alignment\": \"{alignment}\"")),
                schema::check_time_series_row,
                ts_row_from_wire,
            )
        };
        assert!(decode("PERIOD_ENDING").is_ok());
        let Err(err) = decode("PERIOD_BEGINNING") else {
            panic!("a non-default alignment has no catalog column");
        };
        let message = err.to_string();
        assert!(message.contains("min_active_power"), "{message}");
        assert!(message.contains("PERIOD_BEGINNING"), "{message}");
    }
}
