//! Apache Parquet export and import for infrastore time series.
//!
//! A crate of its own rather than a feature on `infrastore-core`, so the Arrow
//! dependency tree is visible in the workspace graph instead of hiding behind a
//! feature flag, and so core's feature surface stays flat. Nothing in the
//! default build reaches it: `infrastore-cli` depends on it behind its own
//! `parquet` feature, which is off.
//!
//! # Long tables, partitioned
//!
//! Not one file per series -- a store with thousands of series would become
//! thousands of files, which defeats every reader worth exporting for. Instead
//! **many series per file, one row per value**, with every catalog column a
//! table column, so a reader opens the directory and has the whole row without
//! attaching the SQLite catalog.
//!
//! Three things cannot vary inside one table without nullable or ill-typed
//! columns: the set of key columns, the Arrow type of `value`, and the zone of
//! `timestamp`. So a selection is partitioned by that triple ([`partition`]) and
//! written one file per part, which is what makes **every column required**.
//!
//! ```no_run
//! use std::path::Path;
//! use infrastore_core::{ListFilter, ReadWindow, open_store};
//!
//! let store = open_store(Path::new("demo.h5"), true)?;
//! let rows = store.list_metadata(ListFilter::new())?;
//! let ids: Vec<_> = rows.iter().filter_map(|r| r.id).collect();
//! let values = store.read_by_ids(&ids, ReadWindow::full())?;
//! let pairs: Vec<_> = rows.into_iter().zip(values).collect();
//!
//! let report = infrastore_parquet::write_partitions(Path::new("out"), &pairs)?;
//! for file in &report.files {
//!     let back = infrastore_parquet::read_file(&file.path, &Default::default())?;
//!     assert_eq!(back.len(), file.series);
//! }
//! # Ok::<(), infrastore_core::TimeSeriesError>(())
//! ```
//!
//! Python's `to_arrow()` / `from_arrow()` are **not** this format: they are
//! per-series, in-memory conveniences. The relationship is one sentence -- a long
//! table's columns are `to_arrow()`'s schema-metadata keys turned into columns --
//! and nothing depends on the two agreeing.
//!
//! # Errors
//!
//! Everything here reports through [`infrastore_core::TimeSeriesError`] rather
//! than an error enum of its own, per the workspace convention. Arrow and
//! Parquet errors are folded onto the existing variants: an error that describes
//! the *data* becomes
//! [`InvalidParameter`][infrastore_core::TimeSeriesError::InvalidParameter], and
//! one that describes the *file* becomes
//! [`Io`][infrastore_core::TimeSeriesError::Io]. Neither library's own error type
//! leaks, so a caller matching on `TimeSeriesError` needs no new arm.

pub mod partition;
pub mod read;
pub mod schema;
pub mod table;
pub mod write;

pub use read::{ImportOptions, ImportedSeries, parquet_files, read_file};
pub use write::{ExportReport, WrittenFile, write_partitions};

use infrastore_core::TimeSeriesError;

/// This crate's result type — the core's, so nothing new crosses the boundary.
pub type Result<T> = std::result::Result<T, TimeSeriesError>;

/// Something about the data this crate cannot represent: a forecast where a
/// static table is wanted, a value shape Arrow has no type for, a buffer whose
/// length disagrees with its dtype.
///
/// `InvalidParameter` rather than `IntegrityError` because every such case is
/// reachable from a caller's input, which is what the style guide reserves
/// `InvalidParameter` for.
pub(crate) fn unsupported(message: impl Into<String>) -> TimeSeriesError {
    TimeSeriesError::InvalidParameter(message.into())
}

/// Fold an Arrow error onto `InvalidParameter`.
///
/// Arrow's errors here come from building a schema or an array out of values
/// this crate was handed, so they are statements about the data. The message
/// keeps its `arrow:` prefix so a reader can tell which library produced it.
pub(crate) fn arrow_err(e: arrow::error::ArrowError) -> TimeSeriesError {
    TimeSeriesError::InvalidParameter(format!("arrow: {e}"))
}

/// Fold a Parquet error onto `Io` when it is one, and `InvalidParameter`
/// otherwise.
///
/// The distinction is worth keeping: a full disk and a column type the reader
/// cannot decode are different failures, and a caller that retries on `Io`
/// should not retry on a malformed file.
pub(crate) fn parquet_err(e: parquet::errors::ParquetError) -> TimeSeriesError {
    match e {
        parquet::errors::ParquetError::External(inner) => {
            match inner.downcast::<std::io::Error>() {
                Ok(io) => TimeSeriesError::Io(*io),
                Err(other) => TimeSeriesError::InvalidParameter(format!("parquet: {other}")),
            }
        }
        other => TimeSeriesError::InvalidParameter(format!("parquet: {other}")),
    }
}
