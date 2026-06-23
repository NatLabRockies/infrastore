use chrono::Duration;

use super::metadata::{Features, OwnerCategory};
use super::time_series::TimeSeriesType;

/// Logical handle returned from `add_time_series` and `list_time_series_keys`.
/// Carries enough state to look the time series up again.
///
/// Owner identity is the pair `(owner_id, owner_category)`: component and
/// supplemental-attribute id streams are independent, so the category
/// disambiguates an `owner_id` that is reused across the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeSeriesKey {
    pub owner_id: i64,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub resolution: Option<Duration>,
    pub features: Features,
}
