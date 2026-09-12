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
mod write_buffer;

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
    DecodedValues, LinearFunction, QuadraticFunction, StepFunction, XyPoint, decode,
    element_type_of, encode, encode_as,
};
pub use error::{Result, TimeSeriesError};
// The three hashing utilities a binding genuinely needs: `array_hash` to
// content-address an array, `timestamps_hash` to content-address the time axis
// an irregular series sits on -- the catalog's own key for it, and the only
// thing that tells two irregular series with identical values on different axes
// apart -- and `hash_hex`/`hash_from_hex` to render a 32-byte hash as hex and back.
pub use hash::{array_hash, hash_from_hex, hash_hex, timestamps_hash};
pub use metadata::{
    ForecastSummaryRow, ParentChildAssociation, ParentChildFilter, StaticSummaryRow,
    SupplementalAttributeAssociation, SupplementalAttributeFilter, SupplementalAttributeSummaryRow,
};
pub use reader::{ForecastEntry, ForecastReader, StaticGroup, StaticReader, WindowSlot};
pub use storage::{ArrayLocation, CompactionReport, Compression, IntegrityReport};
pub use store::{
    AddRequest, BulkAdd, CatalogMode, ForecastParameters, ListFilter,
    RESERVED_STORE_ATTRIBUTE_PREFIX, ReadWindow, StaticConsistency, Store, TimeSeriesCounts,
    TimeSeriesCountsDetailed, TransformOutcome, TransformPolicy, catalog_sqlite_path,
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
        Descriptors, Deterministic, NonSequentialTimeSeries, PersistentTimeSeries, Probabilistic,
        Scenarios, SingleTimeSeries, TimeSeriesData, TimeSeriesType,
    },
};
pub use version::{Compat, DATA_FORMAT_VERSION, MIN_UPGRADABLE_VERSION, compatibility};
