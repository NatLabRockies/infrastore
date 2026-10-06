//! The normalized, partitioned table layout infrastore exports to, independent
//! of the container that holds it.
//!
//! Per `(time_series_type, value type, time_reference)` partition, three tables:
//! a **values** table holding every distinct array once, one row per value, a
//! **series** table holding one catalog row per series, and an **arrays** table
//! spelling each array's key `(data_hash, time_axis)` once under an integer
//! `id` — which is what the other two carry, as `array_id`, so the key's text is
//! not repeated on every value row. Normalized because the store is
//! content-addressed: a thousand components sharing one profile hold one array.
//! See `docs/src/reference/parquet-format.md` for the full description.
//!
//! Two containers carry it: `infrastore-parquet` (three files per partition,
//! behind the Arrow dependency tree) and [`sqlite`] (three tables per partition,
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
