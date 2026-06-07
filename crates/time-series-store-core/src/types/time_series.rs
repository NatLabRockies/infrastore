use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::array::TypedArray;

/// Discriminator for the six time series types defined in the spec.
///
/// Only `SingleTimeSeries` carries a runtime variant in [`TimeSeriesData`]
/// for v0; the other variants exist so the metadata schema and APIs accept
/// them as future extension points.
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
    pub fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Duration,
        data: TypedArray,
    ) -> Self {
        let length = data.length();
        Self {
            initial_timestamp,
            resolution,
            length,
            data,
        }
    }
}

/// Runtime variant container so future forecast types can be added without
/// breaking the public API.
#[derive(Debug, Clone, PartialEq)]
pub enum TimeSeriesData {
    SingleTimeSeries(SingleTimeSeries),
    // NonSequentialTimeSeries(...), Deterministic(...), Probabilistic(...),
    // Scenarios(...) — added in later milestones.
}

impl TimeSeriesData {
    pub fn time_series_type(&self) -> TimeSeriesType {
        match self {
            TimeSeriesData::SingleTimeSeries(_) => TimeSeriesType::SingleTimeSeries,
        }
    }

    pub fn as_single(&self) -> Option<&SingleTimeSeries> {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => Some(s),
        }
    }
}
