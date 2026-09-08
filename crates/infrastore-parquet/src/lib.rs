//! Apache Parquet export and import for infrastore time series.
//!
//! A crate of its own rather than a feature on `infrastore-core`, so the Arrow
//! dependency tree is visible in the workspace graph instead of hiding behind a
//! feature flag, and so core's feature surface stays flat. `infrastore-cli`
//! depends on it through its own `parquet` feature, which is **on by default**
//! so the shipped binary carries Parquet; the library crates (`infrastore-core`,
//! `infrastore-py`, `infrastore-ffi`) never reach it.
//!
//! # Normalized, partitioned
//!
//! Not one file per series -- a store with thousands of series would become
//! thousands of files. And not one table with the catalog row beside every
//! value either: the store is content-addressed, so a thousand components
//! sharing one profile hold **one** array, and a denormalized table would write
//! that profile a thousand times. Parquet's compression does not find repeats
//! across pages, so the file really would be a thousand times larger.
//!
//! So the layout is normalized the way the store is. Per partition, two files
//! sharing a stem:
//!
//! - `<stem>.values.parquet` — every distinct array once, one row per value.
//! - `<stem>.series.parquet` — one catalog row per series, naming the array it
//!   reads.
//!
//! Both are keyed by [`ArrayKey`][table::ArrayKey], the pair
//! `(data_hash, time_axis)`, and sorted by it, so the import walks them as a
//! merge join. `data_hash` alone will not do: it covers the array bytes and not
//! the time axis, and the same profile on two anchors is one stored array with
//! two different timestamp columns.
//!
//! A partition is a `(time_series_type, value type, time_reference)` triple,
//! because those three cannot vary inside one table without nullable or
//! ill-typed columns — which is what makes **every column required**.
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
//! for partition in &report.partitions {
//!     println!(
//!         "{}: {} series over {} arrays",
//!         partition.stem, partition.series, partition.arrays
//!     );
//! }
//! # Ok::<(), infrastore_core::TimeSeriesError>(())
//! ```
//!
//! Python's `to_arrow()` / `from_arrow()` are **not** this format: they are
//! per-series, in-memory conveniences. The relationship is one sentence -- a
//! series file's columns are `to_arrow()`'s metadata keys turned into columns,
//! and the values file is its two columns keyed by the array -- and nothing
//! depends on the two agreeing.
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

pub use read::{
    ImportOptions, ImportedSeries, SeriesSink, parquet_files, read_file, read_file_with,
};
pub use write::{ExportReport, WrittenPartition, check_destination, write_partitions};

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
