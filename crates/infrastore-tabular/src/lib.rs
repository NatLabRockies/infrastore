//! The normalized, partitioned table layout infrastore exports to, independent
//! of the container that holds it.
//!
//! Per `(time_series_type, value type, time_reference)` partition, two tables:
//! a **values** table holding every distinct array once, one row per value, and
//! a **series** table holding one catalog row per series, joined on the array
//! key `(data_hash, time_axis)`. Normalized because the store is
//! content-addressed: a thousand components sharing one profile hold one array.
//! See `docs/src/reference/parquet-format.md` for the full description.
//!
//! Two containers carry it: `infrastore-parquet` (two files per partition,
//! behind the Arrow dependency tree) and [`sqlite`] (two tables per partition,
//! in this crate because SQLite is already linked by the core's catalog).
//!
//! Errors report through [`infrastore_core::TimeSeriesError`], as everywhere in
//! the workspace.

pub mod export;
pub mod import;
pub mod layout;
pub mod partition;
pub mod schema;
pub mod sqlite;

pub use import::{ImportOptions, ImportedSeries, SeriesSink};

use infrastore_core::TimeSeriesError;

/// This crate's result type — the core's, so nothing new crosses the boundary.
pub type Result<T> = std::result::Result<T, TimeSeriesError>;

/// Something about the data the layout cannot represent. `InvalidParameter`
/// because every such case is reachable from a caller's input.
pub(crate) fn unsupported(message: impl Into<String>) -> TimeSeriesError {
    TimeSeriesError::InvalidParameter(message.into())
}
