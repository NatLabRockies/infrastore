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
use infrastore_core::{
    Dtype, Period, TimeReference, TimeSeriesData, TimeSeriesMetadata, TypedArray,
};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::schema;
use crate::{Result, arrow_err, parquet_err, unsupported};

/// The value column's name. Fixed, so two exports concatenate.
pub const VALUE_COLUMN: &str = "value";
/// A dense forecast's issue-time column: which window a row belongs to.
pub const ISSUE_TIME_COLUMN: &str = "issue_time";
/// A dense forecast's target-time column: the instant the value is for.
pub const TARGET_TIME_COLUMN: &str = "target_time";
/// A `Probabilistic` forecast's percentile column.
pub const PERCENTILE_COLUMN: &str = "percentile";
/// A `Scenarios` forecast's trajectory column, `0..scenario_count`.
pub const SCENARIO_COLUMN: &str = "scenario";
/// The timestamp column's name. For a `PersistentTimeSeries` these are
/// breakpoints rather than the instants the series has values at — the
/// `time_series_type` in the footer is what says so.
pub const TIMESTAMP_COLUMN: &str = "timestamp";

/// Build the one-series Arrow table and the row's footer.
///
/// Two shapes, chosen by the type. A static series is `timestamp`, `value`. A
/// dense forecast is a **long table**: `issue_time`, `target_time`, `value`, plus
/// `percentile` or `scenario` where the type has a third axis. Long rather than
/// one table per window because a Parquet file is one table, and a column of
/// issue times is what makes `GROUP BY issue_time` the natural query.
///
/// A `DeterministicSingleTimeSeries` never reaches here as itself: the store
/// reads one back as the `Deterministic` it is a view of, so it exports as one.
pub fn record_batch(row: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<RecordBatch> {
    // The row's spelling, not the value object's: the two agree for a series
    // read out of a store, and the catalog is the authority when they do not.
    let reference = row.time_reference.as_ref();
    let metadata = schema::metadata_for_row(row);
    match data {
        TimeSeriesData::SingleTimeSeries(_)
        | TimeSeriesData::NonSequentialTimeSeries(_)
        | TimeSeriesData::PersistentTimeSeries(_) => {
            let (instants, array) = static_parts(data)?;
            build_batch(&instants, reference, array, metadata)
        }
        _ => forecast_batch(data, reference, metadata),
    }
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
        // Unreachable through `record_batch`, which dispatches a forecast to
        // its own long-table builder. Kept as a real arm rather than an
        // `unreachable!` so a seventh type added later fails loudly here instead
        // of being silently mis-shaped.
        other => Err(unsupported(format!(
            "{} is not a static series",
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

// ---- Dense forecasts --------------------------------------------------------
//
// A forecast is stored as a cube -- `[H, count, *E]`, or `[P, H, count, *E]`
// with a third axis -- and a Parquet file is one flat table. The long form is
// the standard way to flatten one: every row carries the coordinates that place
// it, so nothing depends on row order and the file reads correctly however a
// query engine chooses to scan it.

/// The axes a forecast's stored cube has, resolved once.
struct ForecastShape<'a> {
    /// Windows.
    count: usize,
    /// Steps per window.
    horizon_steps: usize,
    /// The leading axis: percentiles or scenarios. `None` for `Deterministic`.
    lanes: Option<usize>,
    /// Trailing per-step dims, after the axes above.
    element_shape: Vec<usize>,
    array: &'a TypedArray,
}

impl ForecastShape<'_> {
    fn rows(&self) -> usize {
        self.count * self.horizon_steps * self.lanes.unwrap_or(1)
    }

    /// The flat element index of `(lane, step, window)`, in the stored cube's
    /// own row-major order.
    fn offset(&self, lane: usize, step: usize, window: usize) -> usize {
        (lane * self.horizon_steps + step) * self.count + window
    }
}

fn forecast_shape(data: &TimeSeriesData) -> Result<(ForecastShape<'_>, ForecastGrid)> {
    let (array, lanes, grid) = match data {
        TimeSeriesData::Deterministic(f) => (
            &f.data,
            None,
            ForecastGrid {
                initial_timestamp: f.initial_timestamp,
                resolution: f.resolution,
                interval: f.interval,
                count: f.count,
            },
        ),
        TimeSeriesData::Probabilistic(f) => (
            &f.data,
            Some(f.percentiles.len()),
            ForecastGrid {
                initial_timestamp: f.initial_timestamp,
                resolution: f.resolution,
                interval: f.interval,
                count: f.count,
            },
        ),
        TimeSeriesData::Scenarios(f) => (
            &f.data,
            Some(f.scenario_count),
            ForecastGrid {
                initial_timestamp: f.initial_timestamp,
                resolution: f.resolution,
                interval: f.interval,
                count: f.count,
            },
        ),
        other => {
            return Err(unsupported(format!(
                "{} is not a dense forecast",
                other.time_series_type().as_str()
            )));
        }
    };
    // The leading axes are fixed by the type; everything after them is the
    // per-step element shape, which is one axis further in than the static
    // rule and is why `TypedArray::element_shape` is wrong here.
    let leading = if lanes.is_some() { 3 } else { 2 };
    if array.shape.len() < leading {
        return Err(unsupported(format!(
            "a forecast array has at least {leading} dimensions, got {:?}",
            array.shape
        )));
    }
    let horizon_steps = array.shape[leading - 2];
    Ok((
        ForecastShape {
            count: grid.count,
            horizon_steps,
            lanes,
            element_shape: array.shape[leading..].to_vec(),
            array,
        },
        grid,
    ))
}

/// The two grids a forecast value sits on: windows step by `interval`, and the
/// steps inside one window step by `resolution`.
struct ForecastGrid {
    initial_timestamp: DateTime<Utc>,
    resolution: Period,
    interval: Period,
    count: usize,
}

impl ForecastGrid {
    fn issue_time(&self, window: usize) -> Result<DateTime<Utc>> {
        self.interval
            .add_to(self.initial_timestamp, window as i64)
            .ok_or_else(|| unsupported(format!("forecast window {window} overflows the calendar")))
    }

    fn target_time(&self, issue: DateTime<Utc>, step: usize) -> Result<DateTime<Utc>> {
        self.resolution
            .add_to(issue, step as i64)
            .ok_or_else(|| unsupported(format!("forecast step {step} overflows the calendar")))
    }
}

/// The long table for a dense forecast.
fn forecast_batch(
    data: &TimeSeriesData,
    reference: Option<&TimeReference>,
    mut metadata: std::collections::BTreeMap<String, String>,
) -> Result<RecordBatch> {
    let (shape, grid) = forecast_shape(data)?;
    // The catalog's `element_shape` counts from the wrong axis for a forecast,
    // and `scenario_count` is not a catalog column at all -- both are read off
    // the stored cube here.
    metadata.insert(
        schema::ELEMENT_SHAPE.to_string(),
        schema::encode_element_shape(&shape.element_shape),
    );
    if matches!(data, TimeSeriesData::Scenarios(_)) {
        metadata.insert(
            schema::SCENARIO_COUNT.to_string(),
            shape.lanes.unwrap_or(0).to_string(),
        );
    }

    let rows = shape.rows();
    let mut issue = Vec::with_capacity(rows);
    let mut target = Vec::with_capacity(rows);
    let mut lane_index = Vec::with_capacity(rows);
    // Element offsets in the stored cube, in the order the rows are emitted.
    let mut order = Vec::with_capacity(rows);

    // Window-major, then step, then lane: a `GROUP BY issue_time` scans
    // contiguously, and one instant's percentiles sit together, which is how a
    // fan chart reads a row.
    for window in 0..shape.count {
        let issued = grid.issue_time(window)?;
        for step in 0..shape.horizon_steps {
            let at = grid.target_time(issued, step)?;
            for lane in 0..shape.lanes.unwrap_or(1) {
                issue.push(issued);
                target.push(at);
                lane_index.push(lane);
                order.push(shape.offset(lane, step, window));
            }
        }
    }

    let values = gathered_value_array(&shape, &order)?;
    let mut fields: Vec<Field> = vec![
        Field::new(ISSUE_TIME_COLUMN, timestamp_data_type(reference), false),
        Field::new(TARGET_TIME_COLUMN, timestamp_data_type(reference), false),
    ];
    let mut columns: Vec<ArrayRef> = vec![
        timestamp_array(&issue, reference),
        timestamp_array(&target, reference),
    ];
    match data {
        TimeSeriesData::Probabilistic(f) => {
            let column: Vec<f64> = lane_index.iter().map(|&i| f.percentiles[i]).collect();
            fields.push(Field::new(PERCENTILE_COLUMN, DataType::Float64, false));
            columns.push(Arc::new(Float64Array::from(column)));
        }
        TimeSeriesData::Scenarios(_) => {
            let column: Vec<i32> = lane_index.iter().map(|&i| i as i32).collect();
            fields.push(Field::new(SCENARIO_COLUMN, DataType::Int32, false));
            columns.push(Arc::new(Int32Array::from(column)));
        }
        _ => {}
    }
    fields.push(Field::new(VALUE_COLUMN, values.data_type().clone(), false));
    columns.push(values);

    let schema = Schema::new_with_metadata(fields, metadata.into_iter().collect());
    RecordBatch::try_new(Arc::new(schema), columns).map_err(arrow_err)
}

/// The value column for a long table: the stored elements, permuted into the
/// row order the table emits.
///
/// Built by copying the element bytes rather than by decoding to a typed vector
/// and back, so it is dtype-agnostic and exact — a value never passes through a
/// wider type on its way out.
fn gathered_value_array(shape: &ForecastShape<'_>, order: &[usize]) -> Result<ArrayRef> {
    let per_element: usize = shape.element_shape.iter().product::<usize>().max(1);
    let width = shape.array.dtype.size() * per_element;
    let mut bytes = Vec::with_capacity(order.len() * width);
    for &offset in order {
        let start = offset * width;
        let end = start + width;
        let slice = shape.array.bytes.get(start..end).ok_or_else(|| {
            unsupported(format!(
                "forecast array is {} bytes, too short for element {offset}",
                shape.array.bytes.len()
            ))
        })?;
        bytes.extend_from_slice(slice);
    }
    let mut gathered_shape = vec![order.len()];
    gathered_shape.extend_from_slice(&shape.element_shape);
    let gathered =
        TypedArray::new(shape.array.dtype, gathered_shape, bytes).map_err(unsupported)?;
    value_array(&gathered)
}
