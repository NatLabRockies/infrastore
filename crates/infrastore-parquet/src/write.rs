//! Writing a selection as partitioned, normalized Parquet.
//!
//! Two files per `(time_series_type, value type, time_reference)` triple: a
//! **values** file holding every distinct array once, one row per value, and a
//! **series** file holding one catalog row per series naming the array it reads.
//!
//! Normalized because the store is. A thousand components sharing one profile
//! hold one array in the store, and a table with the catalog row beside every
//! value would write that profile a thousand times — Parquet's compression does
//! not find repeats across pages, so the file really is a thousand times larger.
//!
//! Both files are sorted by the array key `(data_hash, time_axis)`, which is what
//! lets the import walk them as a merge join.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BooleanArray, FixedSizeListArray, Float32Array, Float64Array, Int8Array, Int16Array,
    Int32Array, Int64Array, RecordBatch, StringArray, TimestampMillisecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use arrow::datatypes::{Field, SchemaRef};
use infrastore_core::{Dtype, TimeSeriesData, TimeSeriesMetadata, TimeSeriesType, TypedArray};
pub use infrastore_tabular::export::{array_key, partition_of, per_step_shape};
use infrastore_tabular::export::{descriptor_row, plan, refuse_empty, row_count, series_rows};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::partition::{PartitionKey, disambiguate, series_name, values_name};
use crate::schema;
use crate::table::{self, ArrayKey, PartitionSchema, ROW_GROUP_TARGET};
use crate::{Result, arrow_err, parquet_err, unsupported};

/// One partition the export wrote: its two files and what they hold.
#[derive(Debug, Clone)]
pub struct WrittenPartition {
    pub stem: String,
    pub values_path: PathBuf,
    pub series_path: PathBuf,
    pub time_series_type: TimeSeriesType,
    pub value_slug: String,
    pub reference: String,
    /// Distinct arrays, which is how many groups the values file holds.
    pub arrays: usize,
    /// Series, which is how many rows the series file holds.
    pub series: usize,
    /// Value rows, which is how many the values file holds.
    pub rows: usize,
}

/// What an export did.
#[derive(Debug, Clone, Default)]
pub struct ExportReport {
    pub partitions: Vec<WrittenPartition>,
}

impl ExportReport {
    pub fn rows(&self) -> usize {
        self.partitions.iter().map(|p| p.rows).sum()
    }

    pub fn series(&self) -> usize {
        self.partitions.iter().map(|p| p.series).sum()
    }

    pub fn arrays(&self) -> usize {
        self.partitions.iter().map(|p| p.arrays).sum()
    }

    /// Every file written, values and series alike.
    pub fn files(&self) -> Vec<&Path> {
        self.partitions
            .iter()
            .flat_map(|p| [p.values_path.as_path(), p.series_path.as_path()])
            .collect()
    }
}

/// Refuse a destination that already holds `.parquet` files.
///
/// `add --parquet <dir>` imports every partition it finds, so a narrower export
/// written over an earlier one would leave the earlier partitions in place and a
/// later import would file them too, silently. The export neither merges nor
/// sweeps: the caller empties the directory, or names a fresh one.
///
/// Public so a caller can check *before* it knows whether anything will be
/// written: a selection that matches nothing writes nothing, and must still not
/// leave a stale export standing behind a report that says "exported 0". A
/// directory that does not exist yet passes.
pub fn check_destination(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let stale: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("parquet"))
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    if stale.is_empty() {
        return Ok(());
    }
    Err(unsupported(format!(
        "{} already holds {} .parquet file(s) ({}); export into an empty directory, since \
         `add --parquet <dir>` imports every partition it finds",
        dir.display(),
        stale.len(),
        stale.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
    )))
}

/// Write `series` into `dir` as one file pair per partition.
///
/// The pairs are `(catalog row, values)`, as `Store::list_metadata` and
/// `read_by_ids` hand them back.
///
/// **Fails if any selected series is empty**, naming every one of them, and
/// writes nothing. A series with no values has a catalog row and a zero-length
/// array; the format could represent it as a series row whose values group has
/// no rows, but that would make "a series row with no values group" legal on
/// import too, and that is the shape a truncated or half-written file takes.
/// Refusing here keeps the import's rule simple and its diagnosis honest. The
/// remedy is to narrow the selection past the empty series.
pub fn write_partitions(
    dir: &Path,
    series: &[(TimeSeriesMetadata, TimeSeriesData)],
) -> Result<ExportReport> {
    check_destination(dir)?;

    // Before anything is written, so a refusal leaves the destination exactly as
    // it was found -- which the check above has just established is empty.
    refuse_empty(series)?;

    let groups = plan(series)?;
    std::fs::create_dir_all(dir)?;
    let stems = disambiguate(&groups.keys().cloned().collect::<Vec<_>>());
    let mut report = ExportReport::default();
    for (key, arrays) in &groups {
        let members: Vec<usize> = arrays.values().flatten().copied().collect();
        // The partition's composite width comes from the catalog rows alone -- a
        // composite `element_shape` is `[w]` -- so no array is read to decide it.
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
        let table = PartitionSchema::new(key.clone(), composite_width)?;

        let stem = stems.get(key).expect("every partition was named").clone();
        let values_path = dir.join(values_name(&stem));
        let series_path = dir.join(series_name(&stem));
        let rows = write_values(&values_path, &table, series, arrays)?;
        write_series_file(&series_path, &table, series, arrays)?;

        report.partitions.push(WrittenPartition {
            stem,
            values_path,
            series_path,
            time_series_type: key.time_series_type,
            value_slug: key.value_kind.slug(),
            reference: table::reference_literal(key.time_reference.as_ref()),
            arrays: arrays.len(),
            series: members.len(),
            rows,
        });
    }
    Ok(report)
}

fn writer_properties() -> WriterProperties {
    WriterProperties::builder()
        // Archival files: zstd pays for itself several times over against the
        // HDF5 read that produced the values.
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        // The array key repeats for every row of a group, and every series
        // column is constant per series; this is the whole reason the column
        // sets are affordable.
        .set_dictionary_enabled(true)
        .build()
}

/// Write the values file: each distinct array once, in key order.
///
/// Returns the row count. Only the **first** series of each key is read: the key
/// is a content hash plus a time axis, so every series sharing it has the same
/// values at the same instants by construction, and a composite's re-padding to
/// the partition width makes even its bytes identical.
fn write_values(
    path: &Path,
    table: &PartitionSchema,
    series: &[(TimeSeriesMetadata, TimeSeriesData)],
    arrays: &BTreeMap<ArrayKey, Vec<usize>>,
) -> Result<usize> {
    let file = File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, table.values.clone(), Some(writer_properties()))
        .map_err(parquet_err)?;

    let mut buffer = ValuesBuffer::new(table);
    let mut rows = 0usize;
    for (key, members) in arrays {
        let (row, data) = &series[members[0]];
        // A group ends on an array boundary whenever it can: if the coming array
        // would carry the buffer past the target, the buffer is cut first, so
        // row-group statistics on `data_hash` mean something. Only an array
        // larger than the target on its own is split, below.
        let coming = row_count(row, data)?;
        if !buffer.is_empty() && buffer.len() + coming > ROW_GROUP_TARGET {
            let held = buffer.len();
            let batch = buffer.take(held)?;
            writer.write(&batch).map_err(parquet_err)?;
            writer.flush().map_err(parquet_err)?;
        }
        rows += buffer.push_array(key, row, data)?;
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
    Ok(rows)
}

/// Write the series file: one row per series, in `(key, id)` order.
///
/// One batch, with no row-group policy: this file has one row per series rather
/// than one per value, so even a store with a million series produces a file a
/// reader loads whole.
fn write_series_file(
    path: &Path,
    table: &PartitionSchema,
    series: &[(TimeSeriesMetadata, TimeSeriesData)],
    arrays: &BTreeMap<ArrayKey, Vec<usize>>,
) -> Result<()> {
    let file = File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, table.series.clone(), Some(writer_properties()))
        .map_err(parquet_err)?;

    let mut builder = SeriesFileBuilder::new(table);
    for (key, members) in arrays {
        for index in members {
            let (row, data) = &series[*index];
            builder.push(key, row, data)?;
        }
    }
    if !builder.is_empty() {
        let batch = builder.finish()?;
        writer.write(&batch).map_err(parquet_err)?;
    }
    writer.close().map_err(parquet_err)?;
    Ok(())
}

/// The series file's columns, accumulated.
struct SeriesFileBuilder<'a> {
    table: &'a PartitionSchema,
    data_hash: Vec<String>,
    time_axis: Vec<String>,
    id: Vec<i64>,
    owner_id: Vec<i64>,
    initial_timestamp: Vec<i64>,
    length: Vec<i64>,
    count: Vec<i64>,
    text: BTreeMap<&'static str, Vec<String>>,
}

/// The series file's text columns, in schema order.
fn series_text_columns(ts_type: TimeSeriesType) -> Vec<&'static str> {
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
        schema::ELEMENT_SHAPE,
        schema::TIME_REFERENCE,
        schema::UNITS,
        schema::QUANTITY_KIND,
        schema::UNIT_SYSTEM,
        schema::COMPONENT_FIELD,
        schema::APPLICATION_DATA,
    ]);
    names
}

impl<'a> SeriesFileBuilder<'a> {
    fn new(table: &'a PartitionSchema) -> Self {
        Self {
            table,
            data_hash: Vec::new(),
            time_axis: Vec::new(),
            id: Vec::new(),
            owner_id: Vec::new(),
            initial_timestamp: Vec::new(),
            length: Vec::new(),
            count: Vec::new(),
            text: series_text_columns(table.key.time_series_type)
                .into_iter()
                .map(|n| (n, Vec::new()))
                .collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.data_hash.is_empty()
    }

    /// The grid columns come from `data` rather than `row` for the reason
    /// [`table::time_axis_of`] gives: `export --time-range` writes a slice, and
    /// the catalog's anchor and length are the unsliced series'.
    fn push(
        &mut self,
        key: &ArrayKey,
        row: &TimeSeriesMetadata,
        data: &TimeSeriesData,
    ) -> Result<()> {
        let ts_type = self.table.key.time_series_type;
        let grid = table::grid_of(data);
        self.data_hash.push(key.data_hash.clone());
        self.time_axis.push(key.time_axis.clone());
        self.id.push(row.id.map_or(0, |i| i.get()));
        self.owner_id.push(row.owner_id);
        if ts_type == TimeSeriesType::SingleTimeSeries || ts_type.is_forecast() {
            self.initial_timestamp.push(
                grid.initial_timestamp
                    .ok_or_else(|| {
                        unsupported(format!(
                            "series '{}' is a {} but carries no initial_timestamp",
                            row.name,
                            ts_type.as_str()
                        ))
                    })?
                    .timestamp_millis(),
            );
        }
        if ts_type == TimeSeriesType::SingleTimeSeries {
            self.length.push(grid.count.unwrap_or(0) as i64);
        }
        if ts_type.is_forecast() {
            self.count.push(grid.count.unwrap_or(0) as i64);
        }
        for (name, value) in descriptor_row(ts_type, row) {
            self.text
                .get_mut(name)
                .expect("every descriptor names a column of this file")
                .push(value);
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<RecordBatch> {
        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(std::mem::take(&mut self.data_hash))),
            Arc::new(StringArray::from(std::mem::take(&mut self.time_axis))),
            Arc::new(Int64Array::from(std::mem::take(&mut self.id))),
            Arc::new(Int64Array::from(std::mem::take(&mut self.owner_id))),
        ];
        let ts_type = self.table.key.time_series_type;
        // In schema order: the four text columns that always come next, then the
        // temporal ones interleaved exactly as `PartitionSchema` lays them out.
        let mut text = series_text_columns(ts_type).into_iter();
        for _ in 0..4 {
            let name = text.next().expect("four leading text columns");
            columns.push(self.take_text(name));
        }
        if ts_type == TimeSeriesType::SingleTimeSeries || ts_type.is_forecast() {
            columns.push(zoned(
                std::mem::take(&mut self.initial_timestamp),
                &self.table.key,
            ));
            let name = text.next().expect("resolution");
            columns.push(self.take_text(name));
        }
        if ts_type == TimeSeriesType::SingleTimeSeries {
            columns.push(Arc::new(Int64Array::from(std::mem::take(&mut self.length))));
        }
        if ts_type.is_forecast() {
            for _ in 0..2 {
                let name = text.next().expect("interval and horizon");
                columns.push(self.take_text(name));
            }
            columns.push(Arc::new(Int64Array::from(std::mem::take(&mut self.count))));
        }
        for name in text {
            columns.push(self.take_text(name));
        }
        RecordBatch::try_new(self.table.series.clone(), columns).map_err(arrow_err)
    }

    fn take_text(&mut self, name: &'static str) -> ArrayRef {
        let column = self
            .text
            .get_mut(name)
            .expect("every text column was created with the builder");
        Arc::new(StringArray::from(std::mem::take(column)))
    }
}

/// A millisecond column in the partition's own spelling.
fn zoned(millis: Vec<i64>, key: &PartitionKey) -> ArrayRef {
    let array = TimestampMillisecondArray::from(millis);
    Arc::new(match key.time_reference.as_ref() {
        Some(infrastore_core::TimeReference::Zoneless) => array,
        None | Some(infrastore_core::TimeReference::Utc) => array.with_timezone("UTC"),
        Some(other) => array.with_timezone(other.as_storage_string()),
    })
}

/// Value rows accumulated for the current row group.
///
/// Columns are plain Rust vectors until a flush, which keeps appending cheap and
/// leaves the Arrow arrays to be built once per group. The value column is raw
/// little-endian bytes so it stays dtype-agnostic: one code path serves every
/// dtype, and a value never passes through a wider type on its way out.
struct ValuesBuffer<'a> {
    table: &'a PartitionSchema,
    width: usize,
    element_bytes: usize,
    data_hash: Vec<String>,
    time_axis: Vec<String>,
    timestamp: Vec<i64>,
    /// The forecast key columns, left empty for a static partition.
    issue_time: Vec<i64>,
    percentile: Vec<f64>,
    scenario: Vec<i64>,
    value: Vec<u8>,
}

impl<'a> ValuesBuffer<'a> {
    fn new(table: &'a PartitionSchema) -> Self {
        Self {
            table,
            width: table.element_width(),
            element_bytes: 0,
            data_hash: Vec::new(),
            time_axis: Vec::new(),
            timestamp: Vec::new(),
            issue_time: Vec::new(),
            percentile: Vec::new(),
            scenario: Vec::new(),
            value: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.timestamp.len()
    }

    fn is_empty(&self) -> bool {
        self.timestamp.is_empty()
    }

    /// Append every value row of one array, returning how many there were.
    fn push_array(
        &mut self,
        key: &ArrayKey,
        row: &TimeSeriesMetadata,
        data: &TimeSeriesData,
    ) -> Result<usize> {
        let rows = series_rows(row, data)?;
        let array = rows.array;
        let stride = array.dtype.size();
        if self.element_bytes == 0 {
            self.element_bytes = stride;
        } else if self.element_bytes != stride {
            return Err(unsupported(
                "two arrays in one partition disagree about their dtype",
            ));
        }

        // What one stored row occupies, which for a composite kind is the
        // series' own width rather than the partition's.
        let stored_width = rows.per_step;
        if !self.table.key.value_kind.is_composite() && stored_width != self.width {
            return Err(unsupported(format!(
                "series '{}' has element width {stored_width}, but its partition is {}",
                row.name, self.width
            )));
        }
        if stored_width > self.width {
            return Err(unsupported(format!(
                "series '{}' is wider ({stored_width}) than the partition it was placed in ({})",
                row.name, self.width
            )));
        }

        for (k, offset) in rows.offsets.iter().enumerate() {
            self.data_hash.push(key.data_hash.clone());
            self.time_axis.push(key.time_axis.clone());
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
            // Re-pad a composite row to the partition's width. Zero is what the
            // layout pads with, and the leading count `n` keeps the row
            // self-describing whatever follows it.
            self.value.extend(std::iter::repeat_n(
                0u8,
                (self.width - stored_width) * stride,
            ));
        }
        Ok(rows.offsets.len())
    }

    /// Take the first `n` rows as a `RecordBatch`, leaving the rest buffered.
    fn take(&mut self, n: usize) -> Result<RecordBatch> {
        let n = n.min(self.len());
        let stride = self.element_bytes.max(1);
        let value_bytes: Vec<u8> = self.value.drain(..n * self.width * stride).collect();

        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(
                self.data_hash.drain(..n).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                self.time_axis.drain(..n).collect::<Vec<_>>(),
            )),
            zoned(self.timestamp.drain(..n).collect(), &self.table.key),
        ];
        if !self.issue_time.is_empty() {
            columns.push(zoned(self.issue_time.drain(..n).collect(), &self.table.key));
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

        let dtype = self.table.key.value_kind.leaf_dtype();
        let mut shape = vec![n];
        shape.extend(self.table.element_shape());
        let values = TypedArray::new(dtype, shape, value_bytes).map_err(unsupported)?;
        columns.push(value_array(&values)?);

        batch_of(self.table.values.clone(), columns)
    }
}

/// Assemble a batch, mapping Arrow's own error.
fn batch_of(schema: SchemaRef, columns: Vec<ArrayRef>) -> Result<RecordBatch> {
    RecordBatch::try_new(schema, columns).map_err(arrow_err)
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
