//! Writing one series to one Parquet file.
//!
//! The table is two columns, `timestamp` and `value`, plus the footer
//! [`crate::schema`] describes. Value column named `value` rather than after the
//! series so tables from different components concatenate without renaming; the
//! name rides in the footer with everything else that describes the values but
//! does not address them.
//!
//! One file per series, which is what `export --dir` already does for CSV and
//! what makes the footer a faithful copy of one catalog row.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BooleanArray, FixedSizeListArray, Float32Array, Float64Array, Int8Array, Int16Array,
    Int32Array, Int64Array, RecordBatch, TimestampMillisecondArray, UInt8Array, UInt16Array,
    UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use chrono::{DateTime, Utc};
use infrastore_core::{Dtype, TimeReference, TimeSeriesData, TimeSeriesMetadata, TypedArray};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::schema;
use crate::{Result, arrow_err, parquet_err, unsupported};

/// The value column's name. Fixed, so two exports concatenate.
pub const VALUE_COLUMN: &str = "value";
/// The timestamp column's name. For a `PersistentTimeSeries` these are
/// breakpoints rather than the instants the series has values at — the
/// `time_series_type` in the footer is what says so.
pub const TIMESTAMP_COLUMN: &str = "timestamp";

/// Build the one-series Arrow table: `timestamp`, `value`, and the row's footer.
///
/// Refuses anything but the three static types. A forecast is a different table
/// shape (a long form keyed by issue and target time) and is not part of this
/// version.
pub fn record_batch(row: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<RecordBatch> {
    let (instants, array) = static_parts(data)?;
    // The row's spelling, not the value object's: the two agree for a series
    // read out of a store, and the catalog is the authority when they do not.
    build_batch(
        &instants,
        row.time_reference.as_ref(),
        array,
        schema::metadata_for_row(row),
    )
}

/// Write [`record_batch`] to `path` as Parquet.
///
/// Zstd because these files are archival: a year of hourly doubles compresses
/// several times better than snappy at a write cost nobody notices next to the
/// HDF5 read that produced the values. Every Parquet reader handles both.
pub fn write_series(path: &Path, row: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<()> {
    let batch = record_batch(row, data)?;
    let file = File::create(path)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .build();
    let mut writer =
        ArrowWriter::try_new(file, batch.schema(), Some(props)).map_err(parquet_err)?;
    writer.write(&batch).map_err(parquet_err)?;
    // `close` is what writes the footer, so its error is not ignorable: without
    // it the file on disk is a headerless blob no reader will open.
    writer.close().map_err(parquet_err)?;
    Ok(())
}

/// The timestamps and values of one static series.
///
/// A `SingleTimeSeries` materializes its grid (calendar-aware for a monthly
/// resolution, which is why this cannot be `initial + k * resolution`); the two
/// irregular types hand back the vector they store. A `PersistentTimeSeries`
/// gives **one row per breakpoint, not per instant** — a step function is stored
/// sparsely and the table is that sparse form.
fn static_parts(data: &TimeSeriesData) -> Result<(Vec<DateTime<Utc>>, &TypedArray)> {
    match data {
        TimeSeriesData::SingleTimeSeries(s) => Ok((s.timestamps().collect(), &s.data)),
        TimeSeriesData::NonSequentialTimeSeries(s) => Ok((s.timestamps.clone(), &s.data)),
        TimeSeriesData::PersistentTimeSeries(s) => Ok((s.timestamps.clone(), &s.data)),
        other => Err(unsupported(format!(
            "Parquet export covers the static types; {} is a forecast",
            other.time_series_type().as_str()
        ))),
    }
}

fn build_batch(
    instants: &[DateTime<Utc>],
    reference: Option<&TimeReference>,
    array: &TypedArray,
    metadata: std::collections::BTreeMap<String, String>,
) -> Result<RecordBatch> {
    let timestamps = timestamp_array(instants, reference);
    let values = value_array(array)?;
    if values.len() != instants.len() {
        return Err(unsupported(format!(
            "{} timestamps but {} value rows",
            instants.len(),
            values.len()
        )));
    }
    // Both columns are non-nullable: the store has no nulls, and NaN is a value
    // rather than an absence. Declaring it keeps a foreign reader from having to
    // ask.
    let schema = Schema::new_with_metadata(
        vec![
            Field::new(TIMESTAMP_COLUMN, timestamps.data_type().clone(), false),
            Field::new(VALUE_COLUMN, values.data_type().clone(), false),
        ],
        metadata.into_iter().collect(),
    );
    RecordBatch::try_new(Arc::new(schema), vec![timestamps, values]).map_err(arrow_err)
}

/// The instants as `timestamp[ms, tz]`, in the series' own spelling.
///
/// Arrow's `timestamp(unit, tz)` is the same shape as the store's model — an
/// instant plus the spelling it was written in — so the mapping is total. The
/// unit is milliseconds, the precision every instant the store records is held
/// to, so nothing is widened or truncated.
///
/// | reference | Arrow type |
/// | --- | --- |
/// | `None`, `utc` | `timestamp[ms, tz=UTC]` |
/// | `zoneless` | `timestamp[ms]` (no zone) |
/// | `-07:00` | `timestamp[ms, tz=-07:00]` |
/// | `America/Denver` | `timestamp[ms, tz=America/Denver]` |
///
/// A zone name is passed through unresolved, as everywhere else in the core: the
/// core validates a zone's *shape* and never looks it up, and a reader with a tz
/// database is the layer that can say whether it exists.
fn timestamp_array(instants: &[DateTime<Utc>], reference: Option<&TimeReference>) -> ArrayRef {
    let zone: Option<String> = match reference {
        None | Some(TimeReference::Utc) => Some("UTC".to_string()),
        Some(TimeReference::Zoneless) => None,
        Some(r) => Some(r.as_storage_string()),
    };
    let millis: Vec<i64> = instants.iter().map(|t| t.timestamp_millis()).collect();
    let array = TimestampMillisecondArray::from(millis);
    Arc::new(match zone {
        Some(tz) => array.with_timezone(tz),
        None => array,
    })
}

/// The stored array as one column, one entry per timestep.
///
/// A scalar element gives a primitive array; a multidimensional one gives nested
/// `FixedSizeList`s, one level per element dimension, innermost first — which is
/// the order the flat row-major buffer is already in, so the leaves need no
/// rearranging.
///
/// Composite element types (`piecewise_linear` and friends) keep their **stored
/// packing**: they are a `FixedSizeList<f64>[w]` like any dense row, and
/// `element_type` in the footer is what names them. That packing is the
/// documented cross-language wire form, every binding has a decoder for it, and
/// `to_arrow()` already pins it with a test — decoding to `Struct`/`List` here
/// would break the one-schema rule for a benefit that reaches only foreign
/// readers of function-data series.
fn value_array(array: &TypedArray) -> Result<ArrayRef> {
    let mut column = leaf_array(array)?;
    for dim in array.element_shape().iter().rev() {
        let field = Arc::new(Field::new("item", column.data_type().clone(), false));
        column = Arc::new(
            FixedSizeListArray::try_new(field, *dim as i32, column, None).map_err(arrow_err)?,
        );
    }
    Ok(column)
}

/// The flat values, one Arrow primitive per stored element.
fn leaf_array(array: &TypedArray) -> Result<ArrayRef> {
    /// Decode the whole buffer as `T` and wrap it in `$arrow`. A dtype mismatch
    /// cannot happen — the match below dispatches on the array's own dtype — so
    /// the error arm is a corrupt buffer length, not a wrong type.
    macro_rules! primitive {
        ($t:ty, $arrow:ty) => {{
            let values: Vec<$t> = array.to_vec::<$t>().map_err(unsupported)?;
            Arc::new(<$arrow>::from(values)) as ArrayRef
        }};
    }
    Ok(match array.dtype {
        Dtype::F64 => primitive!(f64, Float64Array),
        Dtype::F32 => primitive!(f32, Float32Array),
        Dtype::I64 => primitive!(i64, Int64Array),
        Dtype::I32 => primitive!(i32, Int32Array),
        Dtype::I16 => primitive!(i16, Int16Array),
        Dtype::I8 => primitive!(i8, Int8Array),
        Dtype::U64 => primitive!(u64, UInt64Array),
        Dtype::U32 => primitive!(u32, UInt32Array),
        Dtype::U16 => primitive!(u16, UInt16Array),
        Dtype::U8 => primitive!(u8, UInt8Array),
        Dtype::Bool => primitive!(bool, BooleanArray),
    })
}

/// The Arrow type a series' timestamp column takes, without building one.
///
/// Exposed for the import, which checks an incoming file's column against what
/// the footer's `time_reference` claims.
pub fn timestamp_data_type(reference: Option<&TimeReference>) -> DataType {
    match reference {
        None | Some(TimeReference::Utc) => {
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
        }
        Some(TimeReference::Zoneless) => DataType::Timestamp(TimeUnit::Millisecond, None),
        Some(r) => DataType::Timestamp(TimeUnit::Millisecond, Some(r.as_storage_string().into())),
    }
}
