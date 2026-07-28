//! Core types, storage, and metadata for `infrastore`.
//!
//! Static time-series types are available through [`TimeSeriesData`].

pub mod error;
pub mod reader;
pub mod storage;
pub mod store;
pub mod types;
pub mod version;

// Implementation-detail modules. The intended public surface is the root
// re-exports below; these modules hold the catalog store and hashing internals
// (`MetadataStore`, `MetadataFilter`, the association identity/family types, the
// feature-set cache, the transaction-taking free functions, and the hashing
// helpers), which are not part of the supported API.
pub(crate) mod hash;
pub(crate) mod metadata;

pub use error::{Result, TimeSeriesError};
// The two hashing utilities a binding genuinely needs: `array_hash` to
// content-address an array and `hash_hex` to render a 32-byte hash as hex.
pub use hash::{array_hash, hash_hex};
pub use metadata::{
    ForecastSummaryRow, ParentChildAssociation, ParentChildFilter, StaticSummaryRow,
    SupplementalAttributeAssociation, SupplementalAttributeFilter, SupplementalAttributeSummaryRow,
};
pub use reader::{ForecastEntry, ForecastReader, StaticGroup, StaticReader, WindowSlot};
pub use storage::{CompactionReport, Compression, IntegrityReport};
pub use store::{
    AddRequest, BulkAdd, ForecastParameters, ListFilter, StaticConsistency, Store,
    TimeSeriesCounts, TimeSeriesCountsDetailed,
};
pub use types::{
    array::{Dtype, Element, TypedArray},
    key::{
        ForecastTimeSeriesKey, KeyIdentity, NonSequentialTimeSeriesKey, SingleTimeSeriesKey,
        TimeSeriesKey,
    },
    metadata::{
        FeatureValue, Features, OwnerCategory, RESERVED_FEATURE_NAMES, TimeSeriesMetadata,
        is_reserved_feature_name, validate_features,
    },
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
