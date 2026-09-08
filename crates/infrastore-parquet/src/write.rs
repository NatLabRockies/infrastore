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
use chrono::{DateTime, Utc};
use infrastore_core::{
    Dtype, Period, TimeSeriesData, TimeSeriesMetadata, TimeSeriesType, TypedArray,
};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::partition::{PartitionKey, ValueKind, disambiguate, series_name, values_name};
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
    let empty: Vec<String> = series
        .iter()
        .map(|(row, data)| Ok((row, row_count(row, data)?)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, n)| *n == 0)
        .map(|(row, _)| format!("'{}' (owner {})", row.name, row.owner_id))
        .collect();
    if !empty.is_empty() {
        return Err(unsupported(format!(
            "{} of the selected series hold no values and cannot be exported: {}. A values file \
             has one row per value, so an empty series would be a series row with no values \
             group -- which is also what a truncated file looks like. Narrow the selection past \
             them.",
            empty.len(),
            empty.join(", ")
        )));
    }

    let mut groups: BTreeMap<PartitionKey, Vec<usize>> = BTreeMap::new();
    for (index, (row, _)) in series.iter().enumerate() {
        groups.entry(partition_of(row)).or_default().push(index);
    }

    std::fs::create_dir_all(dir)?;
    let stems = disambiguate(&groups.keys().cloned().collect::<Vec<_>>());
    let mut report = ExportReport::default();
    for (key, members) in &groups {
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

        // Group the partition's series by the array each reads. `BTreeMap` is
        // the sort both files promise.
        let mut arrays: BTreeMap<ArrayKey, Vec<usize>> = BTreeMap::new();
        for index in members {
            let (row, data) = &series[*index];
            arrays
                .entry(array_key(row, data)?)
                .or_default()
                .push(*index);
        }
        // Within a key, by id: the series file's second sort key, so a reader
        // walking it sees the same order twice.
        for members in arrays.values_mut() {
            members.sort_by_key(|i| series[*i].0.id.map_or(0, |id| id.get()));
        }

        let stem = stems.get(key).expect("every partition was named").clone();
        let values_path = dir.join(values_name(&stem));
        let series_path = dir.join(series_name(&stem));
        let rows = write_values(&values_path, &table, series, &arrays)?;
        write_series_file(&series_path, &table, series, &arrays)?;

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

/// The array a series reads: its content hash and its time axis.
///
/// The hash is the **canonical** one, which for a composite kind is the
/// minimum-width re-encoding rather than the stored bytes — so two series whose
/// curves are the same points at different paddings share one values group, which
/// is the whole point of normalizing.
pub fn array_key(row: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<ArrayKey> {
    let array = series_rows(row, data)?.array;
    let leading = leading_shape(row, array);
    Ok(ArrayKey {
        data_hash: table::canonical_hash(array, row.element_type, &leading)?,
        time_axis: table::time_axis_of(data)?,
    })
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

/// The per-series text columns, one per name `series_text_columns` lists.
///
/// Absent free-form descriptors are the **empty string**, which is what keeps
/// every column required. A stored empty string therefore reads back as absent;
/// §2.8 of the plan documents that, and it is the price of not having five
/// nullable columns.
fn descriptor_row(
    ts_type: TimeSeriesType,
    row: &TimeSeriesMetadata,
) -> Vec<(&'static str, String)> {
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
            schema::ELEMENT_SHAPE,
            schema::encode_element_shape(&row.element_shape),
        ),
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
