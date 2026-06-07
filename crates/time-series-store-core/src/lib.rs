//! Core types, storage, and metadata for `time-series-store`.
//!
//! Static time-series types are available through [`TimeSeriesData`].

pub mod error;
pub mod hash;
pub mod metadata;
pub mod storage;
pub mod store;
pub mod types;
pub mod version;

pub use error::{Result, TimeSeriesError};
pub use storage::{CompactionReport, IntegrityReport};
pub use store::{AddRequest, ForecastParameters, ListFilter, Store, TimeSeriesCounts};
pub use types::{
    array::{Dtype, TypedArray},
    key::TimeSeriesKey,
    metadata::{FeatureValue, Features, OwnerCategory, TimeSeriesMetadata},
    time_series::{NonSequentialTimeSeries, SingleTimeSeries, TimeSeriesData, TimeSeriesType},
};
pub use version::DATA_FORMAT_VERSION;

/// Create a new store.
///
/// If `in_memory` is true, no filesystem I/O occurs and `path` is ignored.
/// Otherwise a sidecar SQLite database is created at `path` (NetCDF persistence
/// is wired in M1).
pub fn create_store(path: Option<&std::path::Path>, in_memory: bool) -> Result<Store> {
    Store::create(path, in_memory)
}

/// Open an existing store from disk.
pub fn open_store(path: &std::path::Path, read_only: bool) -> Result<Store> {
    Store::open(path, read_only)
}
