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

/// The time series type named by a *read* request.
///
/// A request may name a concrete [`TimeSeriesType`], or an abstract family that
/// resolves to exactly one concrete type at read time. The abstract variant is
/// never stored and never returned — it exists only to address a forecast whose
/// concrete type the caller does not (or should not need to) know.
///
/// [`RequestedType::AbstractDeterministic`] mirrors InfrastructureSystems.jl's
/// `AbstractDeterministic` supertype: it matches a stored `Deterministic` *or* a
/// `DeterministicSingleTimeSeries`. Resolving it replaces the old
/// guess-and-retry fallback in the bindings with an authoritative catalog
/// lookup that returns the concrete type that matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedType {
    /// Match exactly one concrete stored type.
    Concrete(TimeSeriesType),
    /// Match a stored `Deterministic` or `DeterministicSingleTimeSeries`.
    AbstractDeterministic,
}

impl RequestedType {
    /// Does the concrete stored type `concrete` satisfy this request?
    pub fn matches(self, concrete: TimeSeriesType) -> bool {
        match self {
            RequestedType::Concrete(t) => t == concrete,
            RequestedType::AbstractDeterministic => matches!(
                concrete,
                TimeSeriesType::Deterministic | TimeSeriesType::DeterministicSingleTimeSeries
            ),
        }
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
    pub name: String,
}

impl SingleTimeSeries {
    pub fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Duration,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Self {
        let length = data.length();
        Self {
            initial_timestamp,
            resolution,
            length,
            data,
            name: name.into(),
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
    pub name: String,
}

impl NonSequentialTimeSeries {
    pub fn new(
        timestamps: Vec<DateTime<Utc>>,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Result<Self, String> {
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
            name: name.into(),
        })
    }
}

/// A deterministic forecast: one complete horizon array per count window.
///
/// `data` has shape `[H, count, *E]` in row-major order, where
/// `H = horizon / resolution` and `*E` is the per-step element shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Deterministic {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Duration,
    pub horizon: Duration,
    pub interval: Duration,
    pub count: usize,
    /// Shape `[H, count, *E]`.
    pub data: TypedArray,
    pub name: String,
}

impl Deterministic {
    /// Construct, validating that `data.shape` matches the canonical layout.
    ///
    /// Returns `Err(String)` (mapped to `IntegrityError` by the store) if any
    /// dimension is inconsistent. Shape must be `[H, count, *E]` where
    /// `H = horizon / resolution` and `*E` is any trailing element dims.
    pub fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Duration,
        horizon: Duration,
        interval: Duration,
        count: usize,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        validate_positive_durations(resolution, horizon, interval)?;
        let h = compute_h(horizon, resolution)?;
        // Derive element dims from trailing shape after [H, count].
        if data.shape.len() < 2 {
            return Err(format!(
                "Deterministic: shape {:?} must have at least 2 dims [H, count]",
                data.shape
            ));
        }
        let elem_dims = &data.shape[2..];
        let expected_shape: Vec<usize> = std::iter::once(h)
            .chain(std::iter::once(count))
            .chain(elem_dims.iter().copied())
            .collect();
        if data.shape != expected_shape {
            return Err(format!(
                "Deterministic: expected shape {expected_shape:?}, got {:?}",
                data.shape
            ));
        }
        Ok(Self {
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            data,
            name: name.into(),
        })
    }
}

/// A probabilistic forecast: per-percentile, per-window horizon arrays.
///
/// `data` has shape `[num_percentiles, H, count, *E]` in row-major order.
#[derive(Debug, Clone, PartialEq)]
pub struct Probabilistic {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Duration,
    pub horizon: Duration,
    pub interval: Duration,
    pub count: usize,
    pub percentiles: Vec<f64>,
    /// Shape `[num_percentiles, H, count, *E]`.
    pub data: TypedArray,
    pub name: String,
}

impl Probabilistic {
    /// Construct, validating shape, percentile ordering, and positive durations.
    ///
    /// Returns `Err(String)` if any constraint is violated. Shape must be
    /// `[num_percentiles, H, count, *E]` where `H = horizon / resolution`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Duration,
        horizon: Duration,
        interval: Duration,
        count: usize,
        percentiles: Vec<f64>,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        validate_positive_durations(resolution, horizon, interval)?;
        if percentiles.is_empty() {
            return Err("Probabilistic: percentiles must be non-empty".to_string());
        }
        if percentiles.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("Probabilistic: percentiles must be strictly increasing".to_string());
        }
        let h = compute_h(horizon, resolution)?;
        let p = percentiles.len();
        if data.shape.len() < 3 {
            return Err(format!(
                "Probabilistic: shape {:?} must have at least 3 dims [P, H, count]",
                data.shape
            ));
        }
        let elem_dims = &data.shape[3..];
        let expected_shape: Vec<usize> = std::iter::once(p)
            .chain(std::iter::once(h))
            .chain(std::iter::once(count))
            .chain(elem_dims.iter().copied())
            .collect();
        if data.shape != expected_shape {
            return Err(format!(
                "Probabilistic: expected shape {expected_shape:?} \
                 (percentiles={p}, H={h}, count={count}), got {:?}",
                data.shape
            ));
        }
        Ok(Self {
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            percentiles,
            data,
            name: name.into(),
        })
    }
}

/// A scenarios forecast: per-scenario, per-window horizon arrays.
///
/// `data` has shape `[scenario_count, H, count, *E]` in row-major order.
#[derive(Debug, Clone, PartialEq)]
pub struct Scenarios {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Duration,
    pub horizon: Duration,
    pub interval: Duration,
    pub count: usize,
    pub scenario_count: usize,
    /// Shape `[scenario_count, H, count, *E]`.
    pub data: TypedArray,
    pub name: String,
}

impl Scenarios {
    /// Construct, validating shape against the canonical layout.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial_timestamp: DateTime<Utc>,
        resolution: Duration,
        horizon: Duration,
        interval: Duration,
        count: usize,
        scenario_count: usize,
        data: TypedArray,
        name: impl Into<String>,
    ) -> Result<Self, String> {
        validate_positive_durations(resolution, horizon, interval)?;
        let h = compute_h(horizon, resolution)?;
        let elem_dims: Vec<usize> = if data.shape.len() > 3 {
            data.shape[3..].to_vec()
        } else {
            vec![]
        };
        let expected_shape: Vec<usize> = std::iter::once(scenario_count)
            .chain(std::iter::once(h))
            .chain(std::iter::once(count))
            .chain(elem_dims)
            .collect();
        if data.shape != expected_shape {
            return Err(format!(
                "Scenarios: expected shape {expected_shape:?} \
                 (scenario_count={scenario_count}, H={h}, count={count}), got {:?}",
                data.shape
            ));
        }
        Ok(Self {
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count,
            scenario_count,
            data,
            name: name.into(),
        })
    }
}

/// Compute H = horizon / resolution, requiring an exact integer division > 0.
pub(crate) fn compute_h(horizon: Duration, resolution: Duration) -> Result<usize, String> {
    let h_ms = horizon.num_milliseconds();
    let r_ms = resolution.num_milliseconds();
    if r_ms <= 0 {
        return Err("resolution must be positive".to_string());
    }
    if h_ms % r_ms != 0 {
        return Err(format!(
            "horizon ({h_ms} ms) is not evenly divisible by resolution ({r_ms} ms)"
        ));
    }
    let h = (h_ms / r_ms) as usize;
    if h == 0 {
        return Err("horizon / resolution = 0 (horizon must be ≥ resolution)".to_string());
    }
    Ok(h)
}

/// Validate that resolution, horizon, and interval are all strictly positive.
fn validate_positive_durations(
    resolution: Duration,
    horizon: Duration,
    interval: Duration,
) -> Result<(), String> {
    let check = |d: Duration, name: &str| {
        if d.num_milliseconds() <= 0 {
            Err(format!("{name} must be strictly positive"))
        } else {
            Ok(())
        }
    };
    check(resolution, "resolution")?;
    check(horizon, "horizon")?;
    check(interval, "interval")?;
    Ok(())
}

/// Runtime variant container for all supported time-series types.
///
/// `DeterministicSingleTimeSeries` is synthesized into `Deterministic` on
/// read; there is no separate `DeterministicSingleTimeSeries` variant here.
#[derive(Debug, Clone, PartialEq)]
pub enum TimeSeriesData {
    SingleTimeSeries(SingleTimeSeries),
    NonSequentialTimeSeries(NonSequentialTimeSeries),
    Deterministic(Deterministic),
    Probabilistic(Probabilistic),
    Scenarios(Scenarios),
}

impl TimeSeriesData {
    pub fn time_series_type(&self) -> TimeSeriesType {
        match self {
            TimeSeriesData::SingleTimeSeries(_) => TimeSeriesType::SingleTimeSeries,
            TimeSeriesData::NonSequentialTimeSeries(_) => TimeSeriesType::NonSequentialTimeSeries,
            TimeSeriesData::Deterministic(_) => TimeSeriesType::Deterministic,
            TimeSeriesData::Probabilistic(_) => TimeSeriesType::Probabilistic,
            TimeSeriesData::Scenarios(_) => TimeSeriesType::Scenarios,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => &s.name,
            TimeSeriesData::NonSequentialTimeSeries(s) => &s.name,
            TimeSeriesData::Deterministic(d) => &d.name,
            TimeSeriesData::Probabilistic(p) => &p.name,
            TimeSeriesData::Scenarios(s) => &s.name,
        }
    }

    pub fn as_single(&self) -> Option<&SingleTimeSeries> {
        match self {
            TimeSeriesData::SingleTimeSeries(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_non_sequential(&self) -> Option<&NonSequentialTimeSeries> {
        match self {
            TimeSeriesData::NonSequentialTimeSeries(s) => Some(s),
            _ => None,
        }
    }

    /// Access the inner [`Deterministic`] forecast, if present.
    ///
    /// Also returns `Some` for a `DeterministicSingleTimeSeries` read, since
    /// that is synthesized into `Deterministic` by the store.
    pub fn as_deterministic(&self) -> Option<&Deterministic> {
        match self {
            TimeSeriesData::Deterministic(d) => Some(d),
            _ => None,
        }
    }

    /// Access the inner [`Probabilistic`] forecast, if present.
    pub fn as_probabilistic(&self) -> Option<&Probabilistic> {
        match self {
            TimeSeriesData::Probabilistic(p) => Some(p),
            _ => None,
        }
    }

    /// Access the inner [`Scenarios`] forecast, if present.
    pub fn as_scenarios(&self) -> Option<&Scenarios> {
        match self {
            TimeSeriesData::Scenarios(s) => Some(s),
            _ => None,
        }
    }
}
