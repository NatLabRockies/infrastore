use std::collections::BTreeMap;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::period::Period;
use super::time_series::TimeSeriesType;
use crate::error::{Result, TimeSeriesError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OwnerCategory {
    Component,
    SupplementalAttribute,
}

impl OwnerCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            OwnerCategory::Component => "Component",
            OwnerCategory::SupplementalAttribute => "SupplementalAttribute",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Component" => OwnerCategory::Component,
            "SupplementalAttribute" => OwnerCategory::SupplementalAttribute,
            _ => return None,
        })
    }

    /// The storage code written to the SQLite catalog.
    ///
    /// As with [`crate::TimeSeriesType::code`], this is the *storage* encoding
    /// and [`Self::as_str`] is the *display and serde* one. Part of the on-disk
    /// contract: changing it requires a [`crate::DATA_FORMAT_VERSION`] bump.
    pub fn code(self) -> i64 {
        match self {
            OwnerCategory::Component => 0,
            OwnerCategory::SupplementalAttribute => 1,
        }
    }

    /// Inverse of [`Self::code`]. `None` for an unknown code, which in the
    /// catalog means a store written by an incompatible version.
    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => OwnerCategory::Component,
            1 => OwnerCategory::SupplementalAttribute,
            _ => return None,
        })
    }
}

impl FromStr for OwnerCategory {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s).ok_or(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FeatureValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

impl Eq for FeatureValue {}

impl std::hash::Hash for FeatureValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            FeatureValue::Int(v) => {
                0u8.hash(state);
                v.hash(state);
            }
            FeatureValue::Float(v) => {
                1u8.hash(state);
                // Hash NaNs as a single canonical bit pattern so logical
                // equality holds for hash and PartialEq alike.
                let bits = if v.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    v.to_bits()
                };
                bits.hash(state);
            }
            FeatureValue::Bool(v) => {
                2u8.hash(state);
                v.hash(state);
            }
            FeatureValue::Str(v) => {
                3u8.hash(state);
                v.hash(state);
            }
        }
    }
}

impl FeatureValue {
    pub fn kind(&self) -> &'static str {
        match self {
            FeatureValue::Int(_) => "int",
            FeatureValue::Float(_) => "float",
            FeatureValue::Bool(_) => "bool",
            FeatureValue::Str(_) => "str",
        }
    }
}

/// Sorted-by-key feature map. `BTreeMap` gives the sort order invariant for
/// free, which matters for hashing and the metadata uniqueness constraint.
pub type Features = BTreeMap<String, FeatureValue>;

/// Feature names the store refuses to accept, because each already names a
/// field of a time-series struct or of the key/metadata tuple that addresses
/// one. Consumers routinely spread a feature map into a keyword-argument query
/// (`get_time_series(...; name = "load", model_year = 2030)`), where a feature
/// called `name` or `resolution` would shadow the real field and silently
/// change what the query means. Rejecting them at write time keeps that
/// ambiguity out of the store entirely.
///
/// The comparison is exact and case-sensitive, matching how the catalog treats
/// every other identifier: `resolution` is reserved, `Resolution` is not.
///
/// Kept sorted so [`is_reserved_feature_name`] can binary-search it, and so a
/// reader can scan it.
pub const RESERVED_FEATURE_NAMES: &[&str] = &[
    "count",
    "data",
    "data_hash",
    "dtype",
    "element_shape",
    "ext",
    "features",
    "horizon",
    "initial_timestamp",
    "interval",
    "length",
    "name",
    "owner_category",
    "owner_id",
    "owner_type",
    "percentiles",
    "resolution",
    "scenario_count",
    "time_series_type",
    "timestamps",
    "units",
];

/// Whether `name` is one of the [`RESERVED_FEATURE_NAMES`].
pub fn is_reserved_feature_name(name: &str) -> bool {
    RESERVED_FEATURE_NAMES.binary_search(&name).is_ok()
}

/// Reject a feature map that uses a reserved name as a key.
///
/// Applied on the write path only, so a store written before this rule existed
/// stays readable and its offending series can still be listed and removed.
pub fn validate_features(features: &Features) -> Result<()> {
    for key in features.keys() {
        if is_reserved_feature_name(key) {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "feature name {key:?} is reserved: it names a field of a time series \
                 or of the key that addresses one; reserved names are {}",
                RESERVED_FEATURE_NAMES.join(", ")
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeSeriesMetadata {
    pub owner_id: i64,
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub data_hash: [u8; 32],

    // Temporal fields (all `Option` so future variants can leave some unset).
    pub initial_timestamp: Option<DateTime<Utc>>,
    pub resolution: Option<Period>,
    pub length: Option<usize>,
    pub horizon: Option<Period>,
    pub interval: Option<Period>,
    pub count: Option<usize>,
    pub timestamps: Option<Vec<DateTime<Utc>>>,

    pub features: Features,
    pub units: Option<String>,
    /// Percentiles for a `Probabilistic` forecast; `None` for other types.
    pub percentiles: Option<Vec<f64>>,

    // Physical element typing of the stored array.
    /// Element dtype of the stored array.
    pub dtype: super::array::Dtype,
    /// Per-step element shape (trailing dims after time); empty = scalar.
    pub element_shape: Vec<usize>,
    /// Opaque, package-owned extension payload stored verbatim (typically a
    /// JSON object such as `{"function_type":"QuadraticFunctionData"}` that a
    /// binding writes and reads to reconstruct its domain objects). The store
    /// never parses or interprets it; end users are not expected to set it.
    pub ext: Option<String>,
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};

    use super::*;
    use crate::types::array::TypedArray;
    use crate::types::key::KeyIdentity;
    use crate::types::time_series::{
        Deterministic, NonSequentialTimeSeries, Probabilistic, Scenarios, SingleTimeSeries,
    };

    fn features_of(pairs: &[(&str, FeatureValue)]) -> Features {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    /// Field names of a serde-serialized struct — the same spelling a consumer
    /// sees when it spreads a time series into keyword arguments.
    fn field_names<T: Serialize>(value: &T) -> Vec<String> {
        match serde_json::to_value(value).expect("struct serializes to JSON") {
            serde_json::Value::Object(map) => map.keys().cloned().collect(),
            other => panic!("expected a JSON object, got {other:?}"),
        }
    }

    #[test]
    fn reserved_names_are_sorted_and_unique() {
        // `is_reserved_feature_name` binary-searches, so order is load-bearing.
        let mut sorted = RESERVED_FEATURE_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, RESERVED_FEATURE_NAMES);
    }

    /// The canary for the whole rule: every field of every time-series struct,
    /// of the metadata row, and of the key identity must be reserved. Adding a
    /// field without adding its name here fails this test.
    #[test]
    fn every_time_series_field_name_is_reserved() {
        let t0 = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
        let hour = Duration::hours(1);
        let arr = |shape: Vec<usize>| {
            let n: usize = shape.iter().product();
            let values: Vec<f64> = (0..n).map(|i| i as f64).collect();
            TypedArray::from_f64(shape, &values)
        };

        let single = SingleTimeSeries::new(t0, hour, arr(vec![2]), "load");
        let non_sequential =
            NonSequentialTimeSeries::new(vec![t0, t0 + hour], arr(vec![2]), "load").unwrap();
        // H = 2, count = 3.
        let deterministic =
            Deterministic::new(t0, hour, hour * 2, hour, 3, arr(vec![2, 3]), "load").unwrap();
        let probabilistic = Probabilistic::new(
            t0,
            hour,
            hour * 2,
            hour,
            3,
            vec![0.1, 0.9],
            arr(vec![2, 2, 3]),
            "load",
        )
        .unwrap();
        let scenarios =
            Scenarios::new(t0, hour, hour * 2, hour, 3, 4, arr(vec![4, 2, 3]), "load").unwrap();

        let metadata = TimeSeriesMetadata {
            owner_id: 1,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::SingleTimeSeries,
            name: "load".into(),
            data_hash: [0u8; 32],
            initial_timestamp: Some(t0),
            resolution: Some(Period::Fixed(hour)),
            length: Some(2),
            horizon: None,
            interval: None,
            count: None,
            timestamps: None,
            features: Features::new(),
            units: None,
            percentiles: None,
            dtype: single.data.dtype,
            element_shape: vec![],
            ext: None,
        };
        let identity = KeyIdentity {
            owner_id: 1,
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::SingleTimeSeries,
            name: "load".into(),
            resolution: Some(Period::Fixed(hour)),
            interval: None,
            features: Features::new(),
        };

        let mut names = Vec::new();
        names.extend(field_names(&single));
        names.extend(field_names(&non_sequential));
        names.extend(field_names(&deterministic));
        names.extend(field_names(&probabilistic));
        names.extend(field_names(&scenarios));
        names.extend(field_names(&metadata));
        names.extend(field_names(&identity));

        for name in names {
            assert!(
                is_reserved_feature_name(&name),
                "{name:?} is a field of a time series or its key but is not in \
                 RESERVED_FEATURE_NAMES; add it there"
            );
        }
    }

    #[test]
    fn validate_features_accepts_ordinary_names() {
        let features = features_of(&[
            ("model_year", FeatureValue::Int(2030)),
            ("scenario", FeatureValue::Str("high".into())),
            ("calibrated", FeatureValue::Bool(true)),
        ]);
        assert!(validate_features(&features).is_ok());
        assert!(validate_features(&Features::new()).is_ok());
    }

    #[test]
    fn validate_features_rejects_every_reserved_name() {
        for reserved in RESERVED_FEATURE_NAMES {
            let features = features_of(&[
                ("model_year", FeatureValue::Int(2030)),
                (reserved, FeatureValue::Str("x".into())),
            ]);
            let err = validate_features(&features).unwrap_err();
            assert!(
                matches!(err, TimeSeriesError::InvalidParameter(ref m) if m.contains(reserved)),
                "{reserved} should be rejected by name, got {err}"
            );
        }
    }

    #[test]
    fn reserved_names_match_case_sensitively() {
        // Only the exact spelling is reserved; the catalog is case-sensitive
        // everywhere else, and a case-folded rule would reject legitimate
        // feature names that merely resemble a field.
        assert!(is_reserved_feature_name("resolution"));
        for spelling in ["Resolution", "RESOLUTION", "resolution_", "my_resolution"] {
            assert!(!is_reserved_feature_name(spelling), "{spelling}");
            let features = features_of(&[(spelling, FeatureValue::Int(1))]);
            assert!(validate_features(&features).is_ok(), "{spelling}");
        }
    }
}
