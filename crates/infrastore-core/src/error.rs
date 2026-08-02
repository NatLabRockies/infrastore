use thiserror::Error;

pub type Result<T> = std::result::Result<T, TimeSeriesError>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TimeSeriesError {
    #[error("time series not found")]
    NotFound,

    #[error("a time series with the same key already exists")]
    DuplicateTimeSeries,

    /// An association with the same identity already exists: the
    /// `(component_id, attribute_id)` pair for a supplemental-attribute
    /// attachment, or the ordered `(parent_id, child_id)` pair for a
    /// parent/child edge. Type names are not part of either identity, so the
    /// same pair under different type names still collides.
    ///
    /// The payload names the offending pair in that relationship's own
    /// vocabulary; it is a human-readable message, not a parseable encoding.
    #[error("duplicate association: {0}")]
    DuplicateAssociation(String),

    #[error("invalid parameter: {0}")]
    InvalidParameter(String),

    #[error("integrity check failed: {0}")]
    IntegrityError(String),

    #[error(
        "store was written in on-disk format {found}, but this build reads {expected}; \
         the formats are incompatible and no in-place upgrade is available"
    )]
    IncompatibleFormat {
        found: String,
        expected: &'static str,
    },

    #[error(
        "the HDF5 file and its catalog carry different generation stamps \
         ({h5} vs {sqlite}); they are halves of two different saves, most likely \
         because a save was interrupted between writing the two files or because \
         one of them was copied without the other"
    )]
    MismatchedArtifact { h5: String, sqlite: String },

    #[error("store is read-only")]
    ReadOnlyStore,

    #[error("connection error: {0}")]
    ConnectionError(String),

    #[error("forecast parameters are incompatible with existing forecasts")]
    IncompatibleForecast,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
