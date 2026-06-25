use std::collections::BTreeMap;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::period::Period;
use super::time_series::TimeSeriesType;

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

#[derive(Debug, Clone, PartialEq)]
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

    // Physical + logical element typing of the stored array.
    /// Element dtype of the stored array.
    pub dtype: super::array::Dtype,
    /// Per-step element shape (trailing dims after time); empty = scalar.
    pub element_shape: Vec<usize>,
    /// Opaque logical-type label for domain reconstruction by the binding
    /// (e.g. `"QuadraticFunctionData"`); the store never interprets it.
    pub logical_type: Option<String>,
}
