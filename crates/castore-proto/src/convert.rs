//! Conversions between generated protobuf types and `castore_core` types.

use std::collections::BTreeMap;

use castore_core::{
    Deterministic, Dtype, FeatureValue, Features, ForecastSummaryRow, ForecastTimeSeriesKey,
    KeyIdentity, NonSequentialTimeSeries, NonSequentialTimeSeriesKey, OwnerCategory, Period,
    Probabilistic, RequestedType, Scenarios, SingleTimeSeries, SingleTimeSeriesKey,
    StaticSummaryRow, TimeSeriesData, TimeSeriesKey, TimeSeriesMetadata, TimeSeriesType,
    TypedArray,
};
use chrono::{DateTime, Utc};

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

// The identity-only key encoding: maps a [`KeyIdentity`] with the descriptive
// snapshot fields left absent. Used by the identity-addressed RPCs (get, has),
// where only the identity tuple is needed to look a series up.
pub fn key_to_pb(k: &KeyIdentity) -> pb::TimeSeriesKey {
    pb::TimeSeriesKey {
        owner_id: k.owner_id,
        owner_category: pb::OwnerCategory::from(k.owner_category) as i32,
        time_series_type: pb::TimeSeriesType::from(k.time_series_type) as i32,
        name: k.name.clone(),
        resolution: period_to_iso(k.resolution),
        interval: period_to_iso(k.interval),
        features: Some(features_to_pb(&k.features)),
        initial_timestamp_rfc3339: None,
        length: None,
        horizon: None,
        count: None,
    }
}

// The full-key encoding: the identity plus the per-variant descriptive snapshot
// (the variant is implied by `time_series_type`). Used by the RPCs that return
// keys to a caller (ListKeys, GetTimeSeriesKeys, ResolveForecastKey).
pub fn full_key_to_pb(k: &TimeSeriesKey) -> pb::TimeSeriesKey {
    let mut pb = key_to_pb(k.identity());
    match k {
        TimeSeriesKey::Single(s) => {
            pb.initial_timestamp_rfc3339 = Some(s.initial_timestamp.to_rfc3339());
            pb.length = Some(s.length as u64);
        }
        TimeSeriesKey::NonSequential(s) => {
            pb.length = Some(s.length as u64);
        }
        TimeSeriesKey::Forecast(f) => {
            pb.initial_timestamp_rfc3339 = Some(f.initial_timestamp.to_rfc3339());
            pb.horizon = Some(f.horizon.to_iso8601());
            pb.count = Some(f.count as u64);
        }
    }
    pb
}

// Decode a full key sent by the server back into the core [`TimeSeriesKey`]
// enum, reconstructing the variant from `time_series_type`. The descriptive
// snapshot fields are required for the matched variant (the server always sends
// them via [`full_key_to_pb`]).
pub fn full_key_from_pb(k: pb::TimeSeriesKey) -> Result<TimeSeriesKey, ConvertError> {
    let ts_type_pb = pb::TimeSeriesType::try_from(k.time_series_type).map_err(|_| {
        ConvertError::InvalidValue {
            field: "time_series_type",
            message: format!("unknown enum value {}", k.time_series_type),
        }
    })?;
    let ts_type = TimeSeriesType::from(ts_type_pb);
    let initial_ts = &k.initial_timestamp_rfc3339;
    let horizon = &k.horizon;
    let count = k.count;
    let length = k.length;
    let identity = key_from_pb(k.clone())?;

    let parse_initial = |s: &Option<String>| -> Result<DateTime<Utc>, ConvertError> {
        let s = s.as_deref().ok_or(ConvertError::MissingField(
            "TimeSeriesKey.initial_timestamp_rfc3339",
        ))?;
        Ok(DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc))?)
    };
    let require_length = || length.ok_or(ConvertError::MissingField("TimeSeriesKey.length"));

    match ts_type {
        TimeSeriesType::SingleTimeSeries => Ok(TimeSeriesKey::Single(SingleTimeSeriesKey {
            identity,
            initial_timestamp: parse_initial(initial_ts)?,
            length: require_length()? as usize,
        })),
        TimeSeriesType::NonSequentialTimeSeries => {
            Ok(TimeSeriesKey::NonSequential(NonSequentialTimeSeriesKey {
                identity,
                length: require_length()? as usize,
            }))
        }
        TimeSeriesType::Deterministic
        | TimeSeriesType::DeterministicSingleTimeSeries
        | TimeSeriesType::Probabilistic
        | TimeSeriesType::Scenarios => Ok(TimeSeriesKey::Forecast(ForecastTimeSeriesKey {
            identity,
            initial_timestamp: parse_initial(initial_ts)?,
            horizon: opt_period(horizon.as_deref())?
                .ok_or(ConvertError::MissingField("TimeSeriesKey.horizon"))?,
            count: count.ok_or(ConvertError::MissingField("TimeSeriesKey.count"))? as usize,
        })),
    }
}

pub fn key_from_pb(k: pb::TimeSeriesKey) -> Result<KeyIdentity, ConvertError> {
    let owner_category =
        pb::OwnerCategory::try_from(k.owner_category).map_err(|_| ConvertError::InvalidValue {
            field: "owner_category",
            message: format!("unknown enum value {}", k.owner_category),
        })?;
    let ts_type = pb::TimeSeriesType::try_from(k.time_series_type).map_err(|_| {
        ConvertError::InvalidValue {
            field: "time_series_type",
            message: format!("unknown enum value {}", k.time_series_type),
        }
    })?;
    let resolution = optional_period(&k.resolution)?;
    let interval = optional_period(&k.interval)?;
    let features = match k.features {
        Some(f) => features_from_pb(f)?,
        None => Features::new(),
    };
    Ok(KeyIdentity {
        owner_id: k.owner_id,
        owner_category: OwnerCategory::from(owner_category),
        time_series_type: TimeSeriesType::from(ts_type),
        name: k.name,
        resolution,
        interval,
        features,
    })
}

pub fn metadata_to_pb(m: &TimeSeriesMetadata) -> pb::TimeSeriesMetadata {
    pb::TimeSeriesMetadata {
        owner_id: m.owner_id,
        owner_type: m.owner_type.clone(),
        owner_category: pb::OwnerCategory::from(m.owner_category) as i32,
        time_series_type: pb::TimeSeriesType::from(m.time_series_type) as i32,
        name: m.name.clone(),
        data_hash: m.data_hash.to_vec(),
        initial_timestamp_rfc3339: m.initial_timestamp.map(|t| t.to_rfc3339()),
        resolution: m.resolution.map(|p| p.to_iso8601()),
        length: m.length.map(|l| l as u64),
        horizon: m.horizon.map(|p| p.to_iso8601()),
        interval: m.interval.map(|p| p.to_iso8601()),
        count: m.count.map(|c| c as u64),
        timestamps_rfc3339: m
            .timestamps
            .as_ref()
            .map(|ts| ts.iter().map(|t| t.to_rfc3339()).collect())
            .unwrap_or_default(),
        features: Some(features_to_pb(&m.features)),
        units: m.units.clone(),
        dtype: m.dtype.code(),
        element_shape: m.element_shape.iter().map(|d| *d as u64).collect(),
        ext: m.ext.clone(),
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

    let initial_timestamp = match &m.initial_timestamp_rfc3339 {
        Some(s) => Some(DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc))?),
        None => None,
    };
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
        owner_id: m.owner_id,
        owner_type: m.owner_type,
        owner_category: OwnerCategory::from(owner_category),
        time_series_type: TimeSeriesType::from(ts_type),
        name: m.name,
        data_hash,
        initial_timestamp,
        resolution: opt_period(m.resolution.as_deref())?,
        length: m.length.map(|l| l as usize),
        horizon: opt_period(m.horizon.as_deref())?,
        interval: opt_period(m.interval.as_deref())?,
        count: m.count.map(|c| c as usize),
        timestamps,
        features,
        units: m.units,
        percentiles: if m.percentiles.is_empty() {
            None
        } else {
            Some(m.percentiles)
        },
        // Error on an unknown dtype (matching the data-decode path) rather than
        // silently coercing to F64.
        dtype: Dtype::from_code(m.dtype).ok_or(ConvertError::InvalidValue {
            field: "dtype",
            message: format!("unknown dtype code {}", m.dtype),
        })?,
        element_shape: m.element_shape.iter().map(|d| *d as usize).collect(),
        ext: m.ext,
    })
}

// ---- TimeSeriesData (for GetResp body construction) ----

/// Encode a [`TimeSeriesData`] into the wire-shape used by `GetResp`.
///
/// `GetResp.ext` is left empty on every variant, and that is not an omission:
/// `ext` belongs to the association row ([`TimeSeriesMetadata::ext`], which
/// [`metadata_to_pb`] does carry), not to the time-series values, so a
/// `TimeSeriesData` has no `ext` for this function to forward. A gRPC caller
/// reads `ext` from `GetMetadata` or `ListTimeSeries`. Pinned by
/// `ext_is_always_empty_in_get_resp`; see the proto comment on field 10.
pub fn time_series_data_to_get_resp(data: &TimeSeriesData) -> pb::GetResp {
    match data {
        TimeSeriesData::SingleTimeSeries(s) => pb::GetResp {
            initial_timestamp_rfc3339: s.initial_timestamp.to_rfc3339(),
            resolution: s.resolution.to_iso8601(),
            length: s.length as u64,
            shape: s.data.shape.iter().map(|d| *d as u64).collect(),
            dtype: s.data.dtype.code(),
            value_bytes: s.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::SingleTimeSeries as i32,
            timestamps_rfc3339: Vec::new(),
            ext: String::new(),
            horizon: String::new(),
            interval: String::new(),
            count: 0,
            percentiles: Vec::new(),
            scenario_count: 0,
        },
        TimeSeriesData::NonSequentialTimeSeries(s) => pb::GetResp {
            initial_timestamp_rfc3339: String::new(),
            resolution: String::new(),
            length: s.length as u64,
            shape: s.data.shape.iter().map(|d| *d as u64).collect(),
            dtype: s.data.dtype.code(),
            value_bytes: s.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::NonSequentialTimeSeries as i32,
            timestamps_rfc3339: s.timestamps.iter().map(|t| t.to_rfc3339()).collect(),
            ext: String::new(),
            horizon: String::new(),
            interval: String::new(),
            count: 0,
            percentiles: Vec::new(),
            scenario_count: 0,
        },
        TimeSeriesData::Deterministic(d) => pb::GetResp {
            initial_timestamp_rfc3339: d.initial_timestamp.to_rfc3339(),
            resolution: d.resolution.to_iso8601(),
            length: d.data.shape[0] as u64,
            shape: d.data.shape.iter().map(|x| *x as u64).collect(),
            dtype: d.data.dtype.code(),
            value_bytes: d.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::Deterministic as i32,
            timestamps_rfc3339: Vec::new(),
            ext: String::new(),
            horizon: d.horizon.to_iso8601(),
            interval: d.interval.to_iso8601(),
            count: d.count as u64,
            percentiles: Vec::new(),
            scenario_count: 0,
        },
        TimeSeriesData::Probabilistic(p) => pb::GetResp {
            initial_timestamp_rfc3339: p.initial_timestamp.to_rfc3339(),
            resolution: p.resolution.to_iso8601(),
            length: p.data.shape[0] as u64,
            shape: p.data.shape.iter().map(|x| *x as u64).collect(),
            dtype: p.data.dtype.code(),
            value_bytes: p.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::Probabilistic as i32,
            timestamps_rfc3339: Vec::new(),
            ext: String::new(),
            horizon: p.horizon.to_iso8601(),
            interval: p.interval.to_iso8601(),
            count: p.count as u64,
            percentiles: p.percentiles.clone(),
            scenario_count: 0,
        },
        TimeSeriesData::Scenarios(s) => pb::GetResp {
            initial_timestamp_rfc3339: s.initial_timestamp.to_rfc3339(),
            resolution: s.resolution.to_iso8601(),
            length: s.data.shape[0] as u64,
            shape: s.data.shape.iter().map(|x| *x as u64).collect(),
            dtype: s.data.dtype.code(),
            value_bytes: s.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::Scenarios as i32,
            timestamps_rfc3339: Vec::new(),
            ext: String::new(),
            horizon: s.horizon.to_iso8601(),
            interval: s.interval.to_iso8601(),
            count: s.count as u64,
            percentiles: Vec::new(),
            scenario_count: s.scenario_count as u64,
        },
    }
}

pub fn get_resp_to_time_series_data(
    resp: pb::GetResp,
    name: String,
) -> Result<TimeSeriesData, ConvertError> {
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
                resolution: period_from_iso(&resp.resolution)?,
                length: resp.length as usize,
                data,
                name,
            }))
        }
        pb::TimeSeriesType::NonSequentialTimeSeries => {
            let timestamps = resp
                .timestamps_rfc3339
                .iter()
                .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
                .collect::<Result<Vec<_>, _>>()?;
            let series =
                NonSequentialTimeSeries::new(timestamps, data, name).map_err(|message| {
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
                period_from_iso(&resp.resolution)?,
                period_from_iso(&resp.horizon)?,
                period_from_iso(&resp.interval)?,
                resp.count as usize,
                data,
                name,
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
                period_from_iso(&resp.resolution)?,
                period_from_iso(&resp.horizon)?,
                period_from_iso(&resp.interval)?,
                resp.count as usize,
                resp.percentiles,
                data,
                name,
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
                period_from_iso(&resp.resolution)?,
                period_from_iso(&resp.horizon)?,
                period_from_iso(&resp.interval)?,
                resp.count as usize,
                resp.scenario_count as usize,
                data,
                name,
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

/// Encode an optional period as its ISO-8601 string; `None` -> empty string.
fn period_to_iso(p: Option<Period>) -> String {
    p.map(|p| p.to_iso8601()).unwrap_or_default()
}

/// Decode an ISO-8601 period string; empty -> `None`. Used by the key message,
/// whose resolution/interval remain empty-string-sentinel `string` fields.
fn optional_period(s: &str) -> Result<Option<Period>, ConvertError> {
    if s.is_empty() {
        Ok(None)
    } else {
        Period::from_iso8601(s)
            .map(Some)
            .map_err(|e| ConvertError::InvalidValue {
                field: "period",
                message: e.to_string(),
            })
    }
}

/// Decode an optional ISO-8601 period from a proto3 `optional string` field.
fn opt_period(s: Option<&str>) -> Result<Option<Period>, ConvertError> {
    match s {
        Some(s) => Period::from_iso8601(s)
            .map(Some)
            .map_err(|e| ConvertError::InvalidValue {
                field: "period",
                message: e.to_string(),
            }),
        None => Ok(None),
    }
}

/// Decode a required ISO-8601 period string.
fn period_from_iso(s: &str) -> Result<Period, ConvertError> {
    Period::from_iso8601(s).map_err(|e| ConvertError::InvalidValue {
        field: "period",
        message: e.to_string(),
    })
}

// ---- Summary rows + requested type ----

pub fn static_summary_row_to_pb(r: &StaticSummaryRow) -> pb::StaticSummaryRow {
    pb::StaticSummaryRow {
        owner_type: r.owner_type.clone(),
        owner_category: pb::OwnerCategory::from(r.owner_category) as i32,
        time_series_type: pb::TimeSeriesType::from(r.time_series_type) as i32,
        name: r.name.clone(),
        initial_timestamp_rfc3339: r.initial_timestamp.map(|t| t.to_rfc3339()),
        resolution: r.resolution.map(|p| p.to_iso8601()),
        time_step_count: r.time_step_count,
        count: r.count,
    }
}

pub fn static_summary_row_from_pb(
    r: pb::StaticSummaryRow,
) -> Result<StaticSummaryRow, ConvertError> {
    Ok(StaticSummaryRow {
        owner_type: r.owner_type,
        owner_category: owner_category_from_i32(r.owner_category)?,
        time_series_type: ts_type_from_i32(r.time_series_type)?,
        name: r.name,
        initial_timestamp: parse_opt_rfc3339(r.initial_timestamp_rfc3339.as_deref())?,
        resolution: opt_period(r.resolution.as_deref())?,
        time_step_count: r.time_step_count,
        count: r.count,
    })
}

pub fn forecast_summary_row_to_pb(r: &ForecastSummaryRow) -> pb::ForecastSummaryRow {
    pb::ForecastSummaryRow {
        owner_type: r.owner_type.clone(),
        owner_category: pb::OwnerCategory::from(r.owner_category) as i32,
        time_series_type: pb::TimeSeriesType::from(r.time_series_type) as i32,
        name: r.name.clone(),
        initial_timestamp_rfc3339: r.initial_timestamp.map(|t| t.to_rfc3339()),
        resolution: r.resolution.map(|p| p.to_iso8601()),
        horizon: r.horizon.map(|p| p.to_iso8601()),
        interval: r.interval.map(|p| p.to_iso8601()),
        window_count: r.window_count,
        count: r.count,
    }
}

pub fn forecast_summary_row_from_pb(
    r: pb::ForecastSummaryRow,
) -> Result<ForecastSummaryRow, ConvertError> {
    Ok(ForecastSummaryRow {
        owner_type: r.owner_type,
        owner_category: owner_category_from_i32(r.owner_category)?,
        time_series_type: ts_type_from_i32(r.time_series_type)?,
        name: r.name,
        initial_timestamp: parse_opt_rfc3339(r.initial_timestamp_rfc3339.as_deref())?,
        resolution: opt_period(r.resolution.as_deref())?,
        horizon: opt_period(r.horizon.as_deref())?,
        interval: opt_period(r.interval.as_deref())?,
        window_count: r.window_count,
        count: r.count,
    })
}

/// Decode a [`RequestedType`] from its proto oneof.
pub fn requested_type_from_pb(r: pb::RequestedType) -> Result<RequestedType, ConvertError> {
    match r.kind {
        Some(pb::requested_type::Kind::Concrete(code)) => {
            Ok(RequestedType::Concrete(ts_type_from_i32(code)?))
        }
        Some(pb::requested_type::Kind::AbstractDeterministic(_)) => {
            Ok(RequestedType::AbstractDeterministic)
        }
        None => Err(ConvertError::MissingField("RequestedType.kind")),
    }
}

/// Encode a [`RequestedType`] into its proto oneof.
pub fn requested_type_to_pb(r: RequestedType) -> pb::RequestedType {
    let kind = match r {
        RequestedType::Concrete(t) => {
            pb::requested_type::Kind::Concrete(pb::TimeSeriesType::from(t) as i32)
        }
        RequestedType::AbstractDeterministic => {
            pb::requested_type::Kind::AbstractDeterministic(true)
        }
    };
    pb::RequestedType { kind: Some(kind) }
}

fn owner_category_from_i32(v: i32) -> Result<OwnerCategory, ConvertError> {
    pb::OwnerCategory::try_from(v)
        .map(OwnerCategory::from)
        .map_err(|_| ConvertError::InvalidValue {
            field: "owner_category",
            message: format!("unknown enum value {v}"),
        })
}

fn ts_type_from_i32(v: i32) -> Result<TimeSeriesType, ConvertError> {
    pb::TimeSeriesType::try_from(v)
        .map(TimeSeriesType::from)
        .map_err(|_| ConvertError::InvalidValue {
            field: "time_series_type",
            message: format!("unknown enum value {v}"),
        })
}

fn parse_opt_rfc3339(s: Option<&str>) -> Result<Option<DateTime<Utc>>, ConvertError> {
    match s {
        Some(s) => Ok(Some(
            DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc))?,
        )),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castore_core::{Dtype, TypedArray};
    use chrono::{Duration, TimeZone};

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

    // ---- Metadata wire-fidelity (proto3 optional) ----

    fn base_pb_metadata() -> pb::TimeSeriesMetadata {
        pb::TimeSeriesMetadata {
            owner_id: 1,
            owner_type: "Generator".into(),
            owner_category: pb::OwnerCategory::Component as i32,
            time_series_type: pb::TimeSeriesType::SingleTimeSeries as i32,
            name: "load".into(),
            data_hash: vec![0u8; 32],
            initial_timestamp_rfc3339: Some(make_ts().to_rfc3339()),
            resolution: Some("PT1H".into()),
            length: Some(0),
            horizon: None,
            interval: None,
            count: None,
            timestamps_rfc3339: Vec::new(),
            features: Some(pb::Features::default()),
            units: None,
            dtype: Dtype::F64.code(),
            element_shape: Vec::new(),
            ext: None,
            percentiles: Vec::new(),
        }
    }

    #[test]
    fn length_zero_decodes_as_present() {
        // A genuine length == 0 must decode as Some(0), not None (the old
        // sentinel encoding coerced 0 to absent).
        let m = metadata_from_pb(base_pb_metadata()).unwrap();
        assert_eq!(m.length, Some(0));
        // And it round-trips back to Some(0) on the wire.
        let pb = metadata_to_pb(&m);
        assert_eq!(pb.length, Some(0));
    }

    #[test]
    fn unknown_dtype_in_metadata_errors() {
        let mut pb = base_pb_metadata();
        pb.dtype = 99; // not a valid Dtype code
        let err = metadata_from_pb(pb).unwrap_err();
        assert!(matches!(
            err,
            ConvertError::InvalidValue { field: "dtype", .. }
        ));
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
            "test",
        )
        .unwrap();
        let original = TimeSeriesData::Deterministic(det);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(
            resp.time_series_type,
            pb::TimeSeriesType::Deterministic as i32
        );
        assert_eq!(resp.count, 3);
        assert_eq!(resp.horizon, "PT4H");
        assert_eq!(resp.interval, "PT6H");
        assert_eq!(resp.length, 4); // shape[0]
        assert!(resp.percentiles.is_empty());
        assert_eq!(resp.scenario_count, 0);

        let roundtripped = get_resp_to_time_series_data(resp, "test".to_string()).unwrap();
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
            "test",
        )
        .unwrap();
        let original = TimeSeriesData::Deterministic(det);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.shape, vec![2u64, 3, 2]);
        assert_eq!(resp.count, 3);

        let roundtripped = get_resp_to_time_series_data(resp, "test".to_string()).unwrap();
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
            "test",
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

        let roundtripped = get_resp_to_time_series_data(resp, "test".to_string()).unwrap();
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
            "test",
        )
        .unwrap();
        let original = TimeSeriesData::Probabilistic(prob);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.shape, vec![2u64, 3, 4, 5]);
        assert_eq!(resp.percentiles, percentiles);

        let roundtripped = get_resp_to_time_series_data(resp, "test".to_string()).unwrap();
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
            "test",
        )
        .unwrap();
        let original = TimeSeriesData::Scenarios(scen);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.time_series_type, pb::TimeSeriesType::Scenarios as i32);
        assert_eq!(resp.scenario_count, 4);
        assert_eq!(resp.count, 3);
        assert!(resp.percentiles.is_empty());
        assert_eq!(resp.length, 4); // shape[0] = scenario_count

        let roundtripped = get_resp_to_time_series_data(resp, "test".to_string()).unwrap();
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
            "test",
        )
        .unwrap();
        let original = TimeSeriesData::Scenarios(scen);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.shape, vec![2u64, 4, 3, 2]);
        assert_eq!(resp.scenario_count, 2);

        let roundtripped = get_resp_to_time_series_data(resp, "test".to_string()).unwrap();
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
            "test",
        )
        .unwrap();
        let original = TimeSeriesData::Scenarios(scen);
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.dtype, Dtype::I64.code());

        let roundtripped = get_resp_to_time_series_data(resp, "test".to_string()).unwrap();
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

#[cfg(test)]
mod convert_coverage_tests {
    //! Conversion cases the module above does not reach.
    //!
    //! `tests` covers F64/I64 and the `Int` feature variant. The dtype matrix,
    //! three of the four `FeatureValue` variants, the `value == None` arm, and
    //! `Period::Months` anywhere on the wire were untested — and `Months` is the
    //! interesting one, because a `Fixed` period is deliberately never equal to a
    //! `Months` one, so a conversion that lost the distinction would silently
    //! turn a monthly series into a fixed-span one.

    use super::*;
    use castore_core::{Dtype, TypedArray};
    use chrono::{Duration, TimeZone};

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap()
    }

    /// Raw little-endian bytes for `n` elements of `dtype`, with a distinct
    /// pattern per byte so a mis-sized copy shows up as a value change.
    fn pattern_bytes(dtype: Dtype, n: usize) -> Vec<u8> {
        (0..n * dtype.size())
            .map(|i| (i as u8).wrapping_add(1))
            .collect()
    }

    fn typed(dtype: Dtype, shape: Vec<usize>) -> TypedArray {
        let n: usize = shape.iter().product();
        TypedArray::new(dtype, shape, pattern_bytes(dtype, n)).unwrap()
    }

    const ALL_DTYPES: [Dtype; 6] = [
        Dtype::F64,
        Dtype::F32,
        Dtype::I64,
        Dtype::I32,
        Dtype::U64,
        Dtype::Bool,
    ];

    // ---- Dtype matrix ------------------------------------------------------

    #[test]
    fn every_dtype_round_trips_through_get_resp() {
        for dtype in ALL_DTYPES {
            let data = typed(dtype, vec![3]);
            let original = TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                t0(),
                Duration::hours(1),
                data.clone(),
                "load",
            ));
            let resp = time_series_data_to_get_resp(&original);
            assert_eq!(resp.dtype, dtype.code(), "{dtype:?}: code on the wire");
            assert_eq!(resp.value_bytes, data.bytes, "{dtype:?}: bytes");

            let back = get_resp_to_time_series_data(resp, "load".to_string()).unwrap();
            assert_eq!(back, original, "{dtype:?}");
        }
    }

    #[test]
    fn every_dtype_round_trips_through_metadata() {
        for dtype in ALL_DTYPES {
            let meta = TimeSeriesMetadata {
                owner_id: 1,
                owner_type: "Generator".into(),
                owner_category: OwnerCategory::Component,
                time_series_type: TimeSeriesType::SingleTimeSeries,
                name: "load".into(),
                data_hash: [7u8; 32],
                initial_timestamp: Some(t0()),
                resolution: Some(Period::fixed(Duration::hours(1))),
                length: Some(3),
                horizon: None,
                interval: None,
                count: None,
                timestamps: None,
                features: Features::new(),
                units: None,
                percentiles: None,
                dtype,
                element_shape: vec![],
                ext: None,
            };
            let pb = metadata_to_pb(&meta);
            assert_eq!(pb.dtype, dtype.code(), "{dtype:?}");
            assert_eq!(metadata_from_pb(pb).unwrap(), meta, "{dtype:?}");
        }
    }

    #[test]
    fn every_dtype_round_trips_in_a_forecast() {
        for dtype in ALL_DTYPES {
            // shape [H=2, count=3]
            let data = typed(dtype, vec![2, 3]);
            let det = Deterministic::new(
                t0(),
                Duration::hours(1),
                Duration::hours(2),
                Duration::hours(6),
                3,
                data.clone(),
                "det",
            )
            .unwrap();
            let original = TimeSeriesData::Deterministic(det);
            let resp = time_series_data_to_get_resp(&original);
            assert_eq!(resp.dtype, dtype.code(), "{dtype:?}");
            let back = get_resp_to_time_series_data(resp, "det".to_string()).unwrap();
            assert_eq!(back, original, "{dtype:?}");
        }
    }

    #[test]
    fn an_unknown_dtype_code_in_a_get_resp_errors() {
        let data = typed(Dtype::F64, vec![3]);
        let original = TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
            t0(),
            Duration::hours(1),
            data,
            "load",
        ));
        let mut resp = time_series_data_to_get_resp(&original);
        resp.dtype = 99;
        assert!(matches!(
            get_resp_to_time_series_data(resp, "load".to_string()),
            Err(ConvertError::InvalidValue { field: "dtype", .. })
        ));
    }

    // ---- FeatureValue variants --------------------------------------------

    #[test]
    fn every_feature_value_variant_round_trips() {
        let mut features = Features::new();
        features.insert("i".into(), FeatureValue::Int(-42));
        features.insert("f".into(), FeatureValue::Float(1.5));
        features.insert("f_neg_zero".into(), FeatureValue::Float(-0.0));
        features.insert("b_true".into(), FeatureValue::Bool(true));
        features.insert("b_false".into(), FeatureValue::Bool(false));
        features.insert("s".into(), FeatureValue::Str("负荷 'x'".into()));
        features.insert("s_empty".into(), FeatureValue::Str(String::new()));

        let pb = features_to_pb(&features);
        assert_eq!(pb.entries.len(), features.len());
        let back = features_from_pb(pb).unwrap();
        assert_eq!(back, features);

        // -0.0 must keep its sign: `==` would not catch a flattened value.
        let Some(FeatureValue::Float(neg_zero)) = back.get("f_neg_zero") else {
            panic!("expected a Float");
        };
        assert!(neg_zero.is_sign_negative());
    }

    #[test]
    fn each_feature_value_variant_maps_to_its_own_wire_arm() {
        for (value, expect_arm) in [
            (FeatureValue::Int(1), "int"),
            (FeatureValue::Float(1.0), "float"),
            (FeatureValue::Bool(true), "bool"),
            (FeatureValue::Str("x".into()), "str"),
        ] {
            let pb = pb::FeatureValue::from(&value);
            let arm = match pb.value {
                Some(pb::feature_value::Value::IntValue(_)) => "int",
                Some(pb::feature_value::Value::FloatValue(_)) => "float",
                Some(pb::feature_value::Value::BoolValue(_)) => "bool",
                Some(pb::feature_value::Value::StrValue(_)) => "str",
                None => "none",
            };
            assert_eq!(arm, expect_arm, "{value:?}");
            // And back again.
            assert_eq!(
                FeatureValue::try_from(pb::FeatureValue::from(&value)).unwrap(),
                value
            );
        }
    }

    #[test]
    fn a_feature_value_with_no_variant_set_is_a_missing_field() {
        // proto3 leaves a oneof unset when a peer sends a field this build does
        // not know. That must be a clean error, not a defaulted value.
        let err = FeatureValue::try_from(pb::FeatureValue { value: None }).unwrap_err();
        assert!(matches!(
            err,
            ConvertError::MissingField("FeatureValue.value")
        ));

        // The same through the map decoder: one unset entry fails the whole map
        // rather than being dropped.
        let mut entries = std::collections::HashMap::new();
        entries.insert("k".to_string(), pb::FeatureValue { value: None });
        assert!(matches!(
            features_from_pb(pb::Features { entries }),
            Err(ConvertError::MissingField("FeatureValue.value"))
        ));
    }

    // ---- Period::Months across the wire types -----------------------------

    #[test]
    fn months_periods_round_trip_in_metadata() {
        let meta = TimeSeriesMetadata {
            owner_id: 9,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::Deterministic,
            name: "monthly".into(),
            data_hash: [3u8; 32],
            initial_timestamp: Some(t0()),
            resolution: Some(Period::Months(1)),
            length: None,
            horizon: Some(Period::Months(3)),
            interval: Some(Period::Months(1)),
            count: Some(4),
            timestamps: None,
            features: Features::new(),
            units: None,
            percentiles: None,
            dtype: Dtype::F64,
            element_shape: vec![],
            ext: None,
        };

        let pb = metadata_to_pb(&meta);
        assert_eq!(pb.resolution.as_deref(), Some("P1M"));
        assert_eq!(pb.horizon.as_deref(), Some("P3M"));
        assert_eq!(pb.interval.as_deref(), Some("P1M"));

        let back = metadata_from_pb(pb).unwrap();
        assert_eq!(back, meta);
        // The decoded periods are calendar periods, not an equivalent-looking
        // fixed span. `Period` equality is by (kind, magnitude).
        assert_eq!(back.resolution, Some(Period::Months(1)));
        assert!(back.resolution.unwrap().is_irregular());
        assert_ne!(back.resolution, Some(Period::fixed(Duration::days(30))));
    }

    #[test]
    fn a_whole_year_renders_with_y_and_decodes_back_to_months() {
        // `to_iso8601` renders a whole number of years with `Y`; the decode must
        // land back on the same `Months` count.
        let meta = TimeSeriesMetadata {
            owner_id: 1,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::SingleTimeSeries,
            name: "yearly".into(),
            data_hash: [0u8; 32],
            initial_timestamp: Some(t0()),
            resolution: Some(Period::Months(12)),
            length: Some(3),
            horizon: None,
            interval: None,
            count: None,
            timestamps: None,
            features: Features::new(),
            units: None,
            percentiles: None,
            dtype: Dtype::F64,
            element_shape: vec![],
            ext: None,
        };
        let pb = metadata_to_pb(&meta);
        assert_eq!(pb.resolution.as_deref(), Some("P1Y"));
        assert_eq!(
            metadata_from_pb(pb).unwrap().resolution,
            Some(Period::Months(12))
        );
    }

    #[test]
    fn months_periods_round_trip_in_a_key() {
        let identity = KeyIdentity {
            owner_id: 9,
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::Deterministic,
            name: "monthly".into(),
            resolution: Some(Period::Months(1)),
            interval: Some(Period::Months(1)),
            features: Features::new(),
        };
        let key = TimeSeriesKey::Forecast(ForecastTimeSeriesKey {
            identity: identity.clone(),
            initial_timestamp: t0(),
            horizon: Period::Months(3),
            count: 4,
        });

        let pb = full_key_to_pb(&key);
        assert_eq!(pb.resolution, "P1M");
        assert_eq!(pb.interval, "P1M");
        assert_eq!(pb.horizon.as_deref(), Some("P3M"));

        let back = full_key_from_pb(pb).unwrap();
        assert_eq!(back, key);
        assert_eq!(back.identity().resolution, Some(Period::Months(1)));

        // The identity-only encoding too.
        let pb = key_to_pb(&identity);
        assert_eq!(pb.resolution, "P1M");
        assert_eq!(key_from_pb(pb).unwrap(), identity);
    }

    #[test]
    fn the_empty_string_sentinel_decodes_an_absent_period_in_a_key() {
        // A key message keeps `resolution`/`interval` as plain `string` fields
        // with the empty string standing in for "absent". A NonSequential key
        // has neither, so both must come back `None` — and a non-empty value
        // must never decode to `None`.
        let key = TimeSeriesKey::NonSequential(NonSequentialTimeSeriesKey {
            identity: KeyIdentity {
                owner_id: 1,
                owner_category: OwnerCategory::Component,
                time_series_type: TimeSeriesType::NonSequentialTimeSeries,
                name: "events".into(),
                resolution: None,
                interval: None,
                features: Features::new(),
            },
            length: 3,
        });

        let pb = full_key_to_pb(&key);
        assert_eq!(pb.resolution, "", "absent period is the empty string");
        assert_eq!(pb.interval, "");
        let back = full_key_from_pb(pb).unwrap();
        assert_eq!(back, key);
        assert_eq!(back.identity().resolution, None);
        assert_eq!(back.identity().interval, None);
    }

    #[test]
    fn a_malformed_period_string_in_a_key_errors() {
        let mut pb = key_to_pb(&KeyIdentity {
            owner_id: 1,
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::SingleTimeSeries,
            name: "load".into(),
            resolution: Some(Period::fixed(Duration::hours(1))),
            interval: None,
            features: Features::new(),
        });
        pb.resolution = "not-a-period".into();
        assert!(matches!(
            key_from_pb(pb),
            Err(ConvertError::InvalidValue {
                field: "period",
                ..
            })
        ));
    }

    #[test]
    fn months_periods_round_trip_through_get_resp() {
        let data = typed(Dtype::F64, vec![3, 4]);
        let det = Deterministic::new(
            t0(),
            Period::Months(1),
            Period::Months(3),
            Period::Months(1),
            4,
            data,
            "monthly",
        )
        .unwrap();
        let original = TimeSeriesData::Deterministic(det);

        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.resolution, "P1M");
        assert_eq!(resp.horizon, "P3M");
        assert_eq!(resp.interval, "P1M");

        let back = get_resp_to_time_series_data(resp, "monthly".to_string()).unwrap();
        assert_eq!(back, original);
        let det = back.as_deterministic().unwrap();
        assert!(det.resolution.is_irregular());
        assert!(det.horizon.is_irregular());
        assert!(det.interval.is_irregular());
    }

    #[test]
    fn months_periods_round_trip_in_summary_rows() {
        let static_row = StaticSummaryRow {
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::SingleTimeSeries,
            name: "monthly".into(),
            initial_timestamp: Some(t0()),
            resolution: Some(Period::Months(1)),
            time_step_count: Some(12),
            count: 2,
        };
        let pb = static_summary_row_to_pb(&static_row);
        assert_eq!(pb.resolution.as_deref(), Some("P1M"));
        assert_eq!(static_summary_row_from_pb(pb).unwrap(), static_row);

        let forecast_row = ForecastSummaryRow {
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::Deterministic,
            name: "monthly_fc".into(),
            initial_timestamp: Some(t0()),
            resolution: Some(Period::Months(1)),
            horizon: Some(Period::Months(3)),
            interval: Some(Period::Months(1)),
            window_count: Some(4),
            count: 1,
        };
        let pb = forecast_summary_row_to_pb(&forecast_row);
        assert_eq!(pb.resolution.as_deref(), Some("P1M"));
        assert_eq!(pb.horizon.as_deref(), Some("P3M"));
        assert_eq!(pb.interval.as_deref(), Some("P1M"));
        assert_eq!(forecast_summary_row_from_pb(pb).unwrap(), forecast_row);
    }

    // ---- full_key_from_pb failure modes -----------------------------------

    fn single_key_pb() -> pb::TimeSeriesKey {
        full_key_to_pb(&TimeSeriesKey::Single(SingleTimeSeriesKey {
            identity: KeyIdentity {
                owner_id: 1,
                owner_category: OwnerCategory::Component,
                time_series_type: TimeSeriesType::SingleTimeSeries,
                name: "load".into(),
                resolution: Some(Period::fixed(Duration::hours(1))),
                interval: None,
                features: Features::new(),
            },
            initial_timestamp: t0(),
            length: 24,
        }))
    }

    #[test]
    fn full_key_from_pb_requires_the_initial_timestamp() {
        let mut pb = single_key_pb();
        pb.initial_timestamp_rfc3339 = None;
        assert!(matches!(
            full_key_from_pb(pb),
            Err(ConvertError::MissingField(
                "TimeSeriesKey.initial_timestamp_rfc3339"
            ))
        ));
    }

    #[test]
    fn full_key_from_pb_requires_the_length() {
        let mut pb = single_key_pb();
        pb.length = None;
        assert!(matches!(
            full_key_from_pb(pb),
            Err(ConvertError::MissingField("TimeSeriesKey.length"))
        ));

        // NonSequential needs it too (and does *not* need a timestamp).
        let mut pb = full_key_to_pb(&TimeSeriesKey::NonSequential(NonSequentialTimeSeriesKey {
            identity: KeyIdentity {
                owner_id: 1,
                owner_category: OwnerCategory::Component,
                time_series_type: TimeSeriesType::NonSequentialTimeSeries,
                name: "events".into(),
                resolution: None,
                interval: None,
                features: Features::new(),
            },
            length: 3,
        }));
        assert!(pb.initial_timestamp_rfc3339.is_none());
        pb.length = None;
        assert!(matches!(
            full_key_from_pb(pb),
            Err(ConvertError::MissingField("TimeSeriesKey.length"))
        ));
    }

    #[test]
    fn full_key_from_pb_requires_the_forecast_horizon_and_count() {
        let forecast = TimeSeriesKey::Forecast(ForecastTimeSeriesKey {
            identity: KeyIdentity {
                owner_id: 1,
                owner_category: OwnerCategory::Component,
                time_series_type: TimeSeriesType::Deterministic,
                name: "det".into(),
                resolution: Some(Period::fixed(Duration::hours(1))),
                interval: Some(Period::fixed(Duration::hours(6))),
                features: Features::new(),
            },
            initial_timestamp: t0(),
            horizon: Period::fixed(Duration::hours(4)),
            count: 3,
        });

        let mut pb = full_key_to_pb(&forecast);
        pb.horizon = None;
        assert!(matches!(
            full_key_from_pb(pb),
            Err(ConvertError::MissingField("TimeSeriesKey.horizon"))
        ));

        let mut pb = full_key_to_pb(&forecast);
        pb.count = None;
        assert!(matches!(
            full_key_from_pb(pb),
            Err(ConvertError::MissingField("TimeSeriesKey.count"))
        ));
    }

    #[test]
    fn full_key_from_pb_rejects_an_unknown_type_enum() {
        let mut pb = single_key_pb();
        pb.time_series_type = 999;
        assert!(matches!(
            full_key_from_pb(pb),
            Err(ConvertError::InvalidValue {
                field: "time_series_type",
                ..
            })
        ));
    }

    #[test]
    fn key_from_pb_rejects_an_unknown_owner_category_enum() {
        let mut pb = single_key_pb();
        pb.owner_category = 999;
        assert!(matches!(
            key_from_pb(pb.clone()),
            Err(ConvertError::InvalidValue {
                field: "owner_category",
                ..
            })
        ));
        assert!(matches!(
            full_key_from_pb(pb),
            Err(ConvertError::InvalidValue {
                field: "owner_category",
                ..
            })
        ));
    }

    #[test]
    fn full_key_from_pb_rejects_a_malformed_initial_timestamp() {
        let mut pb = single_key_pb();
        pb.initial_timestamp_rfc3339 = Some("not a timestamp".into());
        assert!(matches!(
            full_key_from_pb(pb),
            Err(ConvertError::BadTimestamp(_))
        ));
    }

    // ---- DeterministicSingleTimeSeries on the wire ------------------------

    #[test]
    fn a_deterministic_single_time_series_metadata_row_round_trips() {
        // A DST is never returned as a distinct *data* variant (it reads back as
        // a Deterministic), but the tag remains visible in catalog surfaces, so
        // the metadata and key encodings must carry it exactly.
        let meta = TimeSeriesMetadata {
            owner_id: 4,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::DeterministicSingleTimeSeries,
            name: "load".into(),
            data_hash: [1u8; 32],
            initial_timestamp: Some(t0()),
            resolution: Some(Period::fixed(Duration::hours(1))),
            length: Some(8),
            horizon: Some(Period::fixed(Duration::hours(4))),
            interval: Some(Period::fixed(Duration::hours(2))),
            count: Some(3),
            timestamps: None,
            features: Features::new(),
            units: Some("MW".into()),
            percentiles: None,
            dtype: Dtype::F64,
            element_shape: vec![],
            ext: Some("QuadraticFunctionData".into()),
        };

        let pb = metadata_to_pb(&meta);
        assert_eq!(
            pb.time_series_type,
            pb::TimeSeriesType::DeterministicSingleTimeSeries as i32
        );
        // The metadata path *does* carry `ext` (unlike GetResp — see
        // `ext_is_always_empty_in_get_resp`).
        assert_eq!(pb.ext.as_deref(), Some("QuadraticFunctionData"));
        assert_eq!(metadata_from_pb(pb).unwrap(), meta);
    }

    #[test]
    fn a_deterministic_single_time_series_key_round_trips() {
        let key = TimeSeriesKey::Forecast(ForecastTimeSeriesKey {
            identity: KeyIdentity {
                owner_id: 4,
                owner_category: OwnerCategory::Component,
                time_series_type: TimeSeriesType::DeterministicSingleTimeSeries,
                name: "load".into(),
                resolution: Some(Period::fixed(Duration::hours(1))),
                interval: Some(Period::fixed(Duration::hours(2))),
                features: Features::new(),
            },
            initial_timestamp: t0(),
            horizon: Period::fixed(Duration::hours(4)),
            count: 3,
        });
        let pb = full_key_to_pb(&key);
        assert_eq!(
            pb.time_series_type,
            pb::TimeSeriesType::DeterministicSingleTimeSeries as i32
        );
        assert_eq!(full_key_from_pb(pb).unwrap(), key);
    }

    #[test]
    fn requested_type_round_trips_for_every_form() {
        for requested in [
            RequestedType::Concrete(TimeSeriesType::Deterministic),
            RequestedType::Concrete(TimeSeriesType::DeterministicSingleTimeSeries),
            RequestedType::Concrete(TimeSeriesType::Probabilistic),
            RequestedType::Concrete(TimeSeriesType::Scenarios),
            RequestedType::Concrete(TimeSeriesType::SingleTimeSeries),
            RequestedType::Concrete(TimeSeriesType::NonSequentialTimeSeries),
            RequestedType::AbstractDeterministic,
        ] {
            let pb = requested_type_to_pb(requested);
            assert_eq!(
                requested_type_from_pb(pb).unwrap(),
                requested,
                "{requested:?}"
            );
        }

        // An unset oneof is a clean error.
        assert!(matches!(
            requested_type_from_pb(pb::RequestedType { kind: None }),
            Err(ConvertError::MissingField("RequestedType.kind"))
        ));
        // An unknown concrete code is a clean error.
        assert!(matches!(
            requested_type_from_pb(pb::RequestedType {
                kind: Some(pb::requested_type::Kind::Concrete(999)),
            }),
            Err(ConvertError::InvalidValue {
                field: "time_series_type",
                ..
            })
        ));
    }

    // ---- 3.2: `ext` on the GetResp path ----------------------------------

    #[test]
    fn ext_is_always_empty_in_get_resp() {
        // FINDING F1 (TEST_COVERAGE_PLAN.md §9), resolved as documented
        // behavior: `GetResp.ext` is always the empty string, and this test is
        // the tripwire that keeps it that way.
        //
        // It is not a value being dropped. `ext` belongs to the association row
        // — `metadata_to_pb` carries it, and a gRPC caller reads it from
        // `GetMetadata` / `ListTimeSeries` — whereas the core `TimeSeriesData`
        // variants have no `ext` field at all, so this function has nothing to
        // forward. Populating it would mean a second catalog lookup in the
        // server handler (multiplied across `BulkRead`, which reuses `GetResp`),
        // to serve a value the typed Rust client still could not surface.
        //
        // If that ever changes, this assertion is the thing to revisit
        // deliberately — along with the comment on field 10 in
        // `proto/castore/v1/store.proto`.
        let data = typed(Dtype::F64, vec![3]);
        for original in [
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                t0(),
                Duration::hours(1),
                data.clone(),
                "load",
            )),
            TimeSeriesData::NonSequentialTimeSeries(
                NonSequentialTimeSeries::new(
                    vec![t0(), t0() + Duration::hours(1), t0() + Duration::hours(5)],
                    data.clone(),
                    "events",
                )
                .unwrap(),
            ),
            TimeSeriesData::Deterministic(
                Deterministic::new(
                    t0(),
                    Duration::hours(1),
                    Duration::hours(1),
                    Duration::hours(1),
                    3,
                    typed(Dtype::F64, vec![1, 3]),
                    "det",
                )
                .unwrap(),
            ),
            TimeSeriesData::Probabilistic(
                Probabilistic::new(
                    t0(),
                    Duration::hours(1),
                    Duration::hours(1),
                    Duration::hours(1),
                    3,
                    vec![0.5],
                    typed(Dtype::F64, vec![1, 1, 3]),
                    "prob",
                )
                .unwrap(),
            ),
            TimeSeriesData::Scenarios(
                Scenarios::new(
                    t0(),
                    Duration::hours(1),
                    Duration::hours(1),
                    Duration::hours(1),
                    3,
                    2,
                    typed(Dtype::F64, vec![2, 1, 3]),
                    "scen",
                )
                .unwrap(),
            ),
        ] {
            let resp = time_series_data_to_get_resp(&original);
            assert_eq!(
                resp.ext,
                "",
                "GetResp.ext must stay empty for {:?}",
                original.time_series_type()
            );
        }
    }

    // ---- Non-finite and extreme values across the wire -------------------

    #[test]
    fn non_finite_floats_survive_the_wire_encoding_bit_exactly() {
        let values = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0, 0.0];
        let data = TypedArray::from_slice(vec![values.len()], &values).unwrap();
        let original = TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
            t0(),
            Duration::hours(1),
            data.clone(),
            "load",
        ));
        let resp = time_series_data_to_get_resp(&original);
        assert_eq!(resp.value_bytes, data.bytes);
        let back = get_resp_to_time_series_data(resp, "load".to_string()).unwrap();
        // `TypedArray`'s PartialEq compares raw bytes, so this is a bitwise
        // comparison even for NaN.
        assert_eq!(back, original);
    }

    #[test]
    fn a_bad_hash_length_in_metadata_errors() {
        let mut pb = metadata_to_pb(&TimeSeriesMetadata {
            owner_id: 1,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::SingleTimeSeries,
            name: "load".into(),
            data_hash: [0u8; 32],
            initial_timestamp: Some(t0()),
            resolution: Some(Period::fixed(Duration::hours(1))),
            length: Some(3),
            horizon: None,
            interval: None,
            count: None,
            timestamps: None,
            features: Features::new(),
            units: None,
            percentiles: None,
            dtype: Dtype::F64,
            element_shape: vec![],
            ext: None,
        });
        pb.data_hash = vec![0u8; 31];
        assert!(matches!(
            metadata_from_pb(pb),
            Err(ConvertError::BadHashLen(31))
        ));
    }
}
