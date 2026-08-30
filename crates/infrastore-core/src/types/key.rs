use serde::{Deserialize, Serialize};

use super::metadata::{Features, OwnerCategory};
use super::period::Period;
use super::time_series::TimeSeriesType;

/// The tuple the catalog files a row under, matching its uniqueness constraint
/// `(owner_id, owner_category, time_series_type, name, resolution, interval,
/// features)`.
///
/// Owner identity is the pair `(owner_id, owner_category)`: component and
/// supplemental-attribute id streams are independent, so the category
/// disambiguates an `owner_id` reused across the two.
///
/// `resolution` is `Option` because the catalog column is nullable
/// (`NonSequentialTimeSeries` has no resolution).
///
/// This is **not** an address. A caller names a series by its association
/// [`crate::TimeSeriesId`] — recovered from attributes by
/// [`crate::Store::list_metadata`] — and every read, removal and rename takes
/// that id. An identity is how a row is filed
/// and de-duplicated, which is why it stays internal to the write path.
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
