//! OpenAPI-row JSON serde for the two association catalogs.
//!
//! `time_series_associations` and `supplemental_attribute_associations` are
//! the store's own tables; this module is the only place that knows how a row
//! of either one maps to and from the wire spelling SiennaSchemas defines
//! (`TimeSeries/*.json`, `Core/Associations/SupplementalAttributeAssociation.json`,
//! vendored at `conformance/sienna_schemas/`). Everything else in the crate —
//! the catalog schema, the metadata query, the association tables — is
//! untouched by this module; it only maps rows [`crate::store::Store`]
//! already produces into JSON, and JSON back into the calls that already
//! exist for writing.
//!
//! # Wire contract (design doc D3)
//!
//! A time-series association row carries the fields every type shares —
//! `id` (the catalog rowid), `owner_id`, `owner_type`, `owner_category`,
//! `time_series_type`, `name`, `features` (a *plain* scalar map — int, float,
//! bool, or string values, never the store's internally-tagged
//! [`crate::types::metadata::FeatureValue`] form), `address` (the caller's
//! string, stamped verbatim and never interpreted), `element_type`,
//! `element_shape` — plus optional descriptive fields (`units`,
//! `quantity_kind`, `unit_system`, `component_field`, `application_data`)
//! that are *omitted* from the JSON object when unset, never written as
//! `null`. `unit_system` maps the store's snake_case internal spelling to the
//! schema's `NATURAL_UNITS` / `COMPONENT_BASE`; omitted (not merely absent)
//! means *unspecified*, which is a different thing from natural units, so the
//! two must stay distinguishable through the round trip.
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
//! to `PT1H`) — see the fixture correction note below.
//!
//! # Export sort order
//!
//! [`export_ts_rows`] sorts the array by the identity tuple `(owner_id,
//! owner_category code, time_series_type code, name, resolution, interval,
//! features)`, compared as **typed** values — see [`SortKey`] for why that
//! matters and how periods and features participate in a total order despite
//! not being numeric.
//!
//! # Reconcile (design doc D4)
//!
//! A `data_hash` is `NOT NULL` in the catalog and names dense arrays the
//! schemas deliberately never carry, so a JSON document can never *create* a
//! complete catalog row. [`reconcile_ts_rows`] therefore reconciles JSON rows
//! against the store's existing catalog, matched by the identity tuple above.
//! See [`ReconcilePolicy`] and [`ReconcileReport`] for the exact policy
//! matrix; [`TimeSeriesError::ReconcileConflict`] is the failure mode for
//! every case the policy cannot resolve.
//!
//! # Fixture correction
//!
//! The checked-in fixtures at `conformance/openapi_row_fixtures/*.json` were
//! originally hand-written with seconds-canonical durations (`PT3600S`,
//! `PT86400S`), matching a Julia binding's emitter. [`Period::to_iso8601`]
//! canonicalizes differently — `PT3600S` → `PT1H`, `PT86400S` → `P1D`,
//! `PT900S` → `PT15M`, `PT7200S` → `PT2H`, `PT14400S` → `PT4H` — so the
//! fixtures were corrected to Rust's spelling rather than the module bending
//! to match them; any ISO-8601 string remains schema-valid either way.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Result, TimeSeriesError};
use crate::metadata::{SupplementalAttributeAssociation, SupplementalAttributeFilter};
use crate::store::{DescriptiveUpdate, ListFilter, Store};
use crate::types::element_type::ElementType;
use crate::types::metadata::{
    FeatureValue, Features, OwnerCategory, TimeSeriesMetadata, UnitSystem,
};
use crate::types::period::Period;
use crate::types::time_series::TimeSeriesType;

// ============================================================================
// Time-series associations: export
// ============================================================================

/// The identity tuple export sorts by (D3): `(owner_id, owner_category code,
/// time_series_type code, name, resolution, interval, features)`, compared as
/// **typed** values rather than their string spellings.
///
/// This is deliberate, not cosmetic: the catalog's own hashing philosophy
/// (`crate::hash::features_hash` digests a [`FeatureValue`] by its bits, never
/// by a stringified rendering) extends to the export order. Comparing
/// `owner_id` as a string would sort `10` before `2`; comparing a
/// [`FeatureValue::Int`] `1` against a [`FeatureValue::Str`] `"1"` by their
/// JSON spelling would collide two values the catalog treats as distinct —
/// exactly the `1` vs `"1"` stringification collision `IS.openapi_row_sort_key`
/// used to risk.
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
/// store's internally-tagged [`FeatureValue`] form. The inverse of
/// [`plain_to_features`].
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

/// Parse a plain scalar feature map back into [`Features`]. A JSON integer
/// literal (no decimal point or exponent) becomes [`FeatureValue::Int`]; one
/// with a fractional part becomes [`FeatureValue::Float`] — the same
/// distinction `serde_json::Number::as_i64` / `as_f64` already draws from the
/// literal's own spelling, so this never has to guess. Any other JSON shape
/// (array, object, null) is rejected: the wire contract is scalars only.
fn plain_to_features(map: &Map<String, Value>) -> Result<Features> {
    let mut out = Features::new();
    for (key, value) in map {
        let parsed = match value {
            Value::Bool(b) => FeatureValue::Bool(*b),
            Value::String(s) => FeatureValue::Str(s.clone()),
            Value::Number(n) => match n.as_i64() {
                Some(i) => FeatureValue::Int(i),
                None => n.as_f64().map(FeatureValue::Float).ok_or_else(|| {
                    TimeSeriesError::InvalidParameter(format!(
                        "feature {key:?} has a number the store cannot represent: {n}"
                    ))
                })?,
            },
            other => {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "feature {key:?} must be an int, float, bool, or string; got {other}"
                )));
            }
        };
        out.insert(key.clone(), parsed);
    }
    Ok(out)
}

/// `NATURAL_UNITS` / `COMPONENT_BASE`, the schema's spelling — the module maps
/// to and from the store's snake_case [`UnitSystem::as_str`].
fn unit_system_wire(system: UnitSystem) -> &'static str {
    match system {
        UnitSystem::NaturalUnits => "NATURAL_UNITS",
        UnitSystem::ComponentBase => "COMPONENT_BASE",
    }
}

fn parse_unit_system_wire(s: &str) -> Result<UnitSystem> {
    match s {
        "NATURAL_UNITS" => Ok(UnitSystem::NaturalUnits),
        "COMPONENT_BASE" => Ok(UnitSystem::ComponentBase),
        other => Err(TimeSeriesError::InvalidParameter(format!(
            "unknown unit_system {other:?}; expected NATURAL_UNITS or COMPONENT_BASE"
        ))),
    }
}

/// RFC3339 UTC, floored to millisecond precision: any finer component the
/// catalog happens to hold is dropped rather than surfaced. A whole-second
/// timestamp renders with no fractional part at all (`"...T00:00:00Z"`), and
/// anything with a nonzero millisecond remainder renders with exactly three
/// fractional digits — never nanoseconds, which is what softens the schema's
/// "keeps nanoseconds" wording (D3).
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

/// The true per-step trailing shape (D3's `element_shape`), derived from the
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

/// Map one catalog row to its OpenAPI wire object (D3). `id` is the catalog
/// rowid ([`Store::list_time_series_with_id`]); `address` is the caller's
/// string, stamped verbatim and never interpreted by the store.
fn ts_row_to_json(id: i64, address: &str, meta: &TimeSeriesMetadata) -> Value {
    let mut row = Map::new();
    row.insert("id".into(), Value::from(id));
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
    row.insert("address".into(), Value::from(address));
    row.insert(
        "element_type".into(),
        Value::from(meta.element_type.to_string()),
    );
    row.insert(
        "element_shape".into(),
        Value::from(wire_element_shape(meta).to_vec()),
    );

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
/// mapping over rows [`Store::list_time_series_with_id`] already produced —
/// see the module docs for the wire contract and sort order.
fn export_ts_rows(store: &Store, address: &str, filter: &ListFilter) -> Result<String> {
    let rows = store.list_time_series_with_id(filter.clone())?;
    let mut keyed: Vec<(SortKey, Value)> = rows
        .iter()
        .map(|(id, meta)| (sort_key(meta), ts_row_to_json(*id, address, meta)))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let array: Vec<Value> = keyed.into_iter().map(|(_, row)| row).collect();
    serde_json::to_string(&array).map_err(Into::into)
}

// ============================================================================
// Time-series associations: reconcile (D4)
// ============================================================================

/// How [`reconcile_ts_rows`] treats a matched row whose descriptive columns
/// (`units`, `quantity_kind`, `unit_system`, `component_field`,
/// `application_data`) differ between the JSON document and the catalog.
///
/// Geometry drift (`initial_timestamp`, `length`, `horizon`, `interval`,
/// `count`, `element_type`, `element_shape`, `percentiles`) and a JSON row
/// with no catalog match are hard errors under **either** policy: geometry
/// describes the arrays the catalog actually holds, which a document can
/// never override, and a JSON row naming a series the catalog does not hold
/// means the document claims data the store cannot back up (`data_hash` is
/// `NOT NULL` and the schemas never carry it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReconcilePolicy {
    /// Any drift at all — descriptive or geometric — is a hard error.
    #[default]
    Strict,
    /// Descriptive drift is resolved by letting the JSON document win for
    /// those five columns; geometry drift is still a hard error.
    UpdateDescriptive,
}

/// Outcome of a successful [`reconcile_ts_rows`] call.
///
/// A failed call returns [`TimeSeriesError::ReconcileConflict`] instead of
/// this: every drift or mismatch the requested policy cannot resolve aborts
/// the *whole* call (naming every offender in one message), so this report is
/// only meaningful once every JSON row has been accounted for cleanly.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ReconcileReport {
    /// JSON rows that matched a catalog row by identity, whether left
    /// unchanged or rewritten.
    pub matched: usize,
    /// Of the matched rows, how many had a descriptive column rewritten.
    /// Always 0 under [`ReconcilePolicy::Strict`]: descriptive drift there is
    /// a hard error, never a silent update.
    pub updated: usize,
    /// Always 0 on a successful reconcile. A JSON row naming a series the
    /// catalog does not hold is always fatal (see [`ReconcilePolicy`]'s
    /// docs), so this count never survives into a returned `Ok`; the field
    /// exists because the failure path names the same count in its error
    /// message.
    pub missing_in_store: usize,
    /// Catalog rows that no JSON row referenced. Tolerated under both
    /// policies: an export dumps the whole catalog, but a document — PTDP's
    /// staged output, a hand-augmented file — only ever carries the owners it
    /// contains, so a nonzero count here is the expected shape of a partial
    /// document, not a problem.
    pub unmatched_in_store: usize,
    /// Human-readable notes on matched rows that needed attention: under
    /// [`ReconcilePolicy::UpdateDescriptive`], one entry per row whose
    /// descriptive columns were overwritten, naming the row and which columns
    /// changed.
    pub conflicts: Vec<String>,
}

/// One incoming JSON row of the time-series association catalog, as parsed
/// for [`reconcile_ts_rows`]. Every field the wire schema defines is
/// represented; unknown field names are rejected (typo protection), but `id`
/// and `address` are informational per D4 — `id` is never read here at all,
/// and `address` only ever feeds the `expected_address` check, never
/// identity or comparison.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTsRow {
    // Parsed only so a JSON row carrying it deserializes (`deny_unknown_fields`
    // would otherwise reject it); D4 makes both purely informational, so
    // neither is read past this struct. `owner_type` is a denormalized label
    // the identity tuple and every comparison ignore, matching the catalog's
    // own treatment of it.
    #[allow(dead_code)]
    #[serde(default)]
    id: Option<i64>,
    owner_id: i64,
    #[allow(dead_code)]
    owner_type: String,
    owner_category: String,
    time_series_type: String,
    name: String,
    #[serde(default)]
    features: Map<String, Value>,
    #[serde(default)]
    address: Option<String>,
    element_type: String,
    #[serde(default)]
    element_shape: Vec<usize>,
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

/// The tuple every row is matched by (D4): `(owner_id, owner_category,
/// time_series_type, name, resolution, interval, features)` — the same shape
/// as the catalog's `uq_ts_assoc` uniqueness index, just carrying decoded
/// values instead of the index's encoded columns.
type Identity = (
    i64,
    OwnerCategory,
    TimeSeriesType,
    String,
    Option<Period>,
    Option<Period>,
    Features,
);

fn identity_of_metadata(meta: &TimeSeriesMetadata) -> Identity {
    (
        meta.owner_id,
        meta.owner_category,
        meta.time_series_type,
        meta.name.clone(),
        meta.resolution,
        meta.interval,
        meta.features.clone(),
    )
}

/// A [`RawTsRow`] after its string-encoded fields (periods, timestamp,
/// element type, unit system, features) have been decoded into the store's
/// own types, so the comparisons in [`geometry_diff`] and [`descriptive_diff`]
/// never have to re-parse or juggle `Result`.
struct ParsedTsRow {
    identity: Identity,
    initial_timestamp: Option<DateTime<Utc>>,
    /// Static `length`, or — for a `Scenarios` row, which the wire form spells
    /// as `scenario_count` rather than `length` (D3: "`scenario_count` = the
    /// catalog's `length`") — that value instead. Folding the two into one
    /// field here is what lets [`geometry_diff`] compare a `Scenarios` row's
    /// geometry with the same code path as every other type.
    length: Option<usize>,
    horizon: Option<Period>,
    interval: Option<Period>,
    count: Option<usize>,
    element_type: ElementType,
    element_shape: Vec<usize>,
    percentiles: Option<Vec<f64>>,
    units: Option<String>,
    quantity_kind: Option<String>,
    unit_system: Option<UnitSystem>,
    component_field: Option<String>,
    application_data: Option<String>,
}

impl RawTsRow {
    /// A short human-readable label for error messages: enough to find the
    /// row in the source document without echoing the whole object.
    fn row_label(&self) -> String {
        format!(
            "{} owner {} \"{}\"",
            self.time_series_type, self.owner_id, self.name
        )
    }

    fn parse(&self) -> Result<ParsedTsRow> {
        let owner_category = OwnerCategory::parse(&self.owner_category).ok_or_else(|| {
            TimeSeriesError::InvalidParameter(format!(
                "{}: unknown owner_category {:?}",
                self.row_label(),
                self.owner_category
            ))
        })?;
        let time_series_type = TimeSeriesType::parse(&self.time_series_type).ok_or_else(|| {
            TimeSeriesError::InvalidParameter(format!(
                "{}: unknown time_series_type {:?}",
                self.row_label(),
                self.time_series_type
            ))
        })?;
        let element_type = ElementType::parse(&self.element_type).ok_or_else(|| {
            TimeSeriesError::InvalidParameter(format!(
                "{}: unknown element_type {:?}",
                self.row_label(),
                self.element_type
            ))
        })?;
        let unit_system = self
            .unit_system
            .as_deref()
            .map(parse_unit_system_wire)
            .transpose()?;
        let resolution = self
            .resolution
            .as_deref()
            .map(Period::from_iso8601)
            .transpose()?;
        let horizon = self
            .horizon
            .as_deref()
            .map(Period::from_iso8601)
            .transpose()?;
        let interval = self
            .interval
            .as_deref()
            .map(Period::from_iso8601)
            .transpose()?;
        let features = plain_to_features(&self.features)?;
        let initial_timestamp = self
            .initial_timestamp
            .as_deref()
            .map(|s| {
                DateTime::parse_from_rfc3339(s)
                    .map(|d| d.with_timezone(&Utc))
                    .map_err(|e| {
                        TimeSeriesError::InvalidParameter(format!(
                            "{}: invalid initial_timestamp {s:?}: {e}",
                            self.row_label()
                        ))
                    })
            })
            .transpose()?;

        Ok(ParsedTsRow {
            identity: (
                self.owner_id,
                owner_category,
                time_series_type,
                self.name.clone(),
                resolution,
                interval,
                features,
            ),
            initial_timestamp,
            length: self.length.or(self.scenario_count),
            horizon,
            interval,
            count: self.count,
            element_type,
            element_shape: self.element_shape.clone(),
            percentiles: self.percentiles.clone(),
            units: self.units.clone(),
            quantity_kind: self.quantity_kind.clone(),
            unit_system,
            component_field: self.component_field.clone(),
            application_data: self.application_data.clone(),
        })
    }
}

/// Geometry fields (D4): columns that describe the physically stored array,
/// which a document is never allowed to override under either policy.
/// `interval` is included for documentation completeness even though it is
/// also part of [`Identity`] — a *matched* row's interval can therefore never
/// actually be reported here, since a JSON row with a different interval
/// would already have failed to find a catalog match at all.
fn geometry_diff(row: &ParsedTsRow, meta: &TimeSeriesMetadata) -> Vec<&'static str> {
    let mut drift = Vec::new();
    let timestamps_agree = match (row.initial_timestamp, meta.initial_timestamp) {
        (Some(a), Some(b)) => a.timestamp_millis() == b.timestamp_millis(),
        (None, None) => true,
        _ => false,
    };
    if !timestamps_agree {
        drift.push("initial_timestamp");
    }
    // `meta.length` is populated for every type — `TypedArray::length` is
    // `shape[0]` — but the wire row only carries it for `SingleTimeSeries`,
    // `NonSequentialTimeSeries`, and `Scenarios` (as `scenario_count`). For
    // `Deterministic`/`DeterministicSingleTimeSeries`/`Probabilistic` it holds
    // the horizon-in-steps or percentile-count instead — an internal value the
    // wire contract never exposes, so a JSON row can never carry it and this
    // check must not compare it.
    let length_is_wire_visible = matches!(
        meta.time_series_type,
        TimeSeriesType::SingleTimeSeries
            | TimeSeriesType::NonSequentialTimeSeries
            | TimeSeriesType::Scenarios
    );
    if length_is_wire_visible && row.length != meta.length {
        drift.push("length");
    }
    if row.horizon != meta.horizon {
        drift.push("horizon");
    }
    if row.interval != meta.interval {
        drift.push("interval");
    }
    if row.count != meta.count {
        drift.push("count");
    }
    if row.element_type != meta.element_type {
        drift.push("element_type");
    }
    if row.element_shape.as_slice() != wire_element_shape(meta) {
        drift.push("element_shape");
    }
    if row.percentiles != meta.percentiles {
        drift.push("percentiles");
    }
    drift
}

/// Descriptive fields (D4): the five columns [`ReconcilePolicy::UpdateDescriptive`]
/// lets a JSON document override on a matched row, because none of them sits
/// in [`crate::types::key::TimeSeriesKey`], the catalog's uniqueness index, or
/// `data_hash`.
fn descriptive_diff(row: &ParsedTsRow, meta: &TimeSeriesMetadata) -> Vec<&'static str> {
    let mut drift = Vec::new();
    if row.units != meta.units {
        drift.push("units");
    }
    if row.quantity_kind != meta.quantity_kind {
        drift.push("quantity_kind");
    }
    if row.unit_system != meta.unit_system {
        drift.push("unit_system");
    }
    if row.component_field != meta.component_field {
        drift.push("component_field");
    }
    if row.application_data != meta.application_data {
        drift.push("application_data");
    }
    drift
}

/// Reconcile a JSON array of time-series association rows against the
/// store's catalog under `policy` (D4). See [`ReconcilePolicy`] and
/// [`ReconcileReport`] for the full semantics.
fn reconcile_ts_rows(
    store: &mut Store,
    json: &str,
    policy: ReconcilePolicy,
    expected_address: Option<&str>,
) -> Result<ReconcileReport> {
    let rows: Vec<RawTsRow> = serde_json::from_str(json)?;

    let catalog = store.list_time_series_with_id(ListFilter::new())?;
    let mut by_identity: HashMap<Identity, (i64, TimeSeriesMetadata)> =
        HashMap::with_capacity(catalog.len());
    for (id, meta) in catalog {
        by_identity.insert(identity_of_metadata(&meta), (id, meta));
    }

    let mut referenced: HashSet<Identity> = HashSet::new();
    let mut fatal: Vec<String> = Vec::new();
    let mut clean = 0usize;
    let mut updates: Vec<DescriptiveUpdate> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();

    for row in &rows {
        if let (Some(expected), Some(actual)) = (expected_address, row.address.as_deref())
            && actual != expected
        {
            fatal.push(format!(
                "{}: address {actual:?} does not match the expected storage file {expected:?}",
                row.row_label()
            ));
            continue;
        }

        let parsed = match row.parse() {
            Ok(parsed) => parsed,
            Err(e) => {
                fatal.push(format!("{}: {e}", row.row_label()));
                continue;
            }
        };

        let Some((id, meta)) = by_identity.get(&parsed.identity) else {
            fatal.push(format!(
                "{}: the store's catalog holds no series with this identity",
                row.row_label()
            ));
            continue;
        };
        referenced.insert(parsed.identity.clone());

        let geometry_drift = geometry_diff(&parsed, meta);
        if !geometry_drift.is_empty() {
            fatal.push(format!(
                "{}: geometry drift ({}) — a document can never override the columns that \
                 describe the stored array",
                row.row_label(),
                geometry_drift.join(", ")
            ));
            continue;
        }

        let descriptive_drift = descriptive_diff(&parsed, meta);
        if descriptive_drift.is_empty() {
            clean += 1;
            continue;
        }
        match policy {
            ReconcilePolicy::Strict => {
                fatal.push(format!(
                    "{}: descriptive drift ({}) under strict reconcile",
                    row.row_label(),
                    descriptive_drift.join(", ")
                ));
            }
            ReconcilePolicy::UpdateDescriptive => {
                conflicts.push(format!(
                    "{}: updated descriptive columns ({})",
                    row.row_label(),
                    descriptive_drift.join(", ")
                ));
                updates.push(DescriptiveUpdate {
                    id: *id,
                    units: parsed.units,
                    quantity_kind: parsed.quantity_kind,
                    unit_system: parsed.unit_system,
                    component_field: parsed.component_field,
                    application_data: parsed.application_data,
                });
            }
        }
    }

    if !fatal.is_empty() {
        return Err(TimeSeriesError::ReconcileConflict(fatal.join("; ")));
    }

    let unmatched_in_store = by_identity.len() - referenced.len();
    let updated = updates.len();
    let matched = clean + updated;
    if !updates.is_empty() {
        store.update_time_series_descriptive_bulk(&updates)?;
    }

    Ok(ReconcileReport {
        matched,
        updated,
        missing_in_store: 0,
        unmatched_in_store,
        conflicts,
    })
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
    serde_json::to_string(&rows).map_err(Into::into)
}

/// One row of the incoming SA-association JSON, denying unknown fields so a
/// typo'd column name fails loudly rather than silently vanishing. Unlike the
/// time-series reconcile, this is a straight bulk insert: the row shape
/// already matches [`SupplementalAttributeAssociation`] exactly, with no
/// `id`/`address` fields to tolerate.
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
        }
    }
}

/// Bulk-ingest a JSON array of SA-association rows through the existing
/// all-or-nothing insert path (one savepoint;
/// [`TimeSeriesError::DuplicateAssociation`] propagates and rolls the whole
/// batch back). Returns the number of rows inserted.
fn import_sa_rows(store: &mut Store, json: &str) -> Result<usize> {
    let rows: Vec<RawSaRow> = serde_json::from_str(json)?;
    let associations: Vec<SupplementalAttributeAssociation> =
        rows.into_iter().map(Into::into).collect();
    store.add_supplemental_attribute_associations(associations)
}

// ============================================================================
// `Store` public API
// ============================================================================

impl Store {
    /// Export `time_series_associations` matching `filter` as a JSON array of
    /// OpenAPI rows, each stamped with `address` verbatim. See the module
    /// docs for the wire contract (D3) and sort order.
    pub fn export_time_series_associations_openapi(
        &self,
        address: &str,
        filter: &ListFilter,
    ) -> Result<String> {
        export_ts_rows(self, address, filter)
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

    /// Reconcile a JSON array of time-series association rows against this
    /// store's catalog (D4): match by identity, apply `policy` to any
    /// descriptive drift, and error loudly on anything neither policy can
    /// resolve. See [`ReconcilePolicy`] and [`ReconcileReport`].
    pub fn reconcile_time_series_associations_openapi(
        &mut self,
        json: &str,
        policy: ReconcilePolicy,
        expected_address: Option<&str>,
    ) -> Result<ReconcileReport> {
        reconcile_ts_rows(self, json, policy, expected_address)
    }
}
