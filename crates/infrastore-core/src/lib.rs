//! Core types, storage, and metadata for `infrastore`.
//!
//! Static time-series types are available through [`TimeSeriesData`].

pub mod codec;
pub mod error;
pub mod reader;
pub mod storage;
pub mod store;
pub mod types;
pub mod version;

// Implementation-detail modules. The intended public surface is the root
// re-exports below; these modules hold the catalog store, hashing, and
// timestamp-encoding internals (`MetadataStore`, `MetadataFilter`, the
// association identity/family types, the shared-set cache, the
// transaction-taking free functions, the hashing helpers, and the canonical
// timestamp codec), which are not part of the supported API.
pub(crate) mod hash;
pub(crate) mod metadata;
// OpenAPI-row JSON serde for the two association catalogs (adds inherent
// `Store` methods; see the module docs). Crate-private: the `export_*` /
// `import_supplemental_attribute_associations_openapi` methods on `Store` are
// the supported public surface.
pub(crate) mod openapi;
pub(crate) mod timestamps;

pub use codec::{
    DecodedValues, LinearFunction, QuadraticFunction, StepFunction, XyPoint, decode, encode,
    encode_as,
};
pub use error::{Result, TimeSeriesError};
// The two hashing utilities a binding genuinely needs: `array_hash` to
// content-address an array and `hash_hex` to render a 32-byte hash as hex.
pub use hash::{array_hash, hash_hex};
pub use metadata::{
    ForecastSummaryRow, ParentChildAssociation, ParentChildFilter, StaticSummaryRow,
    SupplementalAttributeAssociation, SupplementalAttributeFilter, SupplementalAttributeSummaryRow,
};
pub use reader::{ForecastEntry, ForecastReader, StaticGroup, StaticReader, WindowSlot};
pub use storage::{ArrayLocation, CompactionReport, Compression, IntegrityReport};
pub use store::{
    AddRequest, BulkAdd, CatalogMode, ForecastParameters, ListFilter, ReadWindow,
    StaticConsistency, Store, TimeSeriesCounts, TimeSeriesCountsDetailed, TransformOutcome,
    TransformPolicy, catalog_sqlite_path,
};
pub use types::{
    array::{Dtype, Element, TypedArray},
    element_type::ElementType,
    id::TimeSeriesId,
    key::KeyIdentity,
    metadata::{
        FeatureValue, Features, OwnerCategory, RESERVED_FEATURE_NAMES, TimeSeriesMetadata,
        UnitSystem, is_reserved_feature_name, validate_features,
    },
    period::Period,
    time_reference::{TimeRange, TimeReference},
    time_series::{
        Descriptors, Deterministic, NonSequentialTimeSeries, Probabilistic, Scenarios,
        SingleTimeSeries, TimeSeriesData, TimeSeriesType,
    },
};
pub use version::{Compat, DATA_FORMAT_VERSION, MIN_UPGRADABLE_VERSION, compatibility};

/// Create a new store.
///
/// If `in_memory` is true, no filesystem I/O occurs and `path` is ignored.
/// Otherwise an HDF5 array file is created at `path` and a catalog SQLite
/// database at `<path>.sqlite`.
pub fn create_store(path: Option<&std::path::Path>, in_memory: bool) -> Result<Store> {
    Store::create(path, in_memory)
}

/// Create a new store with an explicit compression policy.
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

/// Create a new store with an explicit catalog placement.
///
/// Behaves like [`create_store_with_compression`] but decides whether the
/// catalog lives in `<path>.sqlite` or in RAM. See [`CatalogMode`].
pub fn create_store_with_catalog(
    path: Option<&std::path::Path>,
    in_memory: bool,
    compression: Compression,
    catalog: CatalogMode,
) -> Result<Store> {
    Store::create_with_catalog(path, in_memory, compression, catalog)
}

/// Create a store at `path`, discarding any artifact already there.
///
/// The destructive counterpart to [`create_store_with_catalog`], which refuses
/// an existing artifact. See [`Store::create_replacing`].
pub fn create_store_replacing(
    path: &std::path::Path,
    compression: Compression,
    catalog: CatalogMode,
) -> Result<Store> {
    Store::create_replacing(path, compression, catalog)
}

/// Open an existing store from disk.
pub fn open_store(path: &std::path::Path, read_only: bool) -> Result<Store> {
    Store::open(path, read_only)
}

/// Copy the artifact at `src` to `dest` and open the copy read-write.
///
/// The safe way to load a store and then change it: the original is never
/// opened for writing. See [`Store::open_copy`].
pub fn open_store_copy(
    src: &std::path::Path,
    dest: &std::path::Path,
    catalog: CatalogMode,
) -> Result<Store> {
    Store::open_copy(src, dest, catalog)
}

/// Open an existing store from disk with an explicit catalog placement.
///
/// See [`CatalogMode`]. With [`CatalogMode::InMemory`] the catalog file is read
/// into RAM and subsequent mutations reach disk only through
/// [`Store::persist_to`]; the HDF5 half is still opened in place.
pub fn open_store_with_catalog(
    path: &std::path::Path,
    read_only: bool,
    catalog: CatalogMode,
) -> Result<Store> {
    Store::open_with_catalog(path, read_only, catalog)
}
