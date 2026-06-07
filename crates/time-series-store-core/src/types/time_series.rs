use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::array::TypedArray;

/// Discriminator for the six time series types defined in the spec.
///
/// Static series carry runtime variants in [`TimeSeriesData`]. Forecast types
/// use the forecast-specific store API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimeSeriesType {
    SingleTimeSeries,
    NonSequentialTimeSeries,
    Deterministic,
    DeterministicSingleTimeSeries,
    Probabilistic,
    Scenarios,
}

impl TimeSeriesType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeSeriesType::SingleTimeSeries => "SingleTimeSeries",
            TimeSeriesType::NonSequentialTimeSeries => "NonSequentialTimeSeries",
            TimeSeriesType::Deterministic => "Deterministic",
            TimeSeriesType::DeterministicSingleTimeSeries => "DeterministicSingleTimeSeries",
            TimeSeriesType::Probabilistic => "Probabilistic",
            TimeSeriesType::Scenarios => "Scenarios",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "SingleTimeSeries" => TimeSeriesType::SingleTimeSeries,
            "NonSequentialTimeSeries" => TimeSeriesType::NonSequentialTimeSeries,
            "Deterministic" => TimeSeriesType::Deterministic,
            "DeterministicSingleTimeSeries" => TimeSeriesType::DeterministicSingleTimeSeries,
            "Probabilistic" => TimeSeriesType::Probabilistic,
            "Scenarios" => TimeSeriesType::Scenarios,
            _ => return None,
        })
    }
}

impl FromStr for TimeSeriesType {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s).ok_or(())
    }
}

/// A time series array at regular intervals.
///
/// `data` is a [`TypedArray`]: its first dimension is time (`length`) and any
/// trailing dimensions are the per-step element shape (e.g. the 3 coefficients
/// of a quadratic cost curve). The element dtype is part of the array.
#[derive(Debug, Clone, PartialEq)]
pub struct SingleTimeSeries {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Duration,
    pub length: usize,
    pub data: TypedArray,
}

impl SingleTimeSeries {
    pub fn new(initial_timestamp: DateTime<Utc>, resolution: Duration, data: TypedArray) -> Self {
        let length = data.length();
        Self {
            initial_timestamp,
            resolution,
            length,
            data,
        }
    }
}

/// A time series array at explicit, irregular timestamps.
///
/// Timestamps must be strictly increasing and the timestamp count must equal
/// the first dimension of `data`.
#[derive(Debug, Clone, PartialEq)]
pub struct NonSequentialTimeSeries {
    pub timestamps: Vec<DateTime<Utc>>,
    pub length: usize,
    pub data: TypedArray,
}

impl NonSequentialTimeSeries {
    pub fn new(timestamps: Vec<DateTime<Utc>>, data: TypedArray) -> Result<Self, String> {
        let length = data.length();
        if timestamps.len() != length {
            return Err(format!(
                "timestamp count {} does not match data length {length}",
                timestamps.len()
            ));
        }
        if timestamps.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("timestamps must be strictly increasing".to_string());
        }
        Ok(Self {
            timestamps,
            length,
            data,
        })
    }
}

/// Runtime variant container for static time-series types.
#[derive(Debug, Clone, PartialEq)]
pub enum TimeSeriesData {
    SingleTimeSeries(SingleTimeSeries),
    NonSequentialTimeSeries(NonSequentialTimeSeries),
}

impl TimeSeriesData {
    pub fn time_series_type(&self) -> TimeSeriesType {
        match self {
            TimeSeriesData::SingleTimeSeries(_) => TimeSeriesType::SingleTimeSeries,
            TimeSeriesData::NonSequentialTimeSeries(_) => TimeSeriesType::NonSequentialTimeSeries,
        }
    }

    pub fn as_single(&self) -> Option<&SingleTimeSeries> {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => Some(s),
            TimeSeriesData::NonSequentialTimeSeries(_) => None,
        }
    }

    pub fn as_non_sequential(&self) -> Option<&NonSequentialTimeSeries> {
        match self {
            TimeSeriesData::SingleTimeSeries(_) => None,
            TimeSeriesData::NonSequentialTimeSeries(s) => Some(s),
        }
    }
}
