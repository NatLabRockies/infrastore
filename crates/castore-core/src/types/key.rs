use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::metadata::{Features, OwnerCategory, TimeSeriesMetadata};
use super::period::Period;
use super::time_series::TimeSeriesType;
use crate::error::{Result, TimeSeriesError};

/// The identifying tuple shared by every [`TimeSeriesKey`] variant. This — and
/// only this — determines key equality, and is what the catalog looks up: it
/// matches the metadata uniqueness constraint
/// `(owner_id, owner_category, time_series_type, name, resolution, interval,
/// features)`.
///
/// Owner identity is the pair `(owner_id, owner_category)`: component and
/// supplemental-attribute id streams are independent, so the category
/// disambiguates an `owner_id` reused across the two.
///
/// `resolution` is `Option` because the catalog column is nullable
/// (`NonSequentialTimeSeries` has no resolution); the per-variant constructors
/// of [`TimeSeriesKey`] enforce which series types may leave it unset.
///
/// `interval` is part of the identity (matching InfrastructureSystems.jl): two
/// forecasts of one variable at the same resolution but different intervals are
/// distinct series. It is `Some` for every forecast type and `None` for the
/// static types, which never carry an interval.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyIdentity {
    pub owner_id: i64,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub resolution: Option<Period>,
    pub interval: Option<Period>,
    pub features: Features,
}

/// Identifying key plus the descriptive snapshot for a `SingleTimeSeries`. The
/// resolution is always present (a `SingleTimeSeries` is a regular grid).
///
/// Unlike [`TimeSeriesKey`], the snapshot structs compare by *all* fields
/// (identity + descriptive): they are point-in-time views, and full equality is
/// what a test comparing two snapshots wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SingleTimeSeriesKey {
    pub identity: KeyIdentity,
    pub initial_timestamp: DateTime<Utc>,
    pub length: usize,
}

/// Identifying key plus the descriptive snapshot for a
/// `NonSequentialTimeSeries`. Its timestamps are irregular, so the snapshot is
/// just `length`; the actual timestamps are read from the data. The resolution
/// is always absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonSequentialTimeSeriesKey {
    pub identity: KeyIdentity,
    pub length: usize,
}

/// Identifying key plus the descriptive snapshot for a forecast
/// (`Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, or
/// `Scenarios`). The resolution is always present.
///
/// `interval` is part of the identity, so it lives in [`KeyIdentity`] rather
/// than as a descriptive field here; read it via [`Self::interval`]. `horizon`
/// and `count`, by contrast, are descriptive and excluded from equality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForecastTimeSeriesKey {
    pub identity: KeyIdentity,
    pub initial_timestamp: DateTime<Utc>,
    pub horizon: Period,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
                interval: None,
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
                interval: None,
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
                interval: Some(interval.into()),
                features,
            },
            initial_timestamp,
            horizon: horizon.into(),
            count,
        }
    }

    /// The forecast window interval. Always present for a forecast key (it is
    /// part of the identity); panics only if a key was hand-built with a `None`
    /// interval, which the constructors never do.
    pub fn interval(&self) -> Period {
        self.identity
            .interval
            .expect("forecast key identity always carries an interval")
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

    /// The forecast interval, part of the identity. `Some` for forecast keys,
    /// `None` for static (`SingleTimeSeries`/`NonSequentialTimeSeries`) keys.
    pub fn interval(&self) -> Option<Period> {
        self.identity().interval
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

impl std::hash::Hash for TimeSeriesKey {
    /// Hashes the [`KeyIdentity`] only, mirroring the identity-only [`PartialEq`]
    /// above. This upholds the `Eq`/`Hash` contract (equal keys hash equal): two
    /// keys with the same identity but differing descriptive snapshots compare
    /// equal, so they must — and do — hash equal, letting a key stay a reliable
    /// `HashMap`/`HashSet` handle.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::types::period::Period;

    fn hash_of(k: &TimeSeriesKey) -> u64 {
        let mut h = DefaultHasher::new();
        k.hash(&mut h);
        h.finish()
    }

    #[test]
    fn keys_are_usable_in_a_hash_set() {
        let a = TimeSeriesKey::Single(SingleTimeSeriesKey::new(
            1,
            OwnerCategory::Component,
            "load".into(),
            Period::fixed(chrono::Duration::hours(1)),
            Features::new(),
            Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap(),
            4,
        ));
        let b = TimeSeriesKey::Single(SingleTimeSeriesKey::new(
            2,
            OwnerCategory::Component,
            "load".into(),
            Period::fixed(chrono::Duration::hours(1)),
            Features::new(),
            Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap(),
            4,
        ));
        let mut set = HashSet::new();
        set.insert(a.clone());
        set.insert(b.clone());
        set.insert(a.clone());
        assert_eq!(set.len(), 2);
        assert!(set.contains(&a));
    }

    #[test]
    fn equal_identity_different_snapshot_hashes_equal() {
        // Same identity, different descriptive `length` snapshot.
        let base = |length: usize| {
            TimeSeriesKey::Single(SingleTimeSeriesKey::new(
                1,
                OwnerCategory::Component,
                "load".into(),
                Period::fixed(chrono::Duration::hours(1)),
                Features::new(),
                Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap(),
                length,
            ))
        };
        let a = base(4);
        let b = base(8);
        assert_eq!(a, b, "identity-only equality ignores the snapshot");
        assert_eq!(hash_of(&a), hash_of(&b), "equal keys must hash equal");
    }
}
