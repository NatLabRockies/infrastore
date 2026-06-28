//! Core types, storage, and metadata for `time-series-store`.
//!
//! Static time-series types are available through [`TimeSeriesData`].

pub mod error;
pub mod hash;
pub mod metadata;
pub mod reader;
pub mod storage;
pub mod store;
pub mod types;
pub mod version;

pub use error::{Result, TimeSeriesError};
pub use metadata::{ForecastSummaryRow, StaticSummaryRow};
pub use reader::{ForecastEntry, ForecastReader, StaticGroup, StaticReader, WindowSlot};
pub use storage::{CompactionReport, Compression, IntegrityReport};
pub use store::{
    AddRequest, ForecastParameters, ListFilter, Store, TimeSeriesCounts, TimeSeriesCountsDetailed,
};
pub use types::{
    array::{Dtype, TypedArray},
    key::{
        ForecastTimeSeriesKey, KeyIdentity, NonSequentialTimeSeriesKey, SingleTimeSeriesKey,
        TimeSeriesKey,
    },
    metadata::{FeatureValue, Features, OwnerCategory, TimeSeriesMetadata},
    period::Period,
    time_series::{
        Deterministic, NonSequentialTimeSeries, Probabilistic, RequestedType, Scenarios,
        SingleTimeSeries, TimeSeriesData, TimeSeriesType,
    },
};
pub use version::DATA_FORMAT_VERSION;

/// Create a new store.
///
/// If `in_memory` is true, no filesystem I/O occurs and `path` is ignored.
/// Otherwise a catalog SQLite database is created at `path` (NetCDF persistence
/// is wired in M1).
pub fn create_store(path: Option<&std::path::Path>, in_memory: bool) -> Result<Store> {
    Store::create(path, in_memory)
}

/// Create a new store with an explicit NetCDF compression policy.
///
/// Behaves like [`create_store`] but applies `compression` to data variables
/// (ignored for `in_memory` stores).
pub fn create_store_with_compression(
    path: Option<&std::path::Path>,
    in_memory: bool,
    compression: Compression,
) -> Result<Store> {
    Store::create_with_compression(path, in_memory, compression)
}

/// Open an existing store from disk.
pub fn open_store(path: &std::path::Path, read_only: bool) -> Result<Store> {
    Store::open(path, read_only)
}
