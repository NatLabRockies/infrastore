//! Writing a selection as partitioned long tables.
//!
//! One file per `(time_series_type, value type, time_reference)` triple, many
//! series per file, one row per value. Within a file each series' rows are
//! **contiguous** and sorted by the key columns, which is what lets the import
//! stream row groups and what makes row-group statistics on `id`, `owner_id`,
//! and `name` worth consulting.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BooleanArray, FixedSizeListArray, Float32Array, Float64Array, Int8Array, Int16Array,
    Int32Array, Int64Array, RecordBatch, StringArray, TimestampMillisecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use arrow::datatypes::Field;
use chrono::{DateTime, Utc};
use infrastore_core::{
    Dtype, Period, TimeSeriesData, TimeSeriesMetadata, TimeSeriesType, TypedArray,
};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::partition::{PartitionKey, ValueKind, disambiguate};
use crate::schema;
use crate::table::{self, ROW_GROUP_TARGET, TableSchema};
use crate::{Result, arrow_err, parquet_err, unsupported};

/// One file the export wrote.
#[derive(Debug, Clone)]
pub struct WrittenFile {
    pub path: PathBuf,
    pub time_series_type: TimeSeriesType,
    pub value_slug: String,
    pub reference: String,
    pub series: usize,
    pub rows: usize,
}

/// What an export did.
#[derive(Debug, Clone, Default)]
pub struct ExportReport {
    pub files: Vec<WrittenFile>,
    /// Series that contributed no rows, and so appear in no file.
    ///
    /// A long table has one row per value, so a series with no values has
    /// nothing to put in one. Reported rather than silently dropped: "it is not
    /// in the export" is a fact the caller has to be told, and the CLI warns.
    pub empty: Vec<String>,
}

impl ExportReport {
    pub fn rows(&self) -> usize {
        self.files.iter().map(|f| f.rows).sum()
    }
}

/// Write `series` into `dir` as one Parquet file per partition.
///
/// The pairs are `(catalog row, values)`, as `Store::list_metadata` and
/// `read_by_ids` hand them back. Order within a partition follows the input,
/// which for the CLI is catalog order.
pub fn write_partitions(
    dir: &Path,
    series: &[(TimeSeriesMetadata, TimeSeriesData)],
) -> Result<ExportReport> {
    std::fs::create_dir_all(dir)?;

    let mut report = ExportReport::default();
    let mut groups: BTreeMap<PartitionKey, Vec<usize>> = BTreeMap::new();
    for (index, (row, data)) in series.iter().enumerate() {
        if row_count(row, data)? == 0 {
            report.empty.push(row.name.clone());
            continue;
        }
        groups.entry(partition_of(row)).or_default().push(index);
    }

    let names = disambiguate(&groups.keys().cloned().collect::<Vec<_>>());
    for (key, members) in &groups {
        // The file's composite width comes from the catalog rows alone — a
        // composite `element_shape` is `[w]` — so no array is read to decide it.
        let composite_width = if key.value_kind.is_composite() {
            Some(
                members
                    .iter()
                    .map(|i| per_step_shape(&series[*i].0).first().copied().unwrap_or(0))
                    .max()
                    .unwrap_or(0)
                    .max(1),
            )
        } else {
            None
        };
        let table = TableSchema::new(key.clone(), composite_width)?;
        let path = dir.join(names.get(key).expect("every partition was named"));
        let written = write_one(&path, &table, series, members)?;
        report.files.push(written);
    }
    Ok(report)
}

/// The partition a catalog row belongs to.
pub fn partition_of(row: &TimeSeriesMetadata) -> PartitionKey {
    PartitionKey {
        time_series_type: row.time_series_type,
        value_kind: ValueKind::of(row.element_type, &per_step_shape(row)),
        time_reference: row.time_reference.clone(),
    }
}

/// The **per-step** element shape, which for a forecast is not the catalog's
/// `element_shape`.
///
/// The catalog stores `TypedArray::element_shape` — everything after the leading
/// axis — which is the per-step shape only for a static series. A forecast's
/// cube is `[H, count, *E]` or `[lanes, H, count, *E]`, so its per-step shape is
/// what follows *all* the leading axes. Getting this wrong would put a forecast
/// in a partition whose `value` column claims the window count is part of one
/// timestep.
pub fn per_step_shape(row: &TimeSeriesMetadata) -> Vec<usize> {
    let skip = row.time_series_type.leading_dims().saturating_sub(1);
    row.element_shape.get(skip..).unwrap_or_default().to_vec()
}

fn write_one(
    path: &Path,
    table: &TableSchema,
    series: &[(TimeSeriesMetadata, TimeSeriesData)],
    members: &[usize],
) -> Result<WrittenFile> {
    let file = File::create(path)?;
    let props = WriterProperties::builder()
        // Archival files: zstd pays for itself several times over against the
        // HDF5 read that produced the values.
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        // Every catalog column is constant per series and most are constant per
        // file, so this is the whole reason the column set is affordable.
        .set_dictionary_enabled(true)
        .build();
    let mut writer =
        ArrowWriter::try_new(file, table.schema.clone(), Some(props)).map_err(parquet_err)?;

    let mut buffer = RowBuffer::new(table);
    let mut rows = 0usize;
    for index in members {
        let (row, data) = &series[*index];
        let appended = buffer.push_series(row, data)?;
        rows += appended;
        // Cut whole target-sized groups only when one series has filled the
        // buffer on its own. Otherwise the buffer waits for the next series, so
        // the flush below lands on a boundary.
        while buffer.len() >= ROW_GROUP_TARGET {
            let batch = buffer.take(ROW_GROUP_TARGET)?;
            writer.write(&batch).map_err(parquet_err)?;
            writer.flush().map_err(parquet_err)?;
        }
    }
    if !buffer.is_empty() {
        let remaining = buffer.len();
        let batch = buffer.take(remaining)?;
        writer.write(&batch).map_err(parquet_err)?;
    }
    // `close` writes the footer; without it the file is a headerless blob.
    writer.close().map_err(parquet_err)?;

    Ok(WrittenFile {
        path: path.to_path_buf(),
        time_series_type: table.key.time_series_type,
        value_slug: table.key.value_kind.slug(),
        reference: table::reference_literal(table.key.time_reference.as_ref()),
        series: members.len(),
        rows,
    })
}

/// How many rows a series contributes.
fn row_count(row: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<usize> {
    Ok(series_rows(row, data)?.target.len())
}

/// One row of a long table, before it is a column: where the value sits on the
/// grids, and where it sits in the stored array.
struct SeriesRows<'a> {
    /// Empty for a static series, which has no windows.
    issue: Vec<i64>,
    target: Vec<i64>,
    /// The lane column, whichever the type has. Empty for `Deterministic`.
    percentile: Vec<f64>,
    scenario: Vec<i64>,
    /// The element index in `array` each row draws from, in emission order.
    ///
    /// An index rather than a slice because a forecast's rows are a *permutation*
    /// of the stored cube: the file is window-major and the cube is step-major,
    /// so nothing can be copied contiguously.
    offsets: Vec<usize>,
    array: &'a TypedArray,
    /// Elements per row.
    per_step: usize,
}

/// The rows one series contributes, in the order the file emits them.
///
/// Static rows are the array in order. A forecast's are window-major, then step,
/// then lane — so `GROUP BY issue_time` scans contiguously and one instant's
/// percentiles sit together, which is how a fan chart reads a row.
fn series_rows<'a>(row: &TimeSeriesMetadata, data: &'a TimeSeriesData) -> Result<SeriesRows<'a>> {
    let per_step = per_step_shape(row).iter().product::<usize>().max(1);
    if !data.time_series_type().is_forecast() {
        let (timestamps, array) = static_parts(data)?;
        let n = timestamps.len();
        return Ok(SeriesRows {
            issue: Vec::new(),
            target: timestamps.iter().map(|t| t.timestamp_millis()).collect(),
            percentile: Vec::new(),
            scenario: Vec::new(),
            offsets: (0..n).collect(),
            array,
            per_step,
        });
    }

    let ForecastParts {
        array,
        lanes,
        percentiles,
        grid,
    } = forecast_parts(data)?;
    let leading = data.time_series_type().leading_dims();
    let horizon_steps = array.shape[leading - 2];
    let count = grid.count;
    let lane_count = lanes.unwrap_or(1);

    let rows = count * horizon_steps * lane_count;
    let mut out = SeriesRows {
        issue: Vec::with_capacity(rows),
        target: Vec::with_capacity(rows),
        percentile: Vec::new(),
        scenario: Vec::new(),
        offsets: Vec::with_capacity(rows),
        array,
        per_step,
    };
    for window in 0..count {
        let issued = grid.issue_time(window)?;
        for step in 0..horizon_steps {
            let at = grid.target_time(issued, step)?;
            for lane in 0..lane_count {
                out.issue.push(issued.timestamp_millis());
                out.target.push(at.timestamp_millis());
                if let Some(percentiles) = &percentiles {
                    out.percentile.push(percentiles[lane]);
                } else if lanes.is_some() {
                    out.scenario.push(lane as i64);
                }
                out.offsets
                    .push((lane * horizon_steps + step) * count + window);
            }
        }
    }
    Ok(out)
}

/// A forecast's cube, its lane axis, its percentile labels, and its two grids.
struct ForecastParts<'a> {
    array: &'a TypedArray,
    /// How wide the lane axis is; `None` for `Deterministic`, which has none.
    lanes: Option<usize>,
    /// The percentile labels, for the one type that has them.
    percentiles: Option<Vec<f64>>,
    grid: ForecastGrid,
}

fn forecast_parts(data: &TimeSeriesData) -> Result<ForecastParts<'_>> {
    let grid_of = |initial, resolution, interval, count| ForecastGrid {
        initial_timestamp: initial,
        resolution,
        interval,
        count,
    };
    Ok(match data {
        TimeSeriesData::Deterministic(f) => ForecastParts {
            array: &f.data,
            lanes: None,
            percentiles: None,
            grid: grid_of(f.initial_timestamp, f.resolution, f.interval, f.count),
        },
        TimeSeriesData::Probabilistic(f) => ForecastParts {
            array: &f.data,
            lanes: Some(f.percentiles.len()),
            percentiles: Some(f.percentiles.clone()),
            grid: grid_of(f.initial_timestamp, f.resolution, f.interval, f.count),
        },
        TimeSeriesData::Scenarios(f) => ForecastParts {
            array: &f.data,
            lanes: Some(f.scenario_count),
            percentiles: None,
            grid: grid_of(f.initial_timestamp, f.resolution, f.interval, f.count),
        },
        other => {
            return Err(unsupported(format!(
                "{} is not a dense forecast",
                other.time_series_type().as_str()
            )));
        }
    })
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

/// A static series' timestamps and values.
///
/// A `SingleTimeSeries` materializes its grid, calendar-aware, which is why this
/// cannot be `initial + k * resolution`. The two irregular types hand back the
/// vector they store; for a `PersistentTimeSeries` those are **breakpoints**, so
/// the table is the sparse step function as stored.
fn static_parts(data: &TimeSeriesData) -> Result<(Vec<DateTime<Utc>>, &TypedArray)> {
    match data {
        TimeSeriesData::SingleTimeSeries(s) => Ok((s.timestamps().collect(), &s.data)),
        TimeSeriesData::NonSequentialTimeSeries(s) => Ok((s.timestamps.clone(), &s.data)),
        TimeSeriesData::PersistentTimeSeries(s) => Ok((s.timestamps.clone(), &s.data)),
        other => Err(unsupported(format!(
            "{} is not a static series",
            other.time_series_type().as_str()
        ))),
    }
}

/// Rows accumulated for the current row group.
///
/// Columns are plain Rust vectors until a flush, which keeps appending cheap and
/// leaves the Arrow arrays to be built once per group. The value column is raw
/// little-endian bytes so it stays dtype-agnostic: one code path serves every
/// dtype, and a value never passes through a wider type on its way out.
struct RowBuffer<'a> {
    table: &'a TableSchema,
    width: usize,
    element_bytes: usize,
    timestamp: Vec<i64>,
    /// The forecast key columns, left empty for a static partition.
    issue_time: Vec<i64>,
    percentile: Vec<f64>,
    scenario: Vec<i64>,
    value: Vec<u8>,
    id: Vec<i64>,
    data_hash: Vec<String>,
    owner_id: Vec<i64>,
    text: BTreeMap<&'static str, Vec<String>>,
}

/// The text columns this file carries, in schema order after `owner_id`.
fn text_columns(ts_type: TimeSeriesType) -> Vec<&'static str> {
    let mut names = vec![
        schema::OWNER_TYPE,
        schema::OWNER_CATEGORY,
        schema::TIME_SERIES_TYPE,
        schema::NAME,
    ];
    if ts_type == TimeSeriesType::SingleTimeSeries || ts_type.is_forecast() {
        names.push(schema::RESOLUTION);
    }
    if ts_type.is_forecast() {
        names.push(schema::INTERVAL);
        names.push(schema::HORIZON);
    }
    names.extend([
        schema::FEATURES,
        schema::ELEMENT_TYPE,
        schema::TIME_REFERENCE,
        schema::UNITS,
        schema::QUANTITY_KIND,
        schema::UNIT_SYSTEM,
        schema::COMPONENT_FIELD,
        schema::APPLICATION_DATA,
    ]);
    names
}

impl<'a> RowBuffer<'a> {
    fn new(table: &'a TableSchema) -> Self {
        let width = table.element_width();
        Self {
            table,
            width,
            element_bytes: 0,
            timestamp: Vec::new(),
            issue_time: Vec::new(),
            percentile: Vec::new(),
            scenario: Vec::new(),
            value: Vec::new(),
            id: Vec::new(),
            data_hash: Vec::new(),
            owner_id: Vec::new(),
            text: text_columns(table.key.time_series_type)
                .into_iter()
                .map(|n| (n, Vec::new()))
                .collect(),
        }
    }

    fn len(&self) -> usize {
        self.timestamp.len()
    }

    fn is_empty(&self) -> bool {
        self.timestamp.is_empty()
    }

    /// Append every row of one series, returning how many there were.
    fn push_series(&mut self, row: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<usize> {
        let rows = series_rows(row, data)?;
        let array = rows.array;
        let dtype = array.dtype;
        let stride = dtype.size();
        if self.element_bytes == 0 {
            self.element_bytes = stride;
        } else if self.element_bytes != stride {
            return Err(unsupported(
                "two series in one partition disagree about their dtype",
            ));
        }

        // What one stored row occupies, which for a composite kind is the
        // series' own width rather than the file's.
        let stored_width = rows.per_step;
        if !self.table.key.value_kind.is_composite() && stored_width != self.width {
            return Err(unsupported(format!(
                "series '{}' has element width {stored_width}, but its partition is {}",
                row.name, self.width
            )));
        }
        if stored_width > self.width {
            return Err(unsupported(format!(
                "series '{}' is wider ({stored_width}) than the file it was placed in ({})",
                row.name, self.width
            )));
        }

        // The hash is over the *stored* cube, not the emitted rows: it
        // identifies the series' values, which a permutation into row order does
        // not change.
        let leading = leading_shape(row, array);
        let hash = table::canonical_hash(array, row.element_type, &leading)?;
        let descriptors = self.descriptor_row(row);

        for (k, offset) in rows.offsets.iter().enumerate() {
            self.timestamp.push(rows.target[k]);
            if !rows.issue.is_empty() {
                self.issue_time.push(rows.issue[k]);
            }
            if !rows.percentile.is_empty() {
                self.percentile.push(rows.percentile[k]);
            }
            if !rows.scenario.is_empty() {
                self.scenario.push(rows.scenario[k]);
            }
            let start = offset * stored_width * stride;
            let end = start + stored_width * stride;
            let slice = array.bytes.get(start..end).ok_or_else(|| {
                unsupported(format!("series '{}' is shorter than its shape", row.name))
            })?;
            self.value.extend_from_slice(slice);
            // Re-pad a composite row to the file's width. Zero is what the
            // layout pads with, and the leading count `n` keeps the row
            // self-describing whatever follows it.
            self.value.extend(std::iter::repeat_n(
                0u8,
                (self.width - stored_width) * stride,
            ));

            self.id.push(row.id.map_or(0, |i| i.get()));
            self.data_hash.push(hash.clone());
            self.owner_id.push(row.owner_id);
            for (name, value) in &descriptors {
                self.text
                    .get_mut(name)
                    .expect("every descriptor names a column of this file")
                    .push(value.clone());
            }
        }
        Ok(rows.offsets.len())
    }

    /// The per-series constants, one per text column.
    ///
    /// Absent free-form descriptors are the **empty string**, which is what
    /// keeps every column required. A stored empty string therefore reads back
    /// as absent; §2.7 of the plan documents that, and it is the price of not
    /// having five nullable columns.
    fn descriptor_row(&self, row: &TimeSeriesMetadata) -> Vec<(&'static str, String)> {
        let mut out: Vec<(&'static str, String)> = vec![
            (schema::OWNER_TYPE, row.owner_type.clone()),
            (
                schema::OWNER_CATEGORY,
                row.owner_category.as_str().to_string(),
            ),
            (
                schema::TIME_SERIES_TYPE,
                row.time_series_type.as_str().to_string(),
            ),
            (schema::NAME, row.name.clone()),
        ];
        let ts_type = self.table.key.time_series_type;
        if ts_type == TimeSeriesType::SingleTimeSeries || ts_type.is_forecast() {
            out.push((
                schema::RESOLUTION,
                row.resolution.map(|p| p.to_iso8601()).unwrap_or_default(),
            ));
        }
        if ts_type.is_forecast() {
            out.push((
                schema::INTERVAL,
                row.interval.map(|p| p.to_iso8601()).unwrap_or_default(),
            ));
            out.push((
                schema::HORIZON,
                row.horizon.map(|p| p.to_iso8601()).unwrap_or_default(),
            ));
        }
        out.extend([
            (schema::FEATURES, schema::encode_features(&row.features)),
            (schema::ELEMENT_TYPE, row.element_type.to_string()),
            (
                schema::TIME_REFERENCE,
                table::reference_literal(row.time_reference.as_ref()),
            ),
            (schema::UNITS, row.units.clone().unwrap_or_default()),
            (
                schema::QUANTITY_KIND,
                row.quantity_kind.clone().unwrap_or_default(),
            ),
            (
                schema::UNIT_SYSTEM,
                row.unit_system.map_or("", |u| u.as_str()).to_string(),
            ),
            (
                schema::COMPONENT_FIELD,
                row.component_field.clone().unwrap_or_default(),
            ),
            (
                schema::APPLICATION_DATA,
                row.application_data.clone().unwrap_or_default(),
            ),
        ]);
        out
    }

    /// Take the first `n` rows as a `RecordBatch`, leaving the rest buffered.
    fn take(&mut self, n: usize) -> Result<RecordBatch> {
        let n = n.min(self.len());
        let stride = self.element_bytes.max(1);
        let value_bytes: Vec<u8> = self.value.drain(..n * self.width * stride).collect();

        let timestamps: Vec<i64> = self.timestamp.drain(..n).collect();
        let stamp_array = TimestampMillisecondArray::from(timestamps);
        let stamp_array = match self.table.key.time_reference.as_ref() {
            Some(infrastore_core::TimeReference::Zoneless) => stamp_array,
            None | Some(infrastore_core::TimeReference::Utc) => stamp_array.with_timezone("UTC"),
            Some(other) => stamp_array.with_timezone(other.as_storage_string()),
        };

        let dtype = self.table.key.value_kind.leaf_dtype();
        let mut shape = vec![n];
        shape.extend(self.table.element_shape());
        let values = TypedArray::new(dtype, shape, value_bytes).map_err(unsupported)?;

        // Key columns first, in schema order: `issue_time` then the lane, both
        // present only for the partitions whose type has them.
        let mut columns: Vec<ArrayRef> = vec![Arc::new(stamp_array)];
        if !self.issue_time.is_empty() {
            let issued =
                TimestampMillisecondArray::from(self.issue_time.drain(..n).collect::<Vec<_>>());
            columns.push(Arc::new(match self.table.key.time_reference.as_ref() {
                Some(infrastore_core::TimeReference::Zoneless) => issued,
                None | Some(infrastore_core::TimeReference::Utc) => issued.with_timezone("UTC"),
                Some(other) => issued.with_timezone(other.as_storage_string()),
            }));
        }
        if !self.percentile.is_empty() {
            columns.push(Arc::new(Float64Array::from(
                self.percentile.drain(..n).collect::<Vec<_>>(),
            )));
        }
        if !self.scenario.is_empty() {
            columns.push(Arc::new(Int64Array::from(
                self.scenario.drain(..n).collect::<Vec<_>>(),
            )));
        }
        columns.push(value_array(&values)?);
        columns.push(Arc::new(Int64Array::from(
            self.id.drain(..n).collect::<Vec<_>>(),
        )));
        columns.push(Arc::new(StringArray::from(
            self.data_hash.drain(..n).collect::<Vec<_>>(),
        )));
        columns.push(Arc::new(Int64Array::from(
            self.owner_id.drain(..n).collect::<Vec<_>>(),
        )));
        for name in text_columns(self.table.key.time_series_type) {
            let column = self
                .text
                .get_mut(name)
                .expect("every text column was created with the buffer");
            columns.push(Arc::new(StringArray::from(
                column.drain(..n).collect::<Vec<_>>(),
            )));
        }
        RecordBatch::try_new(self.table.schema.clone(), columns).map_err(arrow_err)
    }
}

impl ValueKind {
    /// The dtype of the leaf the `value` column is built from.
    pub fn leaf_dtype(&self) -> Dtype {
        match self {
            ValueKind::Dense { dtype, .. } | ValueKind::Tuple { dtype, .. } => *dtype,
            ValueKind::Composite(_) => Dtype::F64,
        }
    }
}

/// A `TypedArray` as one Arrow column: a primitive leaf under one
/// `FixedSizeList` per element dimension, innermost first.
pub fn value_array(array: &TypedArray) -> Result<ArrayRef> {
    let mut column = leaf_array(array)?;
    for dim in array.element_shape().iter().rev() {
        let field = Arc::new(Field::new("item", column.data_type().clone(), false));
        let size = i32::try_from(*dim)
            .map_err(|_| unsupported(format!("element dimension {dim} does not fit an i32")))?;
        column =
            Arc::new(FixedSizeListArray::try_new(field, size, column, None).map_err(arrow_err)?);
    }
    Ok(column)
}

fn leaf_array(array: &TypedArray) -> Result<ArrayRef> {
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

/// The leading axes of a stored array — what precedes the per-step element
/// shape — as `encode` wants them.
///
/// `[length]` for a static series, `[H, count]` for a `Deterministic`,
/// `[lanes, H, count]` for the two with a third axis. Only the composite
/// canonicalization uses it, and only to re-encode at minimum width.
fn leading_shape(row: &TimeSeriesMetadata, array: &TypedArray) -> Vec<usize> {
    let leading = row.time_series_type.leading_dims().min(array.shape.len());
    array.shape[..leading].to_vec()
}
