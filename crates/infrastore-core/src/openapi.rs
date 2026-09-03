//! OpenAPI-row JSON serde for the two association catalogs.
//!
//! `time_series_associations` and `supplemental_attribute_associations` are
//! the store's own tables; this module is the only place that knows how a row
//! of either one maps to and from the wire spelling SiennaSchemas defines
//! (`TimeSeries/*.json`, `Core/Associations/SupplementalAttributeAssociation.json`,
//! vendored at `crates/infrastore-core/sienna_schemas/`). Everything else in the crate —
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
//! Three fields reached the schema after the rest, and the vendored copy
//! carries them from an **un-merged** SiennaSchemas branch (`timestamps_uri`,
//! below, is the third) — see
//! `crates/infrastore-core/sienna_schemas/SOURCE.md` for which commit: `time_reference`,
//! the catalog spelling of [`TimeReference`] (`"utc"`, `"zoneless"`, an offset,
//! or a zone name), and `array_shape`, the stored array's full native shape —
//! `[length, *element_shape]` in the catalog's own terms, where the schema's
//! `element_shape` is only the per-step trailing shape (see
//! [`wire_element_shape`]). Both exist so that a row imported from a document
//! is *identical* to the row that was exported: the forecast layouts are
//! conventions the caller owns, not rules the store enforces, so the native
//! shape cannot be reconstructed from `horizon`/`count`/`percentiles` and has
//! to travel. Neither is required, and the schemas do not close their objects,
//! so a producer predating them writes rows without them and a reader that does
//! not know them ignores them; an import that finds `array_shape` absent falls
//! back to the schema fields, which is exact for the static types and a best
//! effort for forecasts.
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
//! its own geometry fields — see [`ts_row_to_json`] — and every field that
//! does not apply to a row's type is absent from that row's object, never
//! present as `null`.
//!
//! `initial_timestamp` is RFC3339 UTC, floored to millisecond precision (see
//! [`format_initial_timestamp`]) — deliberately less precise than the general
//! store contract, which keeps nanoseconds
//! (`crate::types::period` module docs); the OpenAPI wire form only promises
//! milliseconds. `resolution` / `horizon` / `interval` are
//! [`Period::to_iso8601`]'s canonical spelling, which is not always the
//! "seconds" form a hand-written fixture might guess (`PT3600S` canonicalizes
//! to `PT1H`).
//!
//! # Export sort order
//!
//! [`export_ts_rows`] sorts the array by the identity tuple `(owner_id,
//! owner_category code, time_series_type code, name, resolution, interval,
//! features)`, compared as **typed** values — see [`SortKey`] for why that
//! matters and how periods and features participate in a total order despite
//! not being numeric.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::Result;
use crate::metadata::{SupplementalAttributeAssociation, SupplementalAttributeFilter};
use crate::store::{ListFilter, Store};
use crate::types::id::TimeSeriesId;
use crate::types::metadata::{FeatureValue, Features, TimeSeriesMetadata, UnitSystem};
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
/// store's internally-tagged [`FeatureValue`] form.
fn features_to_plain(features: &Features) -> Map<String, Value> {
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

/// `NATURAL_UNITS` / `COMPONENT_BASE`, the schema's spelling — the module maps
/// to and from the store's snake_case [`UnitSystem::as_str`].
fn unit_system_wire(system: UnitSystem) -> &'static str {
    match system {
        UnitSystem::NaturalUnits => "NATURAL_UNITS",
        UnitSystem::ComponentBase => "COMPONENT_BASE",
    }
}

/// RFC3339 UTC, floored to millisecond precision: any finer component the
/// catalog happens to hold is dropped rather than surfaced. A whole-second
/// timestamp renders with no fractional part at all (`"...T00:00:00Z"`), and
/// anything with a nonzero millisecond remainder renders with exactly three
/// fractional digits — never nanoseconds, which is what softens the schema's
/// "keeps nanoseconds" wording.
fn format_initial_timestamp(ts: DateTime<Utc>) -> String {
    let millis = ts.timestamp_millis();
    let floored = DateTime::<Utc>::from_timestamp_millis(millis)
        .expect("a timestamp's own millisecond count is always representable");
    let format = if millis.rem_euclid(1_000) == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Millis
    };
    floored.to_rfc3339_opts(format, true)
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

/// Insert the fields every forecast type shares
/// (`resolution`/`horizon`/`interval`/`count`); the caller adds the type's own
/// extras (`percentiles`, `scenario_count`) on top.
fn insert_forecast_fields(row: &mut Map<String, Value>, meta: &TimeSeriesMetadata) {
    if let Some(r) = meta.resolution {
        row.insert("resolution".into(), Value::from(r.to_iso8601()));
    }
    if let Some(h) = meta.horizon {
        row.insert("horizon".into(), Value::from(h.to_iso8601()));
    }
    if let Some(i) = meta.interval {
        row.insert("interval".into(), Value::from(i.to_iso8601()));
    }
    if let Some(c) = meta.count {
        row.insert("count".into(), Value::from(c as u64));
    }
}

/// Map one catalog row to its OpenAPI wire object. `uri` and `data_hash` are
/// both derived from the row's own `data_hash` via [`crate::hash::hash_hex`]
/// — infrastore never accepts a caller-supplied locator for its own rows.
fn ts_row_to_json(meta: &TimeSeriesMetadata) -> Value {
    let mut row = Map::new();
    row.insert("owner_id".into(), Value::from(meta.owner_id));
    row.insert("owner_type".into(), Value::from(meta.owner_type.clone()));
    row.insert(
        "owner_category".into(),
        Value::from(meta.owner_category.as_str()),
    );
    row.insert(
        "time_series_type".into(),
        Value::from(meta.time_series_type.as_str()),
    );
    row.insert("name".into(), Value::from(meta.name.clone()));
    row.insert(
        "features".into(),
        Value::Object(features_to_plain(&meta.features)),
    );
    let hash_hex = crate::hash::hash_hex(&meta.data_hash);
    row.insert("uri".into(), Value::from(hash_hex.clone()));
    row.insert("data_hash".into(), Value::from(hash_hex));
    // Required by the schema, so a row without one is a row that never came
    // from a catalog — which nothing exports.
    if let Some(id) = meta.id {
        row.insert("association_id".into(), Value::from(id.get()));
    }
    row.insert(
        "element_type".into(),
        Value::from(meta.element_type.to_string()),
    );
    row.insert(
        "element_shape".into(),
        Value::from(wire_element_shape(meta).to_vec()),
    );
    // The native shape, so the import reproduces the catalog row exactly
    // (module docs). `length` is the array's first axis for every type.
    if let Some(length) = meta.length {
        let mut array_shape = Vec::with_capacity(meta.element_shape.len() + 1);
        array_shape.push(length);
        array_shape.extend_from_slice(&meta.element_shape);
        row.insert("array_shape".into(), Value::from(array_shape));
    }
    if let Some(reference) = &meta.time_reference {
        row.insert(
            "time_reference".into(),
            Value::from(reference.as_storage_string()),
        );
    }

    if let Some(units) = &meta.units {
        row.insert("units".into(), Value::from(units.clone()));
    }
    if let Some(quantity_kind) = &meta.quantity_kind {
        row.insert("quantity_kind".into(), Value::from(quantity_kind.clone()));
    }
    if let Some(component_field) = &meta.component_field {
        row.insert(
            "component_field".into(),
            Value::from(component_field.clone()),
        );
    }
    if let Some(application_data) = &meta.application_data {
        row.insert(
            "application_data".into(),
            Value::from(application_data.clone()),
        );
    }
    if let Some(unit_system) = meta.unit_system {
        row.insert(
            "unit_system".into(),
            Value::from(unit_system_wire(unit_system)),
        );
    }
    if let Some(ts) = meta.initial_timestamp {
        row.insert(
            "initial_timestamp".into(),
            Value::from(format_initial_timestamp(ts)),
        );
    }

    match meta.time_series_type {
        TimeSeriesType::SingleTimeSeries => {
            if let Some(r) = meta.resolution {
                row.insert("resolution".into(), Value::from(r.to_iso8601()));
            }
            if let Some(l) = meta.length {
                row.insert("length".into(), Value::from(l as u64));
            }
        }
        TimeSeriesType::NonSequentialTimeSeries => {
            if let Some(l) = meta.length {
                row.insert("length".into(), Value::from(l as u64));
            }
            // The axis locator, filled from the axis's own content hash exactly
            // as `uri` is filled from the array's. Absent only for a row that
            // reached here without its axis, which `export_ts_rows`'s listing
            // always supplies.
            if let Some(timestamps) = &meta.timestamps {
                row.insert(
                    "timestamps_uri".into(),
                    Value::from(crate::hash::hash_hex(&crate::hash::timestamps_hash(
                        timestamps,
                    ))),
                );
            }
        }
        TimeSeriesType::Deterministic | TimeSeriesType::DeterministicSingleTimeSeries => {
            insert_forecast_fields(&mut row, meta);
        }
        TimeSeriesType::Probabilistic => {
            insert_forecast_fields(&mut row, meta);
            if let Some(percentiles) = &meta.percentiles {
                row.insert("percentiles".into(), Value::from(percentiles.clone()));
            }
        }
        TimeSeriesType::Scenarios => {
            insert_forecast_fields(&mut row, meta);
            if let Some(l) = meta.length {
                row.insert("scenario_count".into(), Value::from(l as u64));
            }
        }
    }

    Value::Object(row)
}

/// Export `time_series_associations` as a sorted OpenAPI-row JSON array. Pure
/// mapping over rows [`Store::list_metadata`] already produced — see the
/// module docs for the wire contract and sort order.
fn export_ts_rows(store: &Store, filter: &ListFilter) -> Result<String> {
    let rows = store.list_with_timestamps(filter.clone())?;
    let mut keyed: Vec<(SortKey, Value)> = rows
        .iter()
        .map(|meta| (sort_key(meta), ts_row_to_json(meta)))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let array: Vec<Value> = keyed.into_iter().map(|(_, row)| row).collect();
    serde_json::to_string(&array).map_err(Into::into)
}

/// One incoming time-series row. Every field the wire form can carry, with the
/// per-type ones optional, and unknown keys denied so a typo fails loudly
/// rather than vanishing — the same contract [`RawSaRow`] holds.
///
/// `uri` and `data_hash` are both accepted: they are the same hex string on
/// anything this crate exported, but the schema makes `data_hash` optional and
/// specifies `uri` only as a locator with no required format, so a document
/// from another producer may carry a `uri` that is not a hash.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTsRow {
    owner_id: i64,
    owner_type: String,
    owner_category: String,
    time_series_type: String,
    name: String,
    features: Map<String, Value>,
    uri: String,
    /// The axis locator a `NonSequentialTimeSeries` carries; absent on every
    /// other type, which has no explicit axis.
    #[serde(default)]
    timestamps_uri: Option<String>,
    #[serde(default)]
    data_hash: Option<String>,
    element_type: String,
    element_shape: Vec<usize>,
    /// The native `[length, *element_shape]` (module docs); absent in a
    /// document from a producer that predates it.
    #[serde(default)]
    array_shape: Option<Vec<usize>>,
    #[serde(default)]
    time_reference: Option<String>,
    /// The store's `id`, under the schema's spelling.
    #[serde(default)]
    association_id: Option<i64>,
    #[serde(default)]
    units: Option<String>,
    #[serde(default)]
    quantity_kind: Option<String>,
    #[serde(default)]
    unit_system: Option<String>,
    #[serde(default)]
    component_field: Option<String>,
    #[serde(default)]
    application_data: Option<String>,
    #[serde(default)]
    initial_timestamp: Option<String>,
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    length: Option<usize>,
    #[serde(default)]
    horizon: Option<String>,
    #[serde(default)]
    interval: Option<String>,
    #[serde(default)]
    count: Option<usize>,
    #[serde(default)]
    percentiles: Option<Vec<f64>>,
    #[serde(default)]
    scenario_count: Option<usize>,
}

fn wire_err(msg: impl Into<String>) -> crate::error::TimeSeriesError {
    crate::error::TimeSeriesError::InvalidParameter(msg.into())
}

/// A 64-character hex string as 32 bytes, or `None` for anything else —
/// including a `uri` that is a locator rather than a hash. The storage
/// layer's parser, with its error (which names a dataset, not a document)
/// folded to `None`.
fn hash_from_hex(s: &str) -> Option<[u8; 32]> {
    crate::storage::common::hex_to_hash(s).ok()
}

fn parse_period(s: &str, field: &str) -> Result<crate::types::period::Period> {
    crate::types::period::Period::from_iso8601(s)
        .map_err(|e| wire_err(format!("{field} {s:?} is not an ISO-8601 duration: {e}")))
}

/// The inverse of [`unit_system_wire`]. The schema's spelling is SCREAMING_CASE
/// and the store's is snake_case, so this is not `UnitSystem::parse`.
fn unit_system_from_wire(s: &str) -> Result<UnitSystem> {
    match s {
        "NATURAL_UNITS" => Ok(UnitSystem::NaturalUnits),
        "COMPONENT_BASE" => Ok(UnitSystem::ComponentBase),
        other => Err(wire_err(format!(
            "unknown unit_system {other:?}; expected NATURAL_UNITS or COMPONENT_BASE"
        ))),
    }
}

/// The inverse of [`features_to_plain`]: a plain scalar map back into the
/// store's tagged feature values.
fn features_from_plain(map: &Map<String, Value>) -> Result<Features> {
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
/// turn a locator into a vector, and [`RawTsRow::into_metadata`] has no store.
/// `None` for every type but `NonSequentialTimeSeries`, which is the only one
/// with an explicit axis.
struct DecodedTsRow {
    meta: TimeSeriesMetadata,
    time_axis: Option<[u8; 32]>,
}

impl RawTsRow {
    /// The 32-byte hash of the time axis this row names, for the one type that
    /// has one.
    ///
    /// Required there, and required to be a hash: unlike `uri` — which the
    /// schema leaves as a free-form locator, and whose value this crate can
    /// fall back to `data_hash` for — there is no second field carrying the
    /// axis. A locator this store cannot resolve is a row it cannot file.
    fn resolve_time_axis(&self, ts_type: TimeSeriesType) -> Result<Option<[u8; 32]>> {
        if ts_type != TimeSeriesType::NonSequentialTimeSeries {
            return Ok(None);
        }
        let Some(locator) = self.timestamps_uri.as_deref() else {
            return Err(wire_err(format!(
                "row '{}': a NonSequentialTimeSeries carries its time axis as timestamps_uri, \
                 and this row has none — without it the document does not say which of the \
                 store's axes the row sits on",
                self.name,
            )));
        };
        hash_from_hex(locator).map(Some).ok_or_else(|| {
            wire_err(format!(
                "row '{}': timestamps_uri {locator:?} is not a 64-character hex hash, and this \
                 import resolves time axes by content hash",
                self.name,
            ))
        })
    }

    /// The 32-byte array hash this row names.
    ///
    /// `data_hash` first, `uri` second: the schema treats the former as the
    /// content hash and the latter as an opaque locator, so preferring the
    /// declared hash is right even though this crate writes both the same.
    fn resolve_hash(&self) -> Result<[u8; 32]> {
        for (field, value) in [
            ("data_hash", self.data_hash.as_deref()),
            ("uri", Some(&*self.uri)),
        ] {
            let Some(value) = value else { continue };
            if let Some(hash) = hash_from_hex(value) {
                return Ok(hash);
            }
            if field == "data_hash" {
                return Err(wire_err(format!(
                    "row '{}': data_hash {value:?} is not a 64-character hex hash",
                    self.name
                )));
            }
        }
        Err(wire_err(format!(
            "row '{}': neither data_hash nor uri names a stored array; uri {:?} is not a hash, \
             and this import resolves arrays by content hash",
            self.name, self.uri,
        )))
    }

    fn into_metadata(self) -> Result<DecodedTsRow> {
        let data_hash = self.resolve_hash()?;
        let ts_type = TimeSeriesType::parse(&self.time_series_type).ok_or_else(|| {
            wire_err(format!(
                "unknown time_series_type {:?}",
                self.time_series_type
            ))
        })?;
        let owner_category = crate::types::metadata::OwnerCategory::parse(&self.owner_category)
            .ok_or_else(|| wire_err(format!("unknown owner_category {:?}", self.owner_category)))?;
        let element_type = crate::types::element_type::ElementType::parse(&self.element_type)
            .ok_or_else(|| wire_err(format!("unknown element_type {:?}", self.element_type)))?;
        let initial_timestamp = match &self.initial_timestamp {
            Some(t) => Some(
                DateTime::parse_from_rfc3339(t)
                    .map_err(|e| wire_err(format!("initial_timestamp {t:?}: {e}")))?
                    .with_timezone(&Utc),
            ),
            None => None,
        };
        let unit_system = match &self.unit_system {
            Some(u) => Some(unit_system_from_wire(u)?),
            None => None,
        };
        let resolution = match &self.resolution {
            Some(r) => Some(parse_period(r, "resolution")?),
            None => None,
        };
        let horizon = match &self.horizon {
            Some(h) => Some(parse_period(h, "horizon")?),
            None => None,
        };
        let interval = match &self.interval {
            Some(i) => Some(parse_period(i, "interval")?),
            None => None,
        };
        let time_reference = match &self.time_reference {
            Some(t) => Some(
                TimeReference::parse(t)
                    .map_err(|e| wire_err(format!("row '{}': time_reference: {e}", self.name)))?,
            ),
            None => None,
        };
        let time_axis = self.resolve_time_axis(ts_type)?;
        // The native shape when the document carries it; the schema's own
        // fields otherwise (module docs).
        let (length, element_shape) = match self.array_shape {
            Some(shape) => {
                let Some((&length, element_shape)) = shape.split_first() else {
                    return Err(wire_err(format!(
                        "row '{}': array_shape must have at least the time axis",
                        self.name
                    )));
                };
                (Some(length), element_shape.to_vec())
            }
            // A `Scenarios` row spells its array length `scenario_count`.
            None => (self.length.or(self.scenario_count), self.element_shape),
        };
        Ok(DecodedTsRow {
            time_axis,
            meta: TimeSeriesMetadata {
                owner_id: self.owner_id,
                owner_type: self.owner_type,
                owner_category,
                time_series_type: ts_type,
                name: self.name,
                data_hash,
                initial_timestamp,
                resolution,
                length,
                horizon,
                interval,
                count: self.count,
                timestamps: None,
                features: features_from_plain(&self.features)?,
                units: self.units,
                quantity_kind: self.quantity_kind,
                unit_system,
                time_reference,
                component_field: self.component_field,
                percentiles: self.percentiles,
                element_type,
                element_shape,
                application_data: self.application_data,
                id: self.association_id.map(TimeSeriesId),
            },
        })
    }
}

/// Parse a JSON array of OpenAPI time-series rows and insert them verbatim.
///
/// Every row is checked against the vendored SiennaSchemas contract
/// ([`schema::check_time_series_row`]) *before* it is decoded, so a document
/// that drifted is reported in the schema's own terms — which field, on which
/// row — rather than as whatever `RawTsRow` happens to notice first. The schema
/// is also the stronger of the two: it knows the `oneOf` over the six types and
/// the closed enums, where the struct only knows names and Rust types.
///
/// Rows only: the values are not on the wire and are never reconstructed here.
/// See [`Store::import_association_rows`] for the invariants each row is
/// checked against before anything is written.
fn import_ts_rows(store: &mut Store, json: &str) -> Result<usize> {
    let decoded = decode_rows(json, schema::check_time_series_row, RawTsRow::into_metadata)?;
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
/// published contract and the `Raw*` struct is this crate's reading of it, so a
/// document is judged against the contract first, and the struct never gets to
/// reject something the contract allows without saying so.
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
        out.push(convert(serde_json::from_value(value)?)?);
    }
    Ok(out)
}

// ============================================================================
// Supplemental-attribute associations
// ============================================================================

/// Export `supplemental_attribute_associations` as a JSON array sorted by
/// `(component_id, attribute_id)`. The row struct's own `#[derive(Serialize)]`
/// already produces exactly the wire shape
/// (`SupplementalAttributeAssociation.json`), so this is a listing plus a
/// sort, nothing more.
fn export_sa_rows(store: &Store) -> Result<String> {
    let mut rows =
        store.list_supplemental_attribute_associations(&SupplementalAttributeFilter::default())?;
    rows.sort_by_key(|row| (row.component_id, row.attribute_id));
    let wire: Vec<SaWireRow> = rows.iter().map(SaWireRow::from).collect();
    serde_json::to_string(&wire).map_err(Into::into)
}

/// The four fields the schema defines, named explicitly rather than serialized
/// off [`SupplementalAttributeAssociation`] directly.
///
/// Naming them explicitly keeps a field added to the struct from silently
/// becoming a wire change. The `id` is deliberately absent — nothing
/// references an attachment — so ids do not survive an export/import cycle
/// here; the importer assigns fresh ones.
#[derive(Debug, Serialize)]
struct SaWireRow<'a> {
    component_id: i64,
    component_type: &'a str,
    attribute_id: i64,
    attribute_type: &'a str,
}

impl<'a> From<&'a SupplementalAttributeAssociation> for SaWireRow<'a> {
    fn from(row: &'a SupplementalAttributeAssociation) -> Self {
        Self {
            component_id: row.component_id,
            component_type: &row.component_type,
            attribute_id: row.attribute_id,
            attribute_type: &row.attribute_type,
        }
    }
}

/// One row of the incoming SA-association JSON, denying unknown fields so a
/// typo'd column name fails loudly rather than silently vanishing. A straight
/// bulk insert: the row shape already matches
/// [`SupplementalAttributeAssociation`] exactly, with no `uri` field to
/// tolerate.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSaRow {
    component_id: i64,
    component_type: String,
    attribute_id: i64,
    attribute_type: String,
}

impl From<RawSaRow> for SupplementalAttributeAssociation {
    fn from(row: RawSaRow) -> Self {
        SupplementalAttributeAssociation {
            component_id: row.component_id,
            component_type: row.component_type,
            attribute_id: row.attribute_id,
            attribute_type: row.attribute_type,
            // The wire form carries no id, so an import is always assigned one.
            id: None,
        }
    }
}

/// Bulk-ingest a JSON array of SA-association rows through the existing
/// all-or-nothing insert path (one savepoint;
/// [`TimeSeriesError::DuplicateAssociation`] propagates and rolls the whole
/// batch back). Returns the number of rows inserted.
fn import_sa_rows(store: &mut Store, json: &str) -> Result<usize> {
    let associations: Vec<SupplementalAttributeAssociation> = decode_rows(
        json,
        schema::check_supplemental_attribute_row,
        |raw: RawSaRow| Ok(raw.into()),
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
    /// why a `NonSequentialTimeSeries` row cannot be imported.
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
