//! Conversions between generated protobuf types and `infrastore_core` types.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use infrastore_core::{
    Deterministic, ElementType, FeatureValue, Features, ForecastSummaryRow,
    NonSequentialTimeSeries, OwnerCategory, Period, PersistentTimeSeries, Probabilistic, Scenarios,
    SingleTimeSeries, StaticSummaryRow, TimeReference, TimeSeriesData, TimeSeriesId,
    TimeSeriesMetadata, TimeSeriesType, TypedArray, UnitSystem,
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
            TimeSeriesType::PersistentTimeSeries => pb::TimeSeriesType::PersistentTimeSeries,
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
            pb::TimeSeriesType::PersistentTimeSeries => TimeSeriesType::PersistentTimeSeries,
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
        quantity_kind: m.quantity_kind.clone(),
        unit_system: m.unit_system.map(|u| u.as_str().to_owned()),
        time_reference: m
            .time_reference
            .as_ref()
            .map(TimeReference::as_storage_string),
        component_field: m.component_field.clone(),
        element_type: m.element_type.to_string(),
        element_shape: m.element_shape.iter().map(|d| *d as u64).collect(),
        application_data: m.application_data.clone(),
        id: m.id.map(|id| id.get()),
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
        quantity_kind: m.quantity_kind,
        // An unrecognized basis is an error rather than a silent `None`, for
        // the same reason the catalog read path errors on one: "unspecified"
        // and "a basis this build does not know" must not look alike.
        unit_system: m
            .unit_system
            .map(|s| {
                UnitSystem::parse(&s).ok_or(ConvertError::InvalidValue {
                    field: "unit_system",
                    message: format!("unknown unit_system {s:?}"),
                })
            })
            .transpose()?,
        time_reference: parse_time_reference(m.time_reference.as_deref())?,
        component_field: m.component_field,
        percentiles: if m.percentiles.is_empty() {
            None
        } else {
            Some(m.percentiles)
        },
        // Error on an unparseable element_type (matching the data-decode path)
        // rather than silently coercing to f64 scalars.
        element_type: ElementType::parse(&m.element_type).ok_or(ConvertError::InvalidValue {
            field: "element_type",
            message: format!("unknown element_type {:?}", m.element_type),
        })?,
        element_shape: m.element_shape.iter().map(|d| *d as usize).collect(),
        application_data: m.application_data,
        id: m.id.map(TimeSeriesId),
    })
}

// ---- TimeSeriesData (for ReadByIdResp body construction) ----

/// Encode a [`TimeSeriesData`] into the wire-shape used by `ReadByIdResp`.
///
/// The `element_type` comes off the data itself: a read fills it in from the
/// association row, so the caller needs no second catalog lookup to describe
/// what the bytes mean.
///
/// `ReadByIdResp.application_data` is left empty on every variant, and that is not an omission:
/// `application_data` belongs to the association row ([`TimeSeriesMetadata::application_data`], which
/// [`metadata_to_pb`] does carry), so a gRPC caller reads it from `GetMetadata`
/// or `ListTimeSeries` rather than from the values. Pinned by
/// `application_data_is_always_empty_in_read_resp`; see the proto comment on field 10.
pub fn time_series_data_to_read_resp(data: &TimeSeriesData) -> pb::ReadByIdResp {
    let element_type = data.element_type();
    // Uniform across every variant, so read once. These describe the values and
    // travel with them, which is exactly why the read path has to carry them:
    // `Store::materialize_time_series` populates them on a local read, so
    // dropping them here made the same call return an undescribed series over
    // the wire. (`application_data` above is the genuine exception and stays
    // empty.)
    let units = data.units().map(str::to_owned);
    let quantity_kind = data.quantity_kind().map(str::to_owned);
    let unit_system = data.unit_system().map(|u| u.as_str().to_owned());
    let time_reference = data.time_reference().map(TimeReference::as_storage_string);
    let component_field = data.component_field().map(str::to_owned);
    let name = data.name().to_owned();
    match data {
        TimeSeriesData::SingleTimeSeries(s) => pb::ReadByIdResp {
            initial_timestamp_rfc3339: s.initial_timestamp.to_rfc3339(),
            resolution: s.resolution.to_iso8601(),
            length: s.length as u64,
            shape: s.data.shape.iter().map(|d| *d as u64).collect(),
            element_type: element_type.to_string(),
            units: units.clone(),
            quantity_kind: quantity_kind.clone(),
            unit_system: unit_system.clone(),
            time_reference: time_reference.clone(),
            component_field: component_field.clone(),
            value_bytes: s.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::SingleTimeSeries as i32,
            timestamps_rfc3339: Vec::new(),
            application_data: String::new(),
            horizon: String::new(),
            interval: String::new(),
            count: 0,
            percentiles: Vec::new(),
            scenario_count: 0,
            name: name.clone(),
        },
        TimeSeriesData::NonSequentialTimeSeries(s) => pb::ReadByIdResp {
            initial_timestamp_rfc3339: String::new(),
            resolution: String::new(),
            length: s.length as u64,
            shape: s.data.shape.iter().map(|d| *d as u64).collect(),
            element_type: element_type.to_string(),
            units: units.clone(),
            quantity_kind: quantity_kind.clone(),
            unit_system: unit_system.clone(),
            time_reference: time_reference.clone(),
            component_field: component_field.clone(),
            value_bytes: s.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::NonSequentialTimeSeries as i32,
            timestamps_rfc3339: s.timestamps.iter().map(|t| t.to_rfc3339()).collect(),
            application_data: String::new(),
            horizon: String::new(),
            interval: String::new(),
            count: 0,
            percentiles: Vec::new(),
            scenario_count: 0,
            name: name.clone(),
        },
        // Wire-identical to the `NonSequentialTimeSeries` arm above but for the
        // type tag: both are static series on an explicit time axis, so both
        // send `length` plus `timestamps_rfc3339` and nothing else. The
        // difference between them is a read *semantic*, not a payload shape.
        TimeSeriesData::PersistentTimeSeries(s) => pb::ReadByIdResp {
            initial_timestamp_rfc3339: String::new(),
            resolution: String::new(),
            length: s.length as u64,
            shape: s.data.shape.iter().map(|d| *d as u64).collect(),
            element_type: element_type.to_string(),
            units: units.clone(),
            quantity_kind: quantity_kind.clone(),
            unit_system: unit_system.clone(),
            time_reference: time_reference.clone(),
            component_field: component_field.clone(),
            value_bytes: s.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::PersistentTimeSeries as i32,
            timestamps_rfc3339: s.timestamps.iter().map(|t| t.to_rfc3339()).collect(),
            application_data: String::new(),
            horizon: String::new(),
            interval: String::new(),
            count: 0,
            percentiles: Vec::new(),
            scenario_count: 0,
            name: name.clone(),
        },
        TimeSeriesData::Deterministic(d) => pb::ReadByIdResp {
            initial_timestamp_rfc3339: d.initial_timestamp.to_rfc3339(),
            resolution: d.resolution.to_iso8601(),
            length: d.data.shape[0] as u64,
            shape: d.data.shape.iter().map(|x| *x as u64).collect(),
            element_type: element_type.to_string(),
            units: units.clone(),
            quantity_kind: quantity_kind.clone(),
            unit_system: unit_system.clone(),
            time_reference: time_reference.clone(),
            component_field: component_field.clone(),
            value_bytes: d.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::Deterministic as i32,
            timestamps_rfc3339: Vec::new(),
            application_data: String::new(),
            horizon: d.horizon.to_iso8601(),
            interval: d.interval.to_iso8601(),
            count: d.count as u64,
            percentiles: Vec::new(),
            scenario_count: 0,
            name: name.clone(),
        },
        TimeSeriesData::Probabilistic(p) => pb::ReadByIdResp {
            initial_timestamp_rfc3339: p.initial_timestamp.to_rfc3339(),
            resolution: p.resolution.to_iso8601(),
            length: p.data.shape[0] as u64,
            shape: p.data.shape.iter().map(|x| *x as u64).collect(),
            element_type: element_type.to_string(),
            units: units.clone(),
            quantity_kind: quantity_kind.clone(),
            unit_system: unit_system.clone(),
            time_reference: time_reference.clone(),
            component_field: component_field.clone(),
            value_bytes: p.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::Probabilistic as i32,
            timestamps_rfc3339: Vec::new(),
            application_data: String::new(),
            horizon: p.horizon.to_iso8601(),
            interval: p.interval.to_iso8601(),
            count: p.count as u64,
            percentiles: p.percentiles.clone(),
            scenario_count: 0,
            name: name.clone(),
        },
        TimeSeriesData::Scenarios(s) => pb::ReadByIdResp {
            initial_timestamp_rfc3339: s.initial_timestamp.to_rfc3339(),
            resolution: s.resolution.to_iso8601(),
            length: s.data.shape[0] as u64,
            shape: s.data.shape.iter().map(|x| *x as u64).collect(),
            element_type: element_type.to_string(),
            units: units.clone(),
            quantity_kind: quantity_kind.clone(),
            unit_system: unit_system.clone(),
            time_reference: time_reference.clone(),
            component_field: component_field.clone(),
            value_bytes: s.data.bytes.clone(),
            time_series_type: pb::TimeSeriesType::Scenarios as i32,
            timestamps_rfc3339: Vec::new(),
            application_data: String::new(),
            horizon: s.horizon.to_iso8601(),
            interval: s.interval.to_iso8601(),
            count: s.count as u64,
            percentiles: Vec::new(),
            scenario_count: s.scenario_count as u64,
            name: name.clone(),
        },
    }
}

/// Rebuild a [`TimeSeriesData`] from the wire form.
///
/// The name comes off the message rather than from the caller: a read names an
/// id, and an id carries no name, so a client has nothing to supply.
pub fn read_resp_to_time_series_data(
    mut resp: pb::ReadByIdResp,
) -> Result<TimeSeriesData, ConvertError> {
    let name = std::mem::take(&mut resp.name);
    let ts_type = pb::TimeSeriesType::try_from(resp.time_series_type).map_err(|_| {
        ConvertError::InvalidValue {
            field: "time_series_type",
            message: format!("unknown enum value {}", resp.time_series_type),
        }
    })?;
    let shape: Vec<usize> = resp.shape.iter().map(|d| *d as usize).collect();
    let element_type =
        ElementType::parse(&resp.element_type).ok_or(ConvertError::InvalidValue {
            field: "element_type",
            message: format!("unknown element_type {:?}", resp.element_type),
        })?;
    let dtype = element_type.physical_dtype();
    // Taken before the match, which moves other fields out of `resp`. An
    // unrecognized unit system is an error rather than a silent `None`, exactly
    // as on the metadata path: "unspecified" and "a basis this build does not
    // know" must not look alike.
    let units = resp.units.take();
    let quantity_kind = resp.quantity_kind.take();
    let component_field = resp.component_field.take();
    let time_reference = parse_time_reference(resp.time_reference.take().as_deref())?;
    let unit_system = resp
        .unit_system
        .take()
        .map(|s| {
            UnitSystem::parse(&s).ok_or(ConvertError::InvalidValue {
                field: "unit_system",
                message: format!("unknown unit_system {s:?}"),
            })
        })
        .transpose()?;
    let data = TypedArray::new(dtype, shape, resp.value_bytes).map_err(|e| {
        ConvertError::InvalidValue {
            field: "value_bytes",
            message: e,
        }
    })?;
    let series: Result<TimeSeriesData, ConvertError> = match ts_type {
        pb::TimeSeriesType::SingleTimeSeries => {
            let initial_timestamp = DateTime::parse_from_rfc3339(&resp.initial_timestamp_rfc3339)
                .map(|d| d.with_timezone(&Utc))?;
            Ok(TimeSeriesData::SingleTimeSeries(SingleTimeSeries {
                initial_timestamp,
                resolution: period_from_iso(&resp.resolution)?,
                length: resp.length as usize,
                data,
                name,
                // Overwritten below, along with every other variant's.
                element_type,
                // Filled in at the tail, along with every other variant's.
                units: None,
                quantity_kind: None,
                unit_system: None,
                time_reference: None,
                component_field: None,
                application_data: None,
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
        pb::TimeSeriesType::PersistentTimeSeries => {
            let timestamps = resp
                .timestamps_rfc3339
                .iter()
                .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
                .collect::<Result<Vec<_>, _>>()?;
            let series = PersistentTimeSeries::new(timestamps, data, name).map_err(|message| {
                ConvertError::InvalidValue {
                    field: "timestamps_rfc3339",
                    message,
                }
            })?;
            Ok(TimeSeriesData::PersistentTimeSeries(series))
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
    };
    // The wire carries the element type, so a read over gRPC reports the same
    // one a local read would. Applied here rather than per branch because the
    // constructors resolve it to plain scalars of the array's dtype, which is
    // right only when that is what the server actually sent. The unit
    // descriptors ride along for the same reason: they describe the values, the
    // core attaches them on a local read, and a client should not be able to
    // tell the two paths apart.
    let mut series = series?.with_element_type(element_type);
    series.set_units(units);
    series.set_quantity_kind(quantity_kind);
    series.set_unit_system(unit_system);
    series.set_time_reference(time_reference);
    series.set_component_field(component_field);
    Ok(series)
}

// ---- Helpers ----

/// Decode a wire `time_reference` string. An unparseable one is an error rather
/// than a silent `None`, for the same reason `unit_system` is: "unspecified" and
/// "a spelling this build cannot read" must not look alike — the second would
/// hand a caller an aware timestamp for a series that never claimed one.
fn parse_time_reference(s: Option<&str>) -> Result<Option<TimeReference>, ConvertError> {
    s.map(|s| {
        TimeReference::parse(s).map_err(|e| ConvertError::InvalidValue {
            field: "time_reference",
            message: format!("unknown time_reference {s:?}: {e}"),
        })
    })
    .transpose()
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

/// Decode a requested [`TimeSeriesType`] from its proto enum code. The widening
/// of a `Deterministic` request to its two storage forms happens in the core
/// (see [`TimeSeriesType::accepts`]), not on the wire.
pub fn requested_type_from_pb(code: i32) -> Result<TimeSeriesType, ConvertError> {
    ts_type_from_i32(code)
}

/// Encode a requested [`TimeSeriesType`] as its proto enum code.
pub fn requested_type_to_pb(t: TimeSeriesType) -> i32 {
    pb::TimeSeriesType::from(t) as i32
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
    use chrono::{Duration, TimeZone};
    use infrastore_core::{Dtype, TypedArray};

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
            quantity_kind: None,
            unit_system: None,
            component_field: None,
            time_reference: None,
            element_type: "f64".into(),
            element_shape: Vec::new(),
            application_data: None,
            id: None,
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
    fn unknown_element_type_in_metadata_errors() {
        let mut pb = base_pb_metadata();
        pb.element_type = "float64".into(); // not a valid element_type spelling
        let err = metadata_from_pb(pb).unwrap_err();
        assert!(matches!(
            err,
            ConvertError::InvalidValue {
                field: "element_type",
                ..
            }
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
        let resp = time_series_data_to_read_resp(&original);
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

        let roundtripped = read_resp_to_time_series_data(resp).unwrap();
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
        let resp = time_series_data_to_read_resp(&original);
        assert_eq!(resp.shape, vec![2u64, 3, 2]);
        assert_eq!(resp.count, 3);

        let roundtripped = read_resp_to_time_series_data(resp).unwrap();
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
        let resp = time_series_data_to_read_resp(&original);
        assert_eq!(
            resp.time_series_type,
            pb::TimeSeriesType::Probabilistic as i32
        );
        assert_eq!(resp.percentiles, percentiles);
        assert_eq!(resp.count, 2);
        assert_eq!(resp.scenario_count, 0);
        assert_eq!(resp.length, 3); // shape[0] = num_percentiles

        let roundtripped = read_resp_to_time_series_data(resp).unwrap();
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
        let resp = time_series_data_to_read_resp(&original);
        assert_eq!(resp.shape, vec![2u64, 3, 4, 5]);
        assert_eq!(resp.percentiles, percentiles);

        let roundtripped = read_resp_to_time_series_data(resp).unwrap();
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
        let resp = time_series_data_to_read_resp(&original);
        assert_eq!(resp.time_series_type, pb::TimeSeriesType::Scenarios as i32);
        assert_eq!(resp.scenario_count, 4);
        assert_eq!(resp.count, 3);
        assert!(resp.percentiles.is_empty());
        assert_eq!(resp.length, 4); // shape[0] = scenario_count

        let roundtripped = read_resp_to_time_series_data(resp).unwrap();
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
        let resp = time_series_data_to_read_resp(&original);
        assert_eq!(resp.shape, vec![2u64, 4, 3, 2]);
        assert_eq!(resp.scenario_count, 2);

        let roundtripped = read_resp_to_time_series_data(resp).unwrap();
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
        let resp = time_series_data_to_read_resp(&original);
        assert_eq!(resp.element_type, "i64");

        let roundtripped = read_resp_to_time_series_data(resp).unwrap();
        assert_eq!(roundtripped, original);
        let s = roundtripped.as_scenarios().unwrap();
        assert_eq!(s.data.dtype, Dtype::I64);
        // Verify first and last i64 values
        let vals: Vec<i64> = s
            .data
            .bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| i64::from_le_bytes(*c))
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
    use chrono::{Duration, TimeZone};
    use infrastore_core::{Dtype, TypedArray};

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

    const ALL_DTYPES: &[Dtype] = Dtype::ALL;

    // ---- Dtype matrix ------------------------------------------------------

    #[test]
    fn every_dtype_round_trips_through_read_resp() {
        for &dtype in ALL_DTYPES {
            let data = typed(dtype, vec![3]);
            let original = TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                t0(),
                Duration::hours(1),
                data.clone(),
                "load",
            ));
            let resp = time_series_data_to_read_resp(&original);
            assert_eq!(
                resp.element_type,
                dtype.as_str(),
                "{dtype:?}: element_type on the wire"
            );
            assert_eq!(resp.value_bytes, data.bytes, "{dtype:?}: bytes");

            let back = read_resp_to_time_series_data(resp).unwrap();
            assert_eq!(back, original, "{dtype:?}");
        }
    }

    #[test]
    fn every_dtype_round_trips_through_metadata() {
        for &dtype in ALL_DTYPES {
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
                quantity_kind: None,
                unit_system: None,
                component_field: None,
                time_reference: None,
                percentiles: None,
                element_type: ElementType::Scalar(dtype),
                element_shape: vec![],
                application_data: None,
                id: None,
            };
            let pb = metadata_to_pb(&meta);
            assert_eq!(pb.element_type, dtype.as_str(), "{dtype:?}");
            assert_eq!(metadata_from_pb(pb).unwrap(), meta, "{dtype:?}");
        }
    }

    #[test]
    fn every_dtype_round_trips_in_a_forecast() {
        for &dtype in ALL_DTYPES {
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
            let resp = time_series_data_to_read_resp(&original);
            assert_eq!(resp.element_type, dtype.as_str(), "{dtype:?}");
            let back = read_resp_to_time_series_data(resp).unwrap();
            assert_eq!(back, original, "{dtype:?}");
        }
    }

    #[test]
    fn an_unknown_element_type_in_a_get_resp_errors() {
        let data = typed(Dtype::F64, vec![3]);
        let original = TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
            t0(),
            Duration::hours(1),
            data,
            "load",
        ));
        let mut resp = time_series_data_to_read_resp(&original);
        resp.element_type = "float64".into();
        assert!(matches!(
            read_resp_to_time_series_data(resp),
            Err(ConvertError::InvalidValue {
                field: "element_type",
                ..
            })
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
            quantity_kind: None,
            unit_system: None,
            component_field: None,
            time_reference: None,
            percentiles: None,
            element_type: ElementType::default(),
            element_shape: vec![],
            application_data: None,
            id: None,
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
            quantity_kind: None,
            unit_system: None,
            component_field: None,
            time_reference: None,
            percentiles: None,
            element_type: ElementType::default(),
            element_shape: vec![],
            application_data: None,
            id: None,
        };
        let pb = metadata_to_pb(&meta);
        assert_eq!(pb.resolution.as_deref(), Some("P1Y"));
        assert_eq!(
            metadata_from_pb(pb).unwrap().resolution,
            Some(Period::Months(12))
        );
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

        let resp = time_series_data_to_read_resp(&original);
        assert_eq!(resp.resolution, "P1M");
        assert_eq!(resp.horizon, "P3M");
        assert_eq!(resp.interval, "P1M");

        let back = read_resp_to_time_series_data(resp).unwrap();
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

    /// A read names an id, and an id carries no name, so the name has to come
    /// off the wire. Before it did, every series read over gRPC came back with
    /// an empty name where the identical local call returned it.
    #[test]
    fn the_name_survives_the_read_round_trip_for_every_variant() {
        let ts = t0();
        let arr = |shape: Vec<usize>| {
            let n: usize = shape.iter().product();
            let vals: Vec<f64> = (0..n).map(|i| i as f64).collect();
            TypedArray::from_f64(shape, &vals)
        };
        let variants = vec![
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                ts,
                Duration::hours(1),
                arr(vec![4]),
                "sts_name",
            )),
            TimeSeriesData::NonSequentialTimeSeries(
                NonSequentialTimeSeries::new(
                    vec![ts, ts + Duration::hours(3)],
                    arr(vec![2]),
                    "nsts_name",
                )
                .unwrap(),
            ),
            TimeSeriesData::Deterministic(
                Deterministic::new(
                    ts,
                    Duration::hours(1),
                    Duration::hours(4),
                    Duration::hours(6),
                    3,
                    arr(vec![4, 3]),
                    "det_name",
                )
                .unwrap(),
            ),
            TimeSeriesData::Probabilistic(
                Probabilistic::new(
                    ts,
                    Duration::hours(1),
                    Duration::hours(3),
                    Duration::hours(3),
                    2,
                    vec![0.1, 0.9],
                    arr(vec![2, 3, 2]),
                    "prob_name",
                )
                .unwrap(),
            ),
            TimeSeriesData::Scenarios(
                Scenarios::new(
                    ts,
                    Duration::hours(1),
                    Duration::hours(3),
                    Duration::hours(3),
                    2,
                    2,
                    arr(vec![2, 3, 2]),
                    "scen_name",
                )
                .unwrap(),
            ),
        ];
        for original in variants {
            let expected = original.name().to_string();
            let resp = time_series_data_to_read_resp(&original);
            assert_eq!(resp.name, expected, "{expected}: encode");
            let back = read_resp_to_time_series_data(resp).unwrap();
            assert_eq!(back.name(), expected, "{expected}: decode");
        }
    }

    // ---- DeterministicSingleTimeSeries on the wire ------------------------

    #[test]
    fn a_deterministic_single_time_series_metadata_row_round_trips() {
        // A DST is never returned as a distinct *data* variant (it reads back as
        // a Deterministic), but the tag remains visible in catalog surfaces, so
        // the metadata encoding must carry it exactly.
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
            quantity_kind: None,
            unit_system: None,
            component_field: Some("max_active_power".into()),
            time_reference: None,
            percentiles: None,
            element_type: ElementType::default(),
            element_shape: vec![],
            application_data: Some("QuadraticFunctionData".into()),
            id: None,
        };

        let pb = metadata_to_pb(&meta);
        assert_eq!(
            pb.time_series_type,
            pb::TimeSeriesType::DeterministicSingleTimeSeries as i32
        );
        // The metadata path *does* carry `application_data` (unlike GetResp — see
        // `application_data_is_always_empty_in_read_resp`).
        assert_eq!(
            pb.application_data.as_deref(),
            Some("QuadraticFunctionData")
        );
        assert_eq!(pb.component_field.as_deref(), Some("max_active_power"));
        // Struct equality, so every descriptor -- `component_field` included --
        // has to survive both directions, not just the ones named above.
        assert_eq!(metadata_from_pb(pb).unwrap(), meta);
    }

    #[test]
    fn requested_type_round_trips_for_every_form() {
        for requested in [
            TimeSeriesType::Deterministic,
            TimeSeriesType::DeterministicSingleTimeSeries,
            TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios,
            TimeSeriesType::SingleTimeSeries,
            TimeSeriesType::NonSequentialTimeSeries,
        ] {
            let pb = requested_type_to_pb(requested);
            assert_eq!(
                requested_type_from_pb(pb).unwrap(),
                requested,
                "{requested:?}"
            );
        }

        // An unknown code is a clean error.
        assert!(matches!(
            requested_type_from_pb(999),
            Err(ConvertError::InvalidValue {
                field: "time_series_type",
                ..
            })
        ));
    }

    // ---- 3.2: `application_data` on the GetResp path ----------------------------------

    #[test]
    fn application_data_is_always_empty_in_read_resp() {
        // FINDING F1 (TEST_COVERAGE_PLAN.md §9), resolved as documented
        // behavior: `GetResp.application_data` is always the empty string, and this test is
        // the tripwire that keeps it that way.
        //
        // It is not a value being dropped. `application_data` belongs to the association row
        // — `metadata_to_pb` carries it, and a gRPC caller reads it from
        // `GetMetadata` / `ListTimeSeries` — whereas the core `TimeSeriesData`
        // variants have no `application_data` field at all, so this function has nothing to
        // forward. Populating it would mean a second catalog lookup in the
        // server handler (multiplied across `BulkRead`, which reuses `GetResp`),
        // to serve a value the typed Rust client still could not surface.
        //
        // If that ever changes, this assertion is the thing to revisit
        // deliberately — along with the comment on field 10 in
        // `crates/infrastore-proto/proto/infrastore/v1/store.proto`.
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
            let resp = time_series_data_to_read_resp(&original);
            assert_eq!(
                resp.application_data,
                "",
                "GetResp.application_data must stay empty for {:?}",
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
        let resp = time_series_data_to_read_resp(&original);
        assert_eq!(resp.value_bytes, data.bytes);
        let back = read_resp_to_time_series_data(resp).unwrap();
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
            quantity_kind: None,
            unit_system: None,
            component_field: None,
            time_reference: None,
            percentiles: None,
            element_type: ElementType::default(),
            element_shape: vec![],
            application_data: None,
            id: None,
        });
        pb.data_hash = vec![0u8; 31];
        assert!(matches!(
            metadata_from_pb(pb),
            Err(ConvertError::BadHashLen(31))
        ));
    }
}
