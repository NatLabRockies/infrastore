use thiserror::Error;

pub type Result<T> = std::result::Result<T, TimeSeriesError>;

#[derive(Debug, Error)]
pub enum TimeSeriesError {
    #[error("time series not found")]
    NotFound,

    #[error("a time series with the same key already exists")]
    DuplicateTimeSeries,

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
