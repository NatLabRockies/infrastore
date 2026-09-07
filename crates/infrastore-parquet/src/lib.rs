//! Apache Parquet export and import for infrastore time series.
//!
//! A crate of its own rather than a feature on `infrastore-core`, so the Arrow
//! dependency tree is visible in the workspace graph instead of hiding behind a
//! feature flag, and so core's feature surface stays flat. Nothing in the
//! default build reaches it: `infrastore-cli` depends on it behind its own
//! `parquet` feature, which is off.
//!
//! # One schema, two producers
//!
//! Python's `to_arrow()` and this crate write the **same table** for the same
//! series — two columns, `timestamp` and `value`, plus a footer of UTF-8
//! key/value pairs — so a Parquet file has one shape whichever wrote it, and one
//! import reads both. [`schema`] is the definition of that footer and the place
//! to look before changing either producer.
//!
//! ```no_run
//! use std::path::Path;
//! use infrastore_core::{ListFilter, ReadWindow, open_store};
//!
//! let store = open_store(Path::new("demo.h5"), true)?;
//! let row = store.list_metadata(ListFilter::new().name("load"))?.remove(0);
//! let id = row.id.expect("a catalog row carries its id");
//! let data = store.read_by_id(id, ReadWindow::full())?;
//! infrastore_parquet::write_series(Path::new("load.parquet"), &row, &data)?;
//! # Ok::<(), infrastore_core::TimeSeriesError>(())
//! ```
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

pub mod export;
pub mod schema;

pub use export::{TIMESTAMP_COLUMN, VALUE_COLUMN, record_batch, timestamp_data_type, write_series};

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
