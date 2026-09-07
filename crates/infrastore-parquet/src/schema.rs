//! The footer: which key/value pairs describe a series, and how each is spelled.
//!
//! **One schema, two producers.** Python's `SingleTimeSeries.to_arrow()` and
//! this crate's export write the same table for the same series, so a Parquet
//! file has one shape whichever wrote it and the import reads that one shape.
//! Every key here is either one `to_arrow()` already writes or one it was
//! extended to write in the same change.
//!
//! Five keys are the exception, and they are the *row-level* ones —
//! [`ID`], [`OWNER_ID`], [`OWNER_TYPE`], [`OWNER_CATEGORY`], [`FEATURES`].
//! `to_arrow()` is a method on a value object, which has no owner and no catalog
//! id: a `SingleTimeSeries` built in Python is not filed anywhere. So a
//! CLI-written file is self-describing enough to `add` back with no flags, while
//! a `to_arrow()` file needs the owner supplied — by a descriptor or by the
//! inline flags — exactly as a CSV does. The import reads whichever keys are
//! present.
//!
//! Values are UTF-8 strings, because Parquet key/value metadata is. Structure
//! (`element_shape`, `features`) rides as JSON, which is the spelling the rest
//! of the project already uses for the same values.

use std::collections::BTreeMap;

use infrastore_core::{
    ElementType, FeatureValue, Features, OwnerCategory, TimeReference, TimeSeriesMetadata,
    TimeSeriesType, UnitSystem,
};

/// Which of the six types the rows describe. Present on every file, and the one
/// key that tells a `PersistentTimeSeries` from a `NonSequentialTimeSeries` —
/// the two share storage and a table shape, and differ only in read semantics.
pub const TIME_SERIES_TYPE: &str = "time_series_type";
/// The series' name. Part of its identity, so it is not optional.
pub const NAME: &str = "name";
/// Canonical [`ElementType`] spelling: `f64`, `tuple(3,f64)`, `piecewise_linear`.
pub const ELEMENT_TYPE: &str = "element_type";
/// Per-step trailing dims as a JSON array, `[]` for a scalar element.
///
/// Written even when empty, unlike the descriptors below: an absent descriptor
/// means "not declared", where an empty shape is a fact about the data.
/// Redundant with the `value` column's Arrow type by construction, and kept so a
/// reader that only looks at the footer does not have to walk a nested
/// `FixedSizeList` to recover it.
pub const ELEMENT_SHAPE: &str = "element_shape";
/// ISO-8601 grid step. `SingleTimeSeries` only — an irregular timeline has no
/// constant step, and its absence is how a reader knows that.
pub const RESOLUTION: &str = "resolution";
/// [`TimeReference::as_storage_string`]: `utc`, `zoneless`, `-07:00`, or an IANA
/// name — plus [`UNSPECIFIED_REFERENCE`] for a series that records none.
///
/// Spelled explicitly rather than inferred from the timestamp column's zone,
/// because Arrow cannot express the difference: a `timestamp[ms]` with no zone
/// is what both `Zoneless` and *unspecified* would produce, and those are
/// different claims. **Always written**, for the same reason: leaving the key
/// out for an unspecified reference would leave nothing but the column's zone to
/// go on, and the export writes a UTC-zoned column there — so an unspecified
/// reference would come back as `utc`, a claim the series never made.
pub const TIME_REFERENCE: &str = "time_reference";

/// What [`TIME_REFERENCE`] holds for a series that records no spelling.
///
/// Deliberately **not** a `TimeReference` variant and not something
/// `TimeReference::parse` accepts: unspecified is `None`, not a fourth kind of
/// reference, and teaching the core's parser this literal would also make the
/// CLI's `--time-reference unspecified` a thing a caller could write. It is a
/// footer encoding, so it lives here, and it is decoded back to `None`.
///
/// `to_arrow()` writes the same literal (`crates/infrastore-py/src/lib.rs`);
/// `the_unspecified_literal_matches_the_python_binding` pins the two together.
///
/// One collision is accepted rather than engineered around: a series whose
/// reference is literally `Zone("unspecified")` writes the same footer value and
/// comes back as unspecified. `unspecified` is not an IANA zone, so nothing could
/// ever resolve such a reference anyway, and every alternative encoding is
/// collidable in the same way.
pub const UNSPECIFIED_REFERENCE: &str = "unspecified";

/// Free-form unit label.
pub const UNITS: &str = "units";
/// What kind of physical quantity the values measure.
pub const QUANTITY_KIND: &str = "quantity_kind";
/// `natural_units` or `component_base`.
pub const UNIT_SYSTEM: &str = "unit_system";
/// The owning component's field these values vary.
pub const COMPONENT_FIELD: &str = "component_field";
/// The opaque package-owned payload, verbatim.
pub const APPLICATION_DATA: &str = "application_data";

/// The catalog row's id. Written so a file names the row it came from, and
/// **ignored on import**: no `add_*` accepts an id, because "never reissued" is
/// a guarantee of `AUTOINCREMENT` and a caller free to name one could re-file a
/// retired id.
pub const ID: &str = "id";
/// The owning component's (or attribute's) id.
pub const OWNER_ID: &str = "owner_id";
/// The owner's type name.
pub const OWNER_TYPE: &str = "owner_type";
/// `Component` or `SupplementalAttribute`, in [`OwnerCategory::as_str`]'s
/// spelling.
pub const OWNER_CATEGORY: &str = "owner_category";
/// The feature map as a JSON object, in the spelling `Features` serializes to.
pub const FEATURES: &str = "features";

// ---- Forecast keys ---------------------------------------------------------
//
// Written only for the dense forecast types, whose long table cannot be read
// back without them: a `(issue_time, target_time, value)` row says where a value
// belongs but not what the grid it belongs to *is*, and inferring five
// parameters from a set of rows would be guessing.

/// The first window's issue time, RFC 3339. The anchor of the window grid.
pub const INITIAL_TIMESTAMP: &str = "initial_timestamp";
/// ISO-8601 forecast horizon: how far ahead one window reaches.
pub const HORIZON: &str = "horizon";
/// ISO-8601 forecast interval: how far apart two windows are issued.
pub const INTERVAL: &str = "interval";
/// Number of windows.
pub const COUNT: &str = "count";
/// `Probabilistic` only: the percentiles, as a JSON array of numbers, in the
/// order the stored array's leading axis is in.
pub const PERCENTILES: &str = "percentiles";
/// `Scenarios` only: how many trajectories each window carries.
pub const SCENARIO_COUNT: &str = "scenario_count";

/// The footer for one catalog row.
///
/// Absent descriptors are **left out** rather than written as an empty string,
/// so `metadata.contains_key(UNITS)` answers "was a label declared?" — the same
/// rule `to_arrow()` follows, and the reason the import can tell "no units" from
/// `units = ""`.
pub fn metadata_for_row(row: &TimeSeriesMetadata) -> BTreeMap<String, String> {
    let mut meta = BTreeMap::new();
    meta.insert(
        TIME_SERIES_TYPE.to_string(),
        row.time_series_type.as_str().to_string(),
    );
    meta.insert(NAME.to_string(), row.name.clone());
    meta.insert(ELEMENT_TYPE.to_string(), row.element_type.to_string());
    meta.insert(
        ELEMENT_SHAPE.to_string(),
        encode_element_shape(&row.element_shape),
    );
    if let Some(resolution) = row.resolution {
        meta.insert(RESOLUTION.to_string(), resolution.to_iso8601());
    }
    // Forecast-only. A static row leaves all of these unset, so nothing here
    // changes the footer `to_arrow()` writes for the three static types.
    if row.time_series_type.is_forecast() {
        if let Some(initial) = row.initial_timestamp {
            meta.insert(INITIAL_TIMESTAMP.to_string(), initial.to_rfc3339());
        }
        if let Some(horizon) = row.horizon {
            meta.insert(HORIZON.to_string(), horizon.to_iso8601());
        }
        if let Some(interval) = row.interval {
            meta.insert(INTERVAL.to_string(), interval.to_iso8601());
        }
        if let Some(count) = row.count {
            meta.insert(COUNT.to_string(), count.to_string());
        }
        if let Some(percentiles) = &row.percentiles {
            meta.insert(
                PERCENTILES.to_string(),
                serde_json::to_string(percentiles).expect("a list of floats always serializes"),
            );
        }
    }
    // Written for every row, unlike the descriptors below. An absent descriptor
    // means "not declared"; an absent reference would mean "read it off the
    // column's zone", which for an unspecified reference gives `utc` -- a claim
    // the series never made. The literal says so instead.
    meta.insert(
        TIME_REFERENCE.to_string(),
        row.time_reference.as_ref().map_or_else(
            || UNSPECIFIED_REFERENCE.to_string(),
            |r| r.as_storage_string(),
        ),
    );
    insert_opt(&mut meta, UNITS, row.units.as_deref());
    insert_opt(&mut meta, QUANTITY_KIND, row.quantity_kind.as_deref());
    insert_opt(&mut meta, UNIT_SYSTEM, row.unit_system.map(|u| u.as_str()));
    insert_opt(&mut meta, COMPONENT_FIELD, row.component_field.as_deref());
    insert_opt(&mut meta, APPLICATION_DATA, row.application_data.as_deref());

    // Row-level keys. `id` is descriptive of the row rather than the data, and
    // is written for provenance only.
    if let Some(id) = row.id {
        meta.insert(ID.to_string(), id.get().to_string());
    }
    meta.insert(OWNER_ID.to_string(), row.owner_id.to_string());
    meta.insert(OWNER_TYPE.to_string(), row.owner_type.clone());
    meta.insert(
        OWNER_CATEGORY.to_string(),
        row.owner_category.as_str().to_string(),
    );
    // Always present, `{}` for the common empty map: a reader distinguishing
    // "no features" from "features not recorded" would be drawing a line the
    // store does not draw, since every row has a feature map.
    meta.insert(FEATURES.to_string(), encode_features(&row.features));
    meta
}

fn insert_opt(meta: &mut BTreeMap<String, String>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        meta.insert(key.to_string(), value.to_string());
    }
}

/// `[2,3]`, or `[]` for a scalar element.
pub fn encode_element_shape(shape: &[usize]) -> String {
    serde_json::to_string(shape).expect("a list of integers always serializes")
}

/// The feature map as a JSON object of plain scalars: `{"model_year":2030}`.
///
/// Deliberately **not** `serde_json::to_string(features)`, which would emit
/// `FeatureValue`'s externally tagged form `{"model_year":{"Int":2030}}`. The
/// plain form is the spelling every other wire in this project uses for a
/// feature map — the C ABI's `features_json`, the CLI's `--features` — and a
/// foreign reader of this footer should see the value, not the discriminant that
/// happens to carry it.
pub fn encode_features(features: &Features) -> String {
    let object: serde_json::Map<String, serde_json::Value> = features
        .iter()
        .map(|(k, v)| {
            let value = match v {
                FeatureValue::Int(i) => serde_json::Value::from(*i),
                FeatureValue::Float(f) => serde_json::Value::from(*f),
                FeatureValue::Bool(b) => serde_json::Value::from(*b),
                FeatureValue::Str(s) => serde_json::Value::from(s.clone()),
            };
            (k.clone(), value)
        })
        .collect();
    serde_json::Value::Object(object).to_string()
}

/// Parse an `element_shape` value back.
pub fn decode_element_shape(text: &str) -> Result<Vec<usize>, String> {
    serde_json::from_str(text)
        .map_err(|e| format!("{ELEMENT_SHAPE} is not a list of integers: {e}"))
}

/// Parse a `features` value back, inferring each value's kind from its JSON
/// type — the inverse of [`encode_features`], and the same inference the C ABI
/// and the CLI do.
pub fn decode_features(text: &str) -> Result<Features, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("{FEATURES} is not JSON: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| format!("{FEATURES} must be a JSON object"))?;
    let mut features = Features::new();
    for (key, value) in object {
        let feature = match value {
            serde_json::Value::Bool(b) => FeatureValue::Bool(*b),
            serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
                (Some(i), _) => FeatureValue::Int(i),
                (None, Some(f)) => FeatureValue::Float(f),
                (None, None) => return Err(format!("feature {key}: number out of range")),
            },
            serde_json::Value::String(s) => FeatureValue::Str(s.clone()),
            _ => {
                return Err(format!(
                    "feature {key}: must be an int, float, bool, or string"
                ));
            }
        };
        features.insert(key.clone(), feature);
    }
    Ok(features)
}

/// Parse a `percentiles` value back.
pub fn decode_percentiles(text: &str) -> Result<Vec<f64>, String> {
    serde_json::from_str(text).map_err(|e| format!("{PERCENTILES} is not a list of numbers: {e}"))
}

/// Parse a `time_series_type` value back.
pub fn decode_time_series_type(text: &str) -> Result<TimeSeriesType, String> {
    TimeSeriesType::parse(text).ok_or_else(|| format!("unknown {TIME_SERIES_TYPE} {text:?}"))
}

/// Parse an `element_type` value back.
pub fn decode_element_type(text: &str) -> Result<ElementType, String> {
    ElementType::parse(text).ok_or_else(|| format!("unknown {ELEMENT_TYPE} {text:?}"))
}

/// Parse a `time_reference` value back. `None` for [`UNSPECIFIED_REFERENCE`].
///
/// The `Option` is the point: unspecified is the absence of a reference, so it
/// is decoded here rather than in `TimeReference::parse`, which must keep
/// refusing the literal -- see [`UNSPECIFIED_REFERENCE`].
pub fn decode_time_reference(text: &str) -> Result<Option<TimeReference>, String> {
    if text == UNSPECIFIED_REFERENCE {
        return Ok(None);
    }
    TimeReference::parse(text)
        .map(Some)
        .map_err(|e| format!("{TIME_REFERENCE} {text:?}: {e}"))
}

/// Parse an `owner_category` value back.
pub fn decode_owner_category(text: &str) -> Result<OwnerCategory, String> {
    OwnerCategory::parse(text).ok_or_else(|| format!("unknown {OWNER_CATEGORY} {text:?}"))
}

/// Parse a `unit_system` value back.
pub fn decode_unit_system(text: &str) -> Result<UnitSystem, String> {
    UnitSystem::parse(text).ok_or_else(|| format!("unknown {UNIT_SYSTEM} {text:?}"))
}
