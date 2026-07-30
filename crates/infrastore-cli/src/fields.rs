//! Shared rendering of metadata fields, so every command spells a feature map,
//! a period, or a content hash the same way.

use infrastore_core::{FeatureValue, Features, TimeSeriesMetadata};
use serde_json::{Map, Value, json};

use crate::parse;

/// How many leading hex characters of a content hash the table views show.
///
/// A full hash is 64 characters and would dominate any table. 12 is the git
/// short-hash convention and is ample here: a store would need on the order of
/// 2^24 distinct arrays before a prefix collision became likely, and the full
/// value is always one `info` or `-f json` away.
pub const SHORT_HASH_LEN: usize = 12;

/// Lowercase hex of a content hash, matching `hash_hex` in the core and the
/// `time_series_readable` SQLite view.
pub fn hash_hex(hash: &[u8; 32]) -> String {
    infrastore_core::hash_hex(hash)
}

/// The leading [`SHORT_HASH_LEN`] characters of a content hash.
pub fn short_hash(hash: &[u8; 32]) -> String {
    let mut s = hash_hex(hash);
    s.truncate(SHORT_HASH_LEN);
    s
}

/// Render a feature map as `k=v` pairs joined by `,`, or `-` when empty.
///
/// Features are part of a series' identity, so this is what keeps two rows that
/// differ only by feature distinguishable in a table.
pub fn features_str(features: &Features) -> String {
    if features.is_empty() {
        return "-".to_string();
    }
    features
        .iter()
        .map(|(k, v)| format!("{k}={}", feature_value_str(v)))
        .collect::<Vec<_>>()
        .join(",")
}

/// A single feature value as text.
pub fn feature_value_str(v: &FeatureValue) -> String {
    match v {
        FeatureValue::Int(i) => i.to_string(),
        FeatureValue::Float(f) => f.to_string(),
        FeatureValue::Bool(b) => b.to_string(),
        FeatureValue::Str(s) => s.clone(),
    }
}

/// A feature map as a JSON object with values kept in their own types.
pub fn features_json(features: &Features) -> Value {
    let mut obj = Map::new();
    for (k, v) in features {
        obj.insert(k.clone(), feature_value_json(v));
    }
    Value::Object(obj)
}

/// A feature value as a JSON scalar of its own type (not a string).
pub fn feature_value_json(v: &FeatureValue) -> Value {
    match v {
        FeatureValue::Int(i) => json!(i),
        FeatureValue::Float(f) => json!(f),
        FeatureValue::Bool(b) => json!(b),
        FeatureValue::Str(s) => json!(s),
    }
}

/// An optional period as its ISO-8601 spelling, or `-`.
pub fn opt_period(p: Option<infrastore_core::Period>) -> String {
    p.map(parse::format_period)
        .unwrap_or_else(|| "-".to_string())
}

/// An optional `Display` value as text, or `-`.
pub fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string())
}

/// One line identifying a series by every field that is part of its identity.
///
/// Used by the ambiguous-selector error, where the whole point is that the
/// caller must be able to tell the candidates apart and construct a narrowing
/// flag from what is printed. Omitting features here is what made that error a
/// dead end for feature-partitioned stores.
pub fn identity_line(m: &TimeSeriesMetadata) -> String {
    format!(
        "owner={} owner_category={} type={} name={} resolution={} interval={} features={} \
         data_hash={}",
        m.owner_id,
        m.owner_category.as_str(),
        m.time_series_type.as_str(),
        m.name,
        opt_period(m.resolution),
        opt_period(m.interval),
        features_str(&m.features),
        short_hash(&m.data_hash),
    )
}
