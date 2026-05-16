use chrono::Duration;

use super::metadata::Features;
use super::time_series::TimeSeriesType;

/// Logical handle returned from `add_time_series` and `list_time_series_keys`.
/// Carries enough state to look the time series up again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeSeriesKey {
    pub owner_id: i64,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub resolution: Option<Duration>,
    pub features: Features,
}
