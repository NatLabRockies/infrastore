use chrono::{DateTime, Utc};

use super::metadata::{Features, OwnerCategory, TimeSeriesMetadata};
use super::period::Period;
use super::time_series::TimeSeriesType;
use crate::error::{Result, TimeSeriesError};

/// The identifying tuple shared by every [`TimeSeriesKey`] variant. This — and
/// only this — determines key equality, and is what the catalog looks up: it
/// matches the metadata uniqueness constraint
/// `(owner_id, owner_category, time_series_type, name, resolution, features)`.
///
/// Owner identity is the pair `(owner_id, owner_category)`: component and
/// supplemental-attribute id streams are independent, so the category
/// disambiguates an `owner_id` reused across the two.
///
/// `resolution` is `Option` because the catalog column is nullable
/// (`NonSequentialTimeSeries` has no resolution); the per-variant constructors
/// of [`TimeSeriesKey`] enforce which series types may leave it unset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyIdentity {
    pub owner_id: i64,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub resolution: Option<Period>,
    pub features: Features,
}

/// Identifying key plus the descriptive snapshot for a `SingleTimeSeries`. The
/// resolution is always present (a `SingleTimeSeries` is a regular grid).
#[derive(Debug, Clone)]
pub struct SingleTimeSeriesKey {
    pub identity: KeyIdentity,
    pub initial_timestamp: DateTime<Utc>,
    pub length: usize,
}

/// Identifying key plus the descriptive snapshot for a
/// `NonSequentialTimeSeries`. Its timestamps are irregular, so the snapshot is
/// just `length`; the actual timestamps are read from the data. The resolution
/// is always absent.
#[derive(Debug, Clone)]
pub struct NonSequentialTimeSeriesKey {
    pub identity: KeyIdentity,
    pub length: usize,
}

/// Identifying key plus the descriptive snapshot for a forecast
/// (`Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, or
/// `Scenarios`). The resolution is always present.
#[derive(Debug, Clone)]
pub struct ForecastTimeSeriesKey {
    pub identity: KeyIdentity,
    pub initial_timestamp: DateTime<Utc>,
    pub horizon: Period,
    pub interval: Period,
    pub count: usize,
}

/// Logical handle returned from `add_time_series`, `list_time_series_keys`, and
/// `resolve_forecast_key`. Carries the identity needed to look the series up
/// again, plus a per-variant descriptive snapshot (window/shape parameters).
///
/// Equality is **identity-only**: two keys with the same [`KeyIdentity`] are
/// equal even if their descriptive snapshots differ, so a key stays a reliable
/// handle. The descriptive fields are a point-in-time view and are deliberately
/// excluded from equality.
#[derive(Debug, Clone)]
pub enum TimeSeriesKey {
    Single(SingleTimeSeriesKey),
    NonSequential(NonSequentialTimeSeriesKey),
    Forecast(ForecastTimeSeriesKey),
}

impl SingleTimeSeriesKey {
    /// Build a `SingleTimeSeries` key. `resolution` is required (not `Option`),
    /// enforcing the invariant that a `SingleTimeSeries` always has one.
    pub fn new(
        owner_id: i64,
        owner_category: OwnerCategory,
        name: String,
        resolution: impl Into<Period>,
        features: Features,
        initial_timestamp: DateTime<Utc>,
        length: usize,
    ) -> Self {
        Self {
            identity: KeyIdentity {
                owner_id,
                owner_category,
                time_series_type: TimeSeriesType::SingleTimeSeries,
                name,
                resolution: Some(resolution.into()),
                features,
            },
            initial_timestamp,
            length,
        }
    }
}

impl NonSequentialTimeSeriesKey {
    /// Build a `NonSequentialTimeSeries` key. There is no `resolution`
    /// parameter, enforcing the invariant that it never carries one.
    pub fn new(
        owner_id: i64,
        owner_category: OwnerCategory,
        name: String,
        features: Features,
        length: usize,
    ) -> Self {
        Self {
            identity: KeyIdentity {
                owner_id,
                owner_category,
                time_series_type: TimeSeriesType::NonSequentialTimeSeries,
                name,
                resolution: None,
                features,
            },
            length,
        }
    }
}

impl ForecastTimeSeriesKey {
    /// Build a forecast key for the given concrete forecast `time_series_type`.
    /// `resolution` is required (not `Option`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner_id: i64,
        owner_category: OwnerCategory,
        time_series_type: TimeSeriesType,
        name: String,
        resolution: impl Into<Period>,
        features: Features,
        initial_timestamp: DateTime<Utc>,
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        count: usize,
    ) -> Self {
        Self {
            identity: KeyIdentity {
                owner_id,
                owner_category,
                time_series_type,
                name,
                resolution: Some(resolution.into()),
                features,
            },
            initial_timestamp,
            horizon: horizon.into(),
            interval: interval.into(),
            count,
        }
    }
}

impl TimeSeriesKey {
    /// The identifying tuple — the only thing the catalog looks up and the only
    /// thing that determines equality.
    pub fn identity(&self) -> &KeyIdentity {
        match self {
            TimeSeriesKey::Single(k) => &k.identity,
            TimeSeriesKey::NonSequential(k) => &k.identity,
            TimeSeriesKey::Forecast(k) => &k.identity,
        }
    }

    pub fn owner_id(&self) -> i64 {
        self.identity().owner_id
    }

    pub fn owner_category(&self) -> OwnerCategory {
        self.identity().owner_category
    }

    pub fn time_series_type(&self) -> TimeSeriesType {
        self.identity().time_series_type
    }

    pub fn name(&self) -> &str {
        &self.identity().name
    }

    pub fn resolution(&self) -> Option<Period> {
        self.identity().resolution
    }

    pub fn features(&self) -> &Features {
        &self.identity().features
    }

    /// Reconstruct the descriptive key for a stored association from its
    /// metadata row. This is the canonical row → key builder used by the listing
    /// and resolution paths. Returns [`TimeSeriesError::IntegrityError`] if a
    /// field required by the series type is missing from the row.
    pub fn from_metadata(m: &TimeSeriesMetadata) -> Result<Self> {
        let owner_id = m.owner_id;
        let owner_category = m.owner_category;
        let name = m.name.clone();
        let features = m.features.clone();
        let missing = |field: &str| -> TimeSeriesError {
            TimeSeriesError::IntegrityError(format!(
                "{} metadata missing {field}",
                m.time_series_type.as_str()
            ))
        };

        match m.time_series_type {
            TimeSeriesType::SingleTimeSeries => {
                Ok(TimeSeriesKey::Single(SingleTimeSeriesKey::new(
                    owner_id,
                    owner_category,
                    name,
                    m.resolution.ok_or_else(|| missing("resolution"))?,
                    features,
                    m.initial_timestamp
                        .ok_or_else(|| missing("initial_timestamp"))?,
                    m.length.ok_or_else(|| missing("length"))?,
                )))
            }
            TimeSeriesType::NonSequentialTimeSeries => Ok(TimeSeriesKey::NonSequential(
                NonSequentialTimeSeriesKey::new(
                    owner_id,
                    owner_category,
                    name,
                    features,
                    m.length.ok_or_else(|| missing("length"))?,
                ),
            )),
            TimeSeriesType::Deterministic
            | TimeSeriesType::DeterministicSingleTimeSeries
            | TimeSeriesType::Probabilistic
            | TimeSeriesType::Scenarios => Ok(TimeSeriesKey::Forecast(ForecastTimeSeriesKey::new(
                owner_id,
                owner_category,
                m.time_series_type,
                name,
                m.resolution.ok_or_else(|| missing("resolution"))?,
                features,
                m.initial_timestamp
                    .ok_or_else(|| missing("initial_timestamp"))?,
                m.horizon.ok_or_else(|| missing("horizon"))?,
                m.interval.ok_or_else(|| missing("interval"))?,
                m.count.ok_or_else(|| missing("count"))?,
            ))),
        }
    }
}

impl PartialEq for TimeSeriesKey {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl Eq for TimeSeriesKey {}
