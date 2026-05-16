use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use ndarray::ArrayD;
use serde::{Deserialize, Serialize};

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

/// A one-dimensional (in time) array of values at regular intervals.
///
/// The trailing axes of `data` may carry per-timestep vectors (e.g. polynomial
/// coefficients for a cost curve), so `data` is `ArrayD<f64>` rather than a
/// 1D array.
#[derive(Debug, Clone, PartialEq)]
pub struct SingleTimeSeries {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Duration,
    pub length: usize,
    pub data: ArrayD<f64>,
}

impl SingleTimeSeries {
    pub fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Duration,
        data: ArrayD<f64>,
    ) -> Self {
        let length = data.shape().first().copied().unwrap_or(0);
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
