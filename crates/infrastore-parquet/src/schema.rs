//! The names a long table uses, and the codecs for the values that are not
//! plain text.
//!
//! Every one of these is a **column** of the table (and, for the three partition
//! keys, also a footer entry). They are gathered here rather than beside the
//! writer because the reader has to agree with them exactly, and a name that
//! drifts between the two is a bug neither side can see.
//!
//! Values are UTF-8, because a Parquet string column is. Structure —
//! `element_shape`, `features` — rides as JSON, which is the spelling the rest of
//! the project already uses for the same values.

use infrastore_core::{
    ElementType, FeatureValue, Features, OwnerCategory, TimeReference, TimeSeriesType, UnitSystem,
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

// ---- Forecast columns -------------------------------------------------------
//
// Present only in the partitions whose type has them. A forecast's grid is what
// the rows cannot say: a `(issue_time, target_time, value)` row states where a
// value belongs, not what the grid it belongs to *is*, so the interval and
// horizon travel as columns of their own.

/// ISO-8601 forecast interval: how far apart two windows are issued.
pub const INTERVAL: &str = "interval";
/// ISO-8601 forecast horizon: how far ahead one window reaches.
pub const HORIZON: &str = "horizon";

/// `[2,3]`, or `[]` for a scalar element.
pub fn encode_element_shape(shape: &[usize]) -> String {
    serde_json::to_string(shape).expect("a list of integers always serializes")
}

/// The feature map as a JSON object of plain scalars: `{"model_year":2030}`.
///
/// Deliberately **not** `serde_json::to_string(features)`, which would emit
/// `FeatureValue`'s externally tagged form `{"model_year":{"Int":2030}}`. The
/// plain form is the spelling every other wire in this project uses for a
/// feature map — the C ABI's `features_json`, the CLI's `--features` — and
/// someone reading this column in DuckDB should see the value, not the
/// discriminant that happens to carry it.
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
