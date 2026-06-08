//! Conversions between generated protobuf types and `time_series_store_core` types.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use time_series_store_core::{
    Deterministic, Dtype, FeatureValue, Features, NonSequentialTimeSeries, OwnerCategory,
    Probabilistic, Scenarios, SingleTimeSeries, TimeSeriesData, TimeSeriesKey, TimeSeriesMetadata,
    TimeSeriesType, TypedArray,
};

use crate::pb;

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("invalid value for {field}: {message}")]
    InvalidValue {
        field: &'static str,
        message: String,
    },
    #[error("data_hash must be exactly 32 bytes, got {0}")]
    BadHashLen(usize),
    #[error("invalid RFC3339 timestamp: {0}")]
    BadTimestamp(#[from] chrono::ParseError),
}

// ---- Enums ----

impl From<TimeSeriesType> for pb::TimeSeriesType {
    fn from(value: TimeSeriesType) -> Self {
        match value {
            TimeSeriesType::SingleTimeSeries => pb::TimeSeriesType::SingleTimeSeries,
            TimeSeriesType::NonSequentialTimeSeries => pb::TimeSeriesType::NonSequentialTimeSeries,
            TimeSeriesType::Deterministic => pb::TimeSeriesType::Deterministic,
            TimeSeriesType::DeterministicSingleTimeSeries => {
                pb::TimeSeriesType::DeterministicSingleTimeSeries
            }
            TimeSeriesType::Probabilistic => pb::TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios => pb::TimeSeriesType::Scenarios,
        }
    }
}

impl From<pb::TimeSeriesType> for TimeSeriesType {
    fn from(value: pb::TimeSeriesType) -> Self {
        match value {
            pb::TimeSeriesType::SingleTimeSeries => TimeSeriesType::SingleTimeSeries,
            pb::TimeSeriesType::NonSequentialTimeSeries => TimeSeriesType::NonSequentialTimeSeries,
            pb::TimeSeriesType::Deterministic => TimeSeriesType::Deterministic,
            pb::TimeSeriesType::DeterministicSingleTimeSeries => {
                TimeSeriesType::DeterministicSingleTimeSeries
            }
            pb::TimeSeriesType::Probabilistic => TimeSeriesType::Probabilistic,
            pb::TimeSeriesType::Scenarios => TimeSeriesType::Scenarios,
        }
    }
}

impl From<OwnerCategory> for pb::OwnerCategory {
    fn from(value: OwnerCategory) -> Self {
        match value {
            OwnerCategory::Component => pb::OwnerCategory::Component,
            OwnerCategory::SupplementalAttribute => pb::OwnerCategory::SupplementalAttribute,
        }
    }
}

impl From<pb::OwnerCategory> for OwnerCategory {
    fn from(value: pb::OwnerCategory) -> Self {
        match value {
            pb::OwnerCategory::Component => OwnerCategory::Component,
            pb::OwnerCategory::SupplementalAttribute => OwnerCategory::SupplementalAttribute,
        }
    }
}

// ---- Features ----

impl From<&FeatureValue> for pb::FeatureValue {
    fn from(value: &FeatureValue) -> Self {
        let v = match value {
            FeatureValue::Int(i) => pb::feature_value::Value::IntValue(*i),
            FeatureValue::Float(f) => pb::feature_value::Value::FloatValue(*f),
            FeatureValue::Bool(b) => pb::feature_value::Value::BoolValue(*b),
            FeatureValue::Str(s) => pb::feature_value::Value::StrValue(s.clone()),
        };
        pb::FeatureValue { value: Some(v) }
    }
}

impl TryFrom<pb::FeatureValue> for FeatureValue {
    type Error = ConvertError;
    fn try_from(value: pb::FeatureValue) -> Result<Self, Self::Error> {
        match value.value {
            Some(pb::feature_value::Value::IntValue(i)) => Ok(FeatureValue::Int(i)),
            Some(pb::feature_value::Value::FloatValue(f)) => Ok(FeatureValue::Float(f)),
            Some(pb::feature_value::Value::BoolValue(b)) => Ok(FeatureValue::Bool(b)),
            Some(pb::feature_value::Value::StrValue(s)) => Ok(FeatureValue::Str(s)),
            None => Err(ConvertError::MissingField("FeatureValue.value")),
        }
    }
}

pub fn features_to_pb(f: &Features) -> pb::Features {
    let entries = f
        .iter()
        .map(|(k, v)| (k.clone(), pb::FeatureValue::from(v)))
        .collect();
    pb::Features { entries }
}

pub fn features_from_pb(f: pb::Features) -> Result<Features, ConvertError> {
    let mut out: Features = BTreeMap::new();
    for (k, v) in f.entries {
        out.insert(k, FeatureValue::try_from(v)?);
    }
    Ok(out)
}

// ---- Key + metadata ----

pub fn key_to_pb(k: &TimeSeriesKey) -> pb::TimeSeriesKey {
    pb::TimeSeriesKey {
        owner_uuid: k.owner_uuid.clone(),
        time_series_type: pb::TimeSeriesType::from(k.time_series_type) as i32,
        name: k.name.clone(),
        resolution_ns: k.resolution.map(duration_to_ns).unwrap_or(0),
        features: Some(features_to_pb(&k.features)),
    }
}

pub fn key_from_pb(k: pb::TimeSeriesKey) -> Result<TimeSeriesKey, ConvertError> {
    let ts_type = pb::TimeSeriesType::try_from(k.time_series_type).map_err(|_| {
        ConvertError::InvalidValue {
            field: "time_series_type",
            message: format!("unknown enum value {}", k.time_series_type),
        }
    })?;
    let resolution = if k.resolution_ns == 0 {
        None
    } else {
        Some(Duration::nanoseconds(k.resolution_ns))
    };
    let features = match k.features {
        Some(f) => features_from_pb(f)?,
        None => Features::new(),
    };
    Ok(TimeSeriesKey {
        owner_uuid: k.owner_uuid,
        time_series_type: TimeSeriesType::from(ts_type),
        name: k.name,
        resolution,
        features,
    })
}

pub fn metadata_to_pb(m: &TimeSeriesMetadata) -> pb::TimeSeriesMetadata {
    pb::TimeSeriesMetadata {
        owner_uuid: m.owner_uuid.clone(),
        owner_type: m.owner_type.clone(),
        owner_category: pb::OwnerCategory::from(m.owner_category) as i32,
        time_series_type: pb::TimeSeriesType::from(m.time_series_type) as i32,
        name: m.name.clone(),
        data_hash: m.data_hash.to_vec(),
        initial_timestamp_rfc3339: m
            .initial_timestamp
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
        resolution_ns: m.resolution.map(duration_to_ns).unwrap_or(0),
        length: m.length.unwrap_or(0) as u64,
        horizon_ns: m.horizon.map(duration_to_ns).unwrap_or(0),
        interval_ns: m.interval.map(duration_to_ns).unwrap_or(0),
        count: m.count.unwrap_or(0) as u64,
        timestamps_rfc3339: m
            .timestamps
            .as_ref()
            .map(|ts| ts.iter().map(|t| t.to_rfc3339()).collect())
            .unwrap_or_default(),
        features: Some(features_to_pb(&m.features)),
        scaling_factor_multiplier: m.scaling_factor_multiplier.clone().unwrap_or_default(),
        units: m.units.clone().unwrap_or_default(),
        dtype: m.dtype.code(),
        element_shape: m.element_shape.iter().map(|d| *d as u64).collect(),
        logical_type: m.logical_type.clone().unwrap_or_default(),
        percentiles: m.percentiles.clone().unwrap_or_default(),
    }
}

pub fn metadata_from_pb(m: pb::TimeSeriesMetadata) -> Result<TimeSeriesMetadata, ConvertError> {
    let owner_category =
        pb::OwnerCategory::try_from(m.owner_category).map_err(|_| ConvertError::InvalidValue {
            field: "owner_category",
            message: format!("unknown enum value {}", m.owner_category),
        })?;
    let ts_type = pb::TimeSeriesType::try_from(m.time_series_type).map_err(|_| {
        ConvertError::InvalidValue {
            field: "time_series_type",
            message: format!("unknown enum value {}", m.time_series_type),
        }
    })?;
    if m.data_hash.len() != 32 {
        return Err(ConvertError::BadHashLen(m.data_hash.len()));
    }
    let mut data_hash = [0u8; 32];
    data_hash.copy_from_slice(&m.data_hash);

    let initial_timestamp = parse_optional_rfc3339(&m.initial_timestamp_rfc3339)?;
    let timestamps = if m.timestamps_rfc3339.is_empty() {
        None
    } else {
        Some(
            m.timestamps_rfc3339
                .iter()
                .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
                .collect::<Result<Vec<_>, _>>()?,
        )
    };
    let features = match m.features {
        Some(f) => features_from_pb(f)?,
        None => Features::new(),
    };

    Ok(TimeSeriesMetadata {
        owner_uuid: m.owner_uuid,
        owner_type: m.owner_type,
        owner_category: OwnerCategory::from(owner_category),
        time_series_type: TimeSeriesType::from(ts_type),
        name: m.name,
        data_hash,
        initial_timestamp,
        resolution: optional_ns(m.resolution_ns),
        length: optional_usize(m.length),
        horizon: optional_ns(m.horizon_ns),
        interval: optional_ns(m.interval_ns),
        count: optional_usize(m.count),
        timestamps,
        features,
        scaling_factor_multiplier: optional_string(m.scaling_factor_multiplier),
        units: optional_string(m.units),
        percentiles: if m.percentiles.is_empty() {
            None
        } else {
            Some(m.percentiles)
        },
        dtype: Dtype::from_code(m.dtype).unwrap_or(Dtype::F64),
        element_shape: m.element_shape.iter().map(|d| *d as usize).collect(),
        logical_type: optional_string(m.logical_type),
    })
}

// ---- TimeSeriesData (for GetResp body construction) ----

/// Encode a [`TimeSeriesData`] into the wire-shape used by `GetResp`.
pub fn time_series_data_to_get_resp(data: &TimeSeriesData) -> pb::GetResp {
    match data {
        TimeSeriesData::SingleTimeSeries(s) => pb::GetResp {
            initial_timestamp_rfc3339: s.initial_timestamp.to_rfc3339(),
            resolution_ns: duration_to_ns(s.resolution),
            length: s.length as u64,
            shape: s.data.shape.iter().map(|d| *d as u64).collect(),
            dtype: s.data.dtype.code(),
            value_bytes: s.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::SingleTimeSeries as i32,
            timestamps_rfc3339: Vec::new(),
            logical_type: String::new(),
            horizon_ns: 0,
            interval_ns: 0,
            count: 0,
            percentiles: Vec::new(),
            scenario_count: 0,
        },
        TimeSeriesData::NonSequentialTimeSeries(s) => pb::GetResp {
            initial_timestamp_rfc3339: String::new(),
            resolution_ns: 0,
            length: s.length as u64,
            shape: s.data.shape.iter().map(|d| *d as u64).collect(),
            dtype: s.data.dtype.code(),
            value_bytes: s.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::NonSequentialTimeSeries as i32,
            timestamps_rfc3339: s.timestamps.iter().map(|t| t.to_rfc3339()).collect(),
            logical_type: String::new(),
            horizon_ns: 0,
            interval_ns: 0,
            count: 0,
            percentiles: Vec::new(),
            scenario_count: 0,
        },
        TimeSeriesData::Deterministic(d) => pb::GetResp {
            initial_timestamp_rfc3339: d.initial_timestamp.to_rfc3339(),
            resolution_ns: duration_to_ns(d.resolution),
            length: d.data.shape[0] as u64,
            shape: d.data.shape.iter().map(|x| *x as u64).collect(),
            dtype: d.data.dtype.code(),
            value_bytes: d.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::Deterministic as i32,
            timestamps_rfc3339: Vec::new(),
            logical_type: String::new(),
            horizon_ns: duration_to_ns(d.horizon),
            interval_ns: duration_to_ns(d.interval),
            count: d.count as u64,
            percentiles: Vec::new(),
            scenario_count: 0,
        },
        TimeSeriesData::Probabilistic(p) => pb::GetResp {
            initial_timestamp_rfc3339: p.initial_timestamp.to_rfc3339(),
            resolution_ns: duration_to_ns(p.resolution),
            length: p.data.shape[0] as u64,
            shape: p.data.shape.iter().map(|x| *x as u64).collect(),
            dtype: p.data.dtype.code(),
            value_bytes: p.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::Probabilistic as i32,
            timestamps_rfc3339: Vec::new(),
            logical_type: String::new(),
            horizon_ns: duration_to_ns(p.horizon),
            interval_ns: duration_to_ns(p.interval),
            count: p.count as u64,
            percentiles: p.percentiles.clone(),
            scenario_count: 0,
        },
        TimeSeriesData::Scenarios(s) => pb::GetResp {
            initial_timestamp_rfc3339: s.initial_timestamp.to_rfc3339(),
            resolution_ns: duration_to_ns(s.resolution),
            length: s.data.shape[0] as u64,
            shape: s.data.shape.iter().map(|x| *x as u64).collect(),
            dtype: s.data.dtype.code(),
            value_bytes: s.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::Scenarios as i32,
            timestamps_rfc3339: Vec::new(),
            logical_type: String::new(),
            horizon_ns: duration_to_ns(s.horizon),
            interval_ns: duration_to_ns(s.interval),
            count: s.count as u64,
            percentiles: Vec::new(),
            scenario_count: s.scenario_count as u64,
        },
    }
}

pub fn get_resp_to_time_series_data(resp: pb::GetResp) -> Result<TimeSeriesData, ConvertError> {
    let ts_type = pb::TimeSeriesType::try_from(resp.time_series_type).map_err(|_| {
        ConvertError::InvalidValue {
            field: "time_series_type",
            message: format!("unknown enum value {}", resp.time_series_type),
        }
    })?;
    let shape: Vec<usize> = resp.shape.iter().map(|d| *d as usize).collect();
    let dtype = Dtype::from_code(resp.dtype).ok_or(ConvertError::InvalidValue {
        field: "dtype",
        message: format!("unknown dtype code {}", resp.dtype),
    })?;
    let data = TypedArray::new(dtype, shape, resp.value_bytes).map_err(|e| {
        ConvertError::InvalidValue {
            field: "value_bytes",
            message: e,
        }
    })?;
    match ts_type {
        pb::TimeSeriesType::SingleTimeSeries => {
            let initial_timestamp = DateTime::parse_from_rfc3339(&resp.initial_timestamp_rfc3339)
                .map(|d| d.with_timezone(&Utc))?;
            Ok(TimeSeriesData::SingleTimeSeries(SingleTimeSeries {
                initial_timestamp,
                resolution: Duration::nanoseconds(resp.resolution_ns),
                length: resp.length as usize,
                data,
            }))
        }
        pb::TimeSeriesType::NonSequentialTimeSeries => {
            let timestamps = resp
                .timestamps_rfc3339
                .iter()
                .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
                .collect::<Result<Vec<_>, _>>()?;
            let series = NonSequentialTimeSeries::new(timestamps, data).map_err(|message| {
                ConvertError::InvalidValue {
                    field: "timestamps_rfc3339",
                    message,
                }
            })?;
            Ok(TimeSeriesData::NonSequentialTimeSeries(series))
        }
        pb::TimeSeriesType::Deterministic | pb::TimeSeriesType::DeterministicSingleTimeSeries => {
            let initial_timestamp = DateTime::parse_from_rfc3339(&resp.initial_timestamp_rfc3339)
                .map(|d| d.with_timezone(&Utc))?;
            let det = Deterministic::new(
                initial_timestamp,
                Duration::nanoseconds(resp.resolution_ns),
                Duration::nanoseconds(resp.horizon_ns),
                Duration::nanoseconds(resp.interval_ns),
                resp.count as usize,
                data,
            )
            .map_err(|message| ConvertError::InvalidValue {
                field: "Deterministic",
                message,
            })?;
            Ok(TimeSeriesData::Deterministic(det))
        }
        pb::TimeSeriesType::Probabilistic => {
            let initial_timestamp = DateTime::parse_from_rfc3339(&resp.initial_timestamp_rfc3339)
                .map(|d| d.with_timezone(&Utc))?;
            let prob = Probabilistic::new(
                initial_timestamp,
                Duration::nanoseconds(resp.resolution_ns),
                Duration::nanoseconds(resp.horizon_ns),
                Duration::nanoseconds(resp.interval_ns),
                resp.count as usize,
                resp.percentiles,
                data,
            )
            .map_err(|message| ConvertError::InvalidValue {
                field: "Probabilistic",
                message,
            })?;
            Ok(TimeSeriesData::Probabilistic(prob))
        }
        pb::TimeSeriesType::Scenarios => {
            let initial_timestamp = DateTime::parse_from_rfc3339(&resp.initial_timestamp_rfc3339)
                .map(|d| d.with_timezone(&Utc))?;
            let scen = Scenarios::new(
                initial_timestamp,
                Duration::nanoseconds(resp.resolution_ns),
                Duration::nanoseconds(resp.horizon_ns),
                Duration::nanoseconds(resp.interval_ns),
                resp.count as usize,
                resp.scenario_count as usize,
                data,
            )
            .map_err(|message| ConvertError::InvalidValue {
                field: "Scenarios",
                message,
            })?;
            Ok(TimeSeriesData::Scenarios(scen))
        }
    }
}

// ---- Helpers ----

fn duration_to_ns(d: Duration) -> i64 {
    d.num_nanoseconds()
        .unwrap_or_else(|| d.num_seconds() * 1_000_000_000)
}

fn optional_ns(ns: i64) -> Option<Duration> {
    if ns == 0 {
        None
    } else {
        Some(Duration::nanoseconds(ns))
    }
}

fn optional_usize(v: u64) -> Option<usize> {
    if v == 0 { None } else { Some(v as usize) }
}

fn optional_string(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn parse_optional_rfc3339(s: &str) -> Result<Option<DateTime<Utc>>, ConvertError> {
    if s.is_empty() {
        Ok(None)
    } else {
        Ok(Some(
            DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use time_series_store_core::{Dtype, TypedArray};

    fn make_ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
    }

    /// Build an f64 TypedArray with sequential values starting from `base`.
    fn seq_f64(shape: Vec<usize>, base: f64) -> TypedArray {
        let n: usize = shape.iter().product();
        let vals: Vec<f64> = (0..n).map(|i| base + i as f64).collect();
        TypedArray::from_f64(shape, &vals)
    }

    /// Build an i64 TypedArray with sequential values starting from `base`.
    fn seq_i64(shape: Vec<usize>, base: i64) -> TypedArray {
        let n: usize = shape.iter().product();
        let mut bytes = Vec::with_capacity(n * 8);
        for i in 0..n {
            let v: i64 = base + i as i64;
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        TypedArray::new(Dtype::I64, shape, bytes).unwrap()
    }

    // ---- Deterministic ----

    #[test]
    fn deterministic_scalar_round_trip() {
        // shape [H=4, count=3]
        let data = seq_f64(vec![4, 3], 1.0);
        let ts = make_ts();
        let det = Deterministic::new(
            ts,
            Duration::hours(1),
            Duration::hours(4),
            Duration::hours(6),
            3,
            data,
        )
        .unwrap();
        let original = TimeSeriesData::Deterministic(det);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(
            resp.time_series_type,
            pb::TimeSeriesType::Deterministic as i32
        );
        assert_eq!(resp.count, 3);
        assert_eq!(
            resp.horizon_ns,
            Duration::hours(4).num_nanoseconds().unwrap()
        );
        assert_eq!(
            resp.interval_ns,
            Duration::hours(6).num_nanoseconds().unwrap()
        );
        assert_eq!(resp.length, 4); // shape[0]
        assert!(resp.percentiles.is_empty());
        assert_eq!(resp.scenario_count, 0);

        let roundtripped = get_resp_to_time_series_data(resp).unwrap();
        assert_eq!(roundtripped, original);
    }

    #[test]
    fn deterministic_multidim_round_trip() {
        // shape [H=2, count=3, elem=2] — multidim element shape
        let data = seq_f64(vec![2, 3, 2], 10.0);
        let ts = make_ts();
        let det = Deterministic::new(
            ts,
            Duration::minutes(30),
            Duration::hours(1),
            Duration::hours(2),
            3,
            data,
        )
        .unwrap();
        let original = TimeSeriesData::Deterministic(det);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.shape, vec![2u64, 3, 2]);
        assert_eq!(resp.count, 3);

        let roundtripped = get_resp_to_time_series_data(resp).unwrap();
        assert_eq!(roundtripped, original);
        let d = roundtripped.as_deterministic().unwrap();
        assert_eq!(d.data.shape, vec![2, 3, 2]);
        assert_eq!(d.data.to_f64_vec().unwrap()[0], 10.0);
    }

    // ---- Probabilistic ----

    #[test]
    fn probabilistic_scalar_round_trip() {
        // shape [P=3, H=4, count=2]
        let data = seq_f64(vec![3, 4, 2], 0.0);
        let ts = make_ts();
        let percentiles = vec![10.0, 50.0, 90.0];
        let prob = Probabilistic::new(
            ts,
            Duration::hours(1),
            Duration::hours(4),
            Duration::hours(6),
            2,
            percentiles.clone(),
            data,
        )
        .unwrap();
        let original = TimeSeriesData::Probabilistic(prob);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(
            resp.time_series_type,
            pb::TimeSeriesType::Probabilistic as i32
        );
        assert_eq!(resp.percentiles, percentiles);
        assert_eq!(resp.count, 2);
        assert_eq!(resp.scenario_count, 0);
        assert_eq!(resp.length, 3); // shape[0] = num_percentiles

        let roundtripped = get_resp_to_time_series_data(resp).unwrap();
        assert_eq!(roundtripped, original);
        let p = roundtripped.as_probabilistic().unwrap();
        assert_eq!(p.percentiles, vec![10.0, 50.0, 90.0]);
    }

    #[test]
    fn probabilistic_multidim_round_trip() {
        // shape [P=2, H=3, count=4, elem=5] — multidim element shape
        let data = seq_f64(vec![2, 3, 4, 5], 100.0);
        let ts = make_ts();
        let percentiles = vec![25.0, 75.0];
        let prob = Probabilistic::new(
            ts,
            Duration::hours(1),
            Duration::hours(3),
            Duration::hours(4),
            4,
            percentiles.clone(),
            data,
        )
        .unwrap();
        let original = TimeSeriesData::Probabilistic(prob);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.shape, vec![2u64, 3, 4, 5]);
        assert_eq!(resp.percentiles, percentiles);

        let roundtripped = get_resp_to_time_series_data(resp).unwrap();
        assert_eq!(roundtripped, original);
    }

    // ---- Scenarios ----

    #[test]
    fn scenarios_scalar_round_trip() {
        // shape [S=4, H=6, count=3]
        let data = seq_f64(vec![4, 6, 3], 5.0);
        let ts = make_ts();
        let scen = Scenarios::new(
            ts,
            Duration::hours(1),
            Duration::hours(6),
            Duration::hours(8),
            3,
            4,
            data,
        )
        .unwrap();
        let original = TimeSeriesData::Scenarios(scen);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.time_series_type, pb::TimeSeriesType::Scenarios as i32);
        assert_eq!(resp.scenario_count, 4);
        assert_eq!(resp.count, 3);
        assert!(resp.percentiles.is_empty());
        assert_eq!(resp.length, 4); // shape[0] = scenario_count

        let roundtripped = get_resp_to_time_series_data(resp).unwrap();
        assert_eq!(roundtripped, original);
        let s = roundtripped.as_scenarios().unwrap();
        assert_eq!(s.scenario_count, 4);
    }

    #[test]
    fn scenarios_multidim_round_trip() {
        // shape [S=2, H=4, count=3, elem=2] — multidim element shape
        let data = seq_f64(vec![2, 4, 3, 2], 0.0);
        let ts = make_ts();
        let scen = Scenarios::new(
            ts,
            Duration::hours(1),
            Duration::hours(4),
            Duration::hours(6),
            3,
            2,
            data,
        )
        .unwrap();
        let original = TimeSeriesData::Scenarios(scen);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.shape, vec![2u64, 4, 3, 2]);
        assert_eq!(resp.scenario_count, 2);

        let roundtripped = get_resp_to_time_series_data(resp).unwrap();
        assert_eq!(roundtripped, original);
    }

    // ---- Non-f64 dtype survives round trip ----

    #[test]
    fn scenarios_i64_dtype_round_trip() {
        // shape [S=2, H=3, count=2] with i64 dtype
        let data = seq_i64(vec![2, 3, 2], 100);
        let ts = make_ts();
        let scen = Scenarios::new(
            ts,
            Duration::hours(1),
            Duration::hours(3),
            Duration::hours(4),
            2,
            2,
            data,
        )
        .unwrap();
        let original = TimeSeriesData::Scenarios(scen);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.dtype, Dtype::I64.code());

        let roundtripped = get_resp_to_time_series_data(resp).unwrap();
        assert_eq!(roundtripped, original);
        let s = roundtripped.as_scenarios().unwrap();
        assert_eq!(s.data.dtype, Dtype::I64);
        // Verify first and last i64 values
        let vals: Vec<i64> = s
            .data
            .bytes
            .chunks_exact(8)
            .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(vals[0], 100);
        assert_eq!(vals[11], 111);
    }
}
