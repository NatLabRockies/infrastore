//! Reading partitioned long tables back.
//!
//! A file or a whole directory. Rows are streamed a row group at a time and
//! grouped by the **`KeyIdentity` columns** — `owner_id`, `owner_category`,
//! `time_series_type`, `name`, `resolution`, `interval`, `features` — because
//! `add` never accepts an id, so the id column cannot be what identifies a
//! series. It is read only to be reported.
//!
//! The grouping is a streaming one: rows accumulate until the key changes, and a
//! key that reappears after another series' rows is **refused** rather than
//! stitched back together. Holding every series in memory to allow that would
//! give up the one property that makes a multi-gigabyte file importable.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use arrow::array::{
    Array, ArrayRef, BooleanArray, FixedSizeListArray, Float32Array, Float64Array, Int8Array,
    Int16Array, Int32Array, Int64Array, RecordBatch, StringArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, TimeUnit};
use chrono::{DateTime, TimeZone, Utc};
use infrastore_core::{
    Dtype, ElementType, Features, NonSequentialTimeSeries, OwnerCategory, Period,
    PersistentTimeSeries, SingleTimeSeries, TimeReference, TimeSeriesData, TimeSeriesType,
    TypedArray,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use crate::schema;
use crate::table::{self, DATA_HASH, ISSUE_TIME, PERCENTILE, SCENARIO, TIMESTAMP, VALUE};
use crate::{Result, arrow_err, parquet_err, unsupported};

/// One series read out of a long table, with the catalog fields a caller needs
/// to file it.
#[derive(Debug, Clone)]
pub struct ImportedSeries {
    /// The ids the file recorded for this series' rows, for reporting only.
    ///
    /// Never used to file the row: "never reissued" is a guarantee of the
    /// catalog's `AUTOINCREMENT`, and a caller free to name an id could re-file
    /// a retired one.
    pub recorded_id: Option<i64>,
    pub owner_id: i64,
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub features: Features,
    pub data: TimeSeriesData,
}

/// What the caller asserts or supplies on top of the file.
///
/// Every field **overrides** the corresponding column for every series in the
/// file, except `element_type`, which is an **assertion**: it states the reading
/// the bytes cannot — whether a `FixedSizeList<double>[3]` is a tuple or a dense
/// row — and a value contradicting the file is an error rather than a silent
/// replacement.
#[derive(Debug, Default, Clone)]
pub struct ImportOptions {
    pub time_series_type: Option<TimeSeriesType>,
    pub element_type: Option<ElementType>,
    pub name: Option<String>,
    pub owner_id: Option<i64>,
    pub owner_type: Option<String>,
    pub owner_category: Option<OwnerCategory>,
    pub time_reference: Option<TimeReference>,
    pub features: Option<Features>,
    /// Skip the `data_hash` comparison. Not exposed on the CLI; the way to
    /// import edited values is to drop the column, which is what a query engine
    /// does anyway when it rewrites a file.
    pub skip_checksum: bool,
}

/// Every `.parquet` file under `path`, or `path` itself when it is a file.
///
/// Sorted, so a directory import is deterministic and a failure part-way
/// through names the same file every run.
pub fn parquet_files(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        return Err(unsupported(format!(
            "{} is neither a Parquet file nor a directory of them",
            path.display()
        )));
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(path)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(unsupported(format!(
            "{} holds no .parquet files",
            path.display()
        )));
    }
    Ok(files)
}

/// A place each completed series goes as soon as its last row is read.
///
/// The whole point of a long table is that a partition can be far larger than
/// memory, so the reader never holds more than the series it is in the middle
/// of. A caller filing a file in one transaction opens the transaction, hands
/// this a closure that adds each series, and commits when the call returns.
pub type SeriesSink<'a> = &'a mut dyn FnMut(ImportedSeries) -> Result<()>;

/// Stream one file's series into `sink`, returning how many were handed over.
///
/// Row groups are read one at a time and each series is released to the sink
/// the moment its rows end, so peak memory is one row group plus one series,
/// not the file. A sink error stops the read and is returned as it is.
pub fn read_file_with(path: &Path, options: &ImportOptions, sink: SeriesSink<'_>) -> Result<usize> {
    let file = std::fs::File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(parquet_err)?;
    let schema = builder.schema().clone();
    let footer: BTreeMap<String, String> = schema
        .metadata()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    check_format(&footer, path)?;

    let mut grouper = Grouper::new(options, &footer, sink);
    let reader = builder.build().map_err(parquet_err)?;
    for batch in reader {
        let batch = batch.map_err(arrow_err)?;
        grouper.push_batch(&batch)?;
    }
    grouper.finish()
}

/// Read one file into the series it holds, all at once.
///
/// The collecting form of [`read_file_with`], for a caller that wants the
/// partition in hand — a test, a dry run over a small file. A load should
/// stream instead: this holds every series of the file until the end.
pub fn read_file(path: &Path, options: &ImportOptions) -> Result<Vec<ImportedSeries>> {
    let mut out = Vec::new();
    read_file_with(path, options, &mut |series| {
        out.push(series);
        Ok(())
    })?;
    Ok(out)
}

/// Refuse a file this build cannot read, by version rather than by whichever
/// column it turns out to be missing.
///
/// A file with no marker at all is **foreign** and allowed: inferring what a
/// stranger's Parquet means is the whole point of the fallbacks below.
fn check_format(footer: &BTreeMap<String, String>, path: &Path) -> Result<()> {
    match footer.get(table::FORMAT) {
        None => Ok(()),
        Some(v) if v == table::FORMAT_V1 => Ok(()),
        Some(other) => Err(unsupported(format!(
            "{} is {other}, which this build does not read (it reads {})",
            path.display(),
            table::FORMAT_V1
        ))),
    }
}

/// The columns that identify a series, in the order the catalog files them.
///
/// Deliberately the `KeyIdentity` tuple and nothing else. `owner_type` and the
/// descriptors are *carried* by a series rather than identifying it, so they
/// must be constant within a group but do not split one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SeriesKey {
    /// `None` when the file has no `owner_id` column at all, which a foreign
    /// file legitimately does not. Distinct from `Some(0)`, which is a real
    /// owner: without the distinction a nameless foreign file filed itself
    /// under owner 0.
    owner_id: Option<i64>,
    owner_category: String,
    time_series_type: String,
    name: String,
    resolution: String,
    interval: String,
    features: String,
}

/// The per-series constants a group carries, checked for agreement across rows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeriesFields {
    owner_type: String,
    horizon: String,
    element_type: String,
    time_reference: String,
    units: String,
    quantity_kind: String,
    unit_system: String,
    component_field: String,
    application_data: String,
    data_hash: String,
    id: i64,
}

/// Accumulates the current series and flushes when the key changes.
struct Grouper<'a> {
    options: &'a ImportOptions,
    footer: &'a BTreeMap<String, String>,
    current: Option<(SeriesKey, SeriesFields)>,
    timestamps: Vec<DateTime<Utc>>,
    /// The forecast key columns, empty for a static partition.
    issue: Vec<DateTime<Utc>>,
    lanes: Vec<LaneValue>,
    values: Vec<u8>,
    dtype: Option<Dtype>,
    element_dims: Vec<usize>,
    /// The `timestamp` column's Arrow zone, `None` for a naive column, taken
    /// from the first batch. It is the last resort for a foreign file that
    /// names no `time_reference` anywhere.
    zone: Option<Option<String>>,
    seen: HashSet<SeriesKey>,
    /// Where a finished series goes; see [`SeriesSink`].
    sink: SeriesSink<'a>,
    /// How many the sink has taken.
    filed: usize,
}

impl<'a> Grouper<'a> {
    fn new(
        options: &'a ImportOptions,
        footer: &'a BTreeMap<String, String>,
        sink: SeriesSink<'a>,
    ) -> Self {
        Self {
            options,
            footer,
            current: None,
            timestamps: Vec::new(),
            issue: Vec::new(),
            lanes: Vec::new(),
            values: Vec::new(),
            dtype: None,
            element_dims: Vec::new(),
            zone: None,
            seen: HashSet::new(),
            sink,
            filed: 0,
        }
    }

    fn push_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let stamps = read_timestamps(batch, TIMESTAMP)?;
        if self.zone.is_none() {
            self.zone = Some(arrow_zone(batch, TIMESTAMP)?);
        }
        let issued = batch
            .column_by_name(ISSUE_TIME)
            .map(|_| read_timestamps(batch, ISSUE_TIME))
            .transpose()?;
        let lanes = read_lanes(batch)?;
        let (dims, values) = batch_values(batch)?;
        match self.dtype {
            None => {
                self.dtype = Some(values.dtype);
                self.element_dims = dims.clone();
            }
            Some(existing) if existing == values.dtype && self.element_dims == dims => {}
            Some(_) => {
                return Err(unsupported(
                    "two row groups disagree about the value column's type",
                ));
            }
        }
        let width: usize = dims.iter().product::<usize>().max(1);
        let stride = values.dtype.size();

        let keys = read_keys(batch)?;
        let fields = read_fields(batch)?;
        for row in 0..batch.num_rows() {
            let key = &keys[row];
            let field = &fields[row];
            let changed = match &self.current {
                None => true,
                Some((current, _)) => current != key,
            };
            if changed {
                self.flush()?;
                if !self.seen.insert(key.clone()) {
                    return Err(unsupported(format!(
                        "series '{}' (owner {:?}) appears again after another series' rows; \
                         a long table must be sorted by the identity columns and then by time",
                        key.name, key.owner_id
                    )));
                }
                self.current = Some((key.clone(), field.clone()));
            } else if let Some((_, current)) = &self.current
                && current != field
            {
                return Err(unsupported(format!(
                    "series '{}' (owner {:?}) carries two different sets of descriptors; \
                     every column but the values must be constant within a series",
                    key.name, key.owner_id
                )));
            }
            self.timestamps.push(stamps[row]);
            if let Some(issued) = &issued {
                self.issue.push(issued[row]);
            }
            if let Some(lanes) = &lanes {
                self.lanes.push(lanes[row]);
            }
            let start = row * width * stride;
            self.values
                .extend_from_slice(&values.bytes[start..start + width * stride]);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<usize> {
        self.flush()?;
        Ok(self.filed)
    }

    fn flush(&mut self) -> Result<()> {
        let Some((key, fields)) = self.current.take() else {
            return Ok(());
        };
        let timestamps = std::mem::take(&mut self.timestamps);
        let issue = std::mem::take(&mut self.issue);
        let lanes = std::mem::take(&mut self.lanes);
        let bytes = std::mem::take(&mut self.values);
        let dtype = self.dtype.expect("a flushed group has seen a batch");

        let mut shape = vec![timestamps.len()];
        shape.extend_from_slice(&self.element_dims);
        let array = TypedArray::new(dtype, shape, bytes).map_err(unsupported)?;
        let series = self.build(
            &key,
            &fields,
            Rows {
                timestamps,
                issue,
                lanes,
            },
            array,
        )?;
        (self.sink)(series)?;
        self.filed += 1;
        Ok(())
    }

    fn build(
        &self,
        key: &SeriesKey,
        fields: &SeriesFields,
        rows: Rows,
        array: TypedArray,
    ) -> Result<ImportedSeries> {
        let ts_type = self.resolve_type(key, &rows.timestamps)?;
        let element_type = self.resolve_element_type(fields, &array)?;
        let reference = self.resolve_reference(fields)?;
        let name = self.resolve_name(key)?;
        let owner_id = self.resolve_owner_id(key, &name)?;
        let owner_type = self.resolve_owner_type(fields, &name)?;

        let mut data = self.assemble(ts_type, key, fields, rows, array, name.clone())?;
        let descriptors = infrastore_core::Descriptors {
            element_type,
            units: optional(&fields.units),
            quantity_kind: optional(&fields.quantity_kind),
            unit_system: optional(&fields.unit_system)
                .map(|text| schema::decode_unit_system(&text).map_err(unsupported))
                .transpose()?,
            time_reference: reference,
            component_field: optional(&fields.component_field),
            application_data: optional(&fields.application_data),
        };
        // Before canonicalizing: the constructor resolved the element type from
        // the array's dtype, and only the declared one says whether these rows
        // are a curve or a dense block of doubles.
        data.set_descriptors(descriptors);

        // A composite row was padded to the file's width; shrink it back to the
        // width its own points need. Unconditional, not part of the checksum:
        // the narrower array is the right one to store whether or not there is a
        // hash to compare it against.
        canonicalize(&mut data)?;
        self.verify_checksum(fields, &data, &name)?;

        Ok(ImportedSeries {
            recorded_id: (fields.id != 0).then_some(fields.id),
            owner_id,
            owner_type,
            owner_category: match &self.options.owner_category {
                Some(c) => *c,
                None if key.owner_category.is_empty() => OwnerCategory::Component,
                None => schema::decode_owner_category(&key.owner_category).map_err(unsupported)?,
            },
            features: match &self.options.features {
                Some(f) => f.clone(),
                None if key.features.is_empty() => Features::new(),
                None => schema::decode_features(&key.features).map_err(unsupported)?,
            },
            data,
        })
    }

    /// The series' name: the option, then the column.
    ///
    /// A name is part of a series' `KeyIdentity`, so there is nothing sensible
    /// to default it to -- an empty one would file every nameless series in a
    /// foreign file under the same identity.
    fn resolve_name(&self, key: &SeriesKey) -> Result<String> {
        if let Some(name) = &self.options.name {
            return Ok(name.clone());
        }
        if key.name.is_empty() {
            return Err(unsupported(
                "the file records no series name and none was given; a name is part of a \
                 series' identity, so pass --name",
            ));
        }
        Ok(key.name.clone())
    }

    /// The owning component's id: the option, then the column.
    ///
    /// Refused rather than defaulted for the same reason as the name. A foreign
    /// file has no `owner_id` column, and owner 0 is a real owner rather than a
    /// sentinel, so silently choosing it would file the series somewhere the
    /// caller never named.
    fn resolve_owner_id(&self, key: &SeriesKey, name: &str) -> Result<i64> {
        self.options
            .owner_id
            .or(key.owner_id)
            .ok_or_else(|| missing_owner(name, "owner_id", "--owner-id"))
    }

    /// The owner's type name, on the same rule: a series owned by `""` is not
    /// something any consumer means.
    fn resolve_owner_type(&self, fields: &SeriesFields, name: &str) -> Result<String> {
        if let Some(owner_type) = &self.options.owner_type {
            return Ok(owner_type.clone());
        }
        if fields.owner_type.is_empty() {
            return Err(missing_owner(name, "owner_type", "--owner-type"));
        }
        Ok(fields.owner_type.clone())
    }

    /// The type to file the group under: the column, then the footer, then the
    /// option, then inference.
    ///
    /// A grid reads as `SingleTimeSeries` and anything else as
    /// `NonSequentialTimeSeries`. `PersistentTimeSeries` is **never inferred**:
    /// it is structurally identical to the irregular type and differs only in
    /// what the values mean between rows, so guessing it would be guessing that.
    fn resolve_type(
        &self,
        key: &SeriesKey,
        timestamps: &[DateTime<Utc>],
    ) -> Result<TimeSeriesType> {
        let declared = if key.time_series_type.is_empty() {
            self.footer
                .get(schema::TIME_SERIES_TYPE)
                .filter(|t| !t.is_empty())
                .map(|t| schema::decode_time_series_type(t).map_err(unsupported))
                .transpose()?
        } else {
            Some(schema::decode_time_series_type(&key.time_series_type).map_err(unsupported)?)
        };
        if let Some(declared) = declared {
            if declared == TimeSeriesType::DeterministicSingleTimeSeries {
                return Err(unsupported(
                    "a DeterministicSingleTimeSeries is derived from a stored SingleTimeSeries \
                     rather than added; import the SingleTimeSeries and run \
                     `transform_single_time_series`",
                ));
            }
            if let Some(asserted) = self.options.time_series_type
                && asserted != declared
            {
                return Err(unsupported(format!(
                    "the file declares {}, but {} was asserted",
                    declared.as_str(),
                    asserted.as_str()
                )));
            }
            return Ok(declared);
        }
        if let Some(asserted) = self.options.time_series_type {
            return Ok(asserted);
        }
        Ok(if Period::infer(timestamps).is_ok() {
            TimeSeriesType::SingleTimeSeries
        } else {
            TimeSeriesType::NonSequentialTimeSeries
        })
    }

    /// The logical element type. The file's wins; a contradicting option is an
    /// error. Without one, the Arrow type alone gives the **weaker** reading —
    /// a `FixedSizeList<double>[3]` is dense `f64` with shape `[3]`, not
    /// `tuple(3,f64)`, because the bytes cannot say and dense assumes less.
    fn resolve_element_type(
        &self,
        fields: &SeriesFields,
        array: &TypedArray,
    ) -> Result<ElementType> {
        let declared = if fields.element_type.is_empty() {
            self.footer
                .get(schema::ELEMENT_TYPE)
                .filter(|t| !t.is_empty())
                .map(|t| schema::decode_element_type(t).map_err(unsupported))
                .transpose()?
        } else {
            Some(schema::decode_element_type(&fields.element_type).map_err(unsupported)?)
        };
        match (declared, self.options.element_type) {
            (Some(from_file), Some(asserted)) if from_file != asserted => Err(unsupported(
                format!("the file declares element_type {from_file}, but {asserted} was asserted"),
            )),
            (Some(from_file), _) => Ok(from_file),
            (None, Some(asserted)) => Ok(asserted),
            (None, None) => Ok(ElementType::Scalar(array.dtype)),
        }
    }

    /// The timestamp spelling: the option, then the column, then the footer,
    /// then the timestamp column's own Arrow zone.
    ///
    /// The literal `unspecified` decodes to *no* reference and beats the zone,
    /// which is what keeps a round trip from inventing a `utc` the series never
    /// claimed — see Finding 7.12.
    fn resolve_reference(&self, fields: &SeriesFields) -> Result<Option<TimeReference>> {
        if let Some(reference) = &self.options.time_reference {
            return Ok(Some(reference.clone()));
        }
        let text = if fields.time_reference.is_empty() {
            self.footer
                .get(schema::TIME_REFERENCE)
                .filter(|t| !t.is_empty())
                .cloned()
        } else {
            Some(fields.time_reference.clone())
        };
        match text {
            Some(text) => schema::decode_time_reference(&text).map_err(unsupported),
            // A foreign file: the column's zone is all there is, and it does
            // say something — a naive column is a wall clock, a zoned one names
            // its spelling. Only the literal `unspecified` means *none*.
            None => reference_from_arrow_zone(self.zone.clone().flatten().as_deref()).map(Some),
        }
    }

    /// Compare the file's `data_hash` against the group as re-encoded.
    ///
    /// Skipped when the column is absent, which is what a foreign file and an
    /// edited one both look like: a user who changes values in DuckDB drops the
    /// column rather than recomputing it.
    fn verify_checksum(
        &self,
        fields: &SeriesFields,
        data: &TimeSeriesData,
        name: &str,
    ) -> Result<()> {
        if self.options.skip_checksum || fields.data_hash.is_empty() {
            return Ok(());
        }
        let (array, leading) = cube_of(data);
        let actual = table::canonical_hash(array, data.element_type(), &leading)?;
        if actual != fields.data_hash {
            return Err(unsupported(format!(
                "series '{name}' does not match its recorded data_hash: the file says {} and \
                 its rows hash to {actual}. If the values were edited, drop the `{DATA_HASH}` \
                 column.",
                fields.data_hash
            )));
        }
        Ok(())
    }

    fn assemble(
        &self,
        ts_type: TimeSeriesType,
        key: &SeriesKey,
        fields: &SeriesFields,
        rows: Rows,
        array: TypedArray,
        name: String,
    ) -> Result<TimeSeriesData> {
        if ts_type.is_forecast() {
            return forecast(ts_type, key, fields, rows, array, name);
        }
        let Rows { timestamps, .. } = rows;
        match ts_type {
            TimeSeriesType::SingleTimeSeries => {
                let resolution = if key.resolution.is_empty() {
                    None
                } else {
                    Some(
                        Period::from_iso8601(&key.resolution)
                            .map_err(|e| unsupported(format!("resolution: {e}")))?,
                    )
                };
                Ok(TimeSeriesData::SingleTimeSeries(single(
                    timestamps, array, &name, resolution,
                )?))
            }
            TimeSeriesType::NonSequentialTimeSeries => Ok(TimeSeriesData::NonSequentialTimeSeries(
                NonSequentialTimeSeries::new(timestamps, array, name).map_err(unsupported)?,
            )),
            TimeSeriesType::PersistentTimeSeries => Ok(TimeSeriesData::PersistentTimeSeries(
                PersistentTimeSeries::new(timestamps, array, name).map_err(unsupported)?,
            )),
            other => Err(unsupported(format!(
                "{} is not a static series",
                other.as_str()
            ))),
        }
    }
}

/// Build a regular series, checking that its rows really sit on the grid the
/// `resolution` column claims.
///
/// Checked against the grid that resolution *generates*, not against successive
/// differences: `Period::Months` clamps to month end, so the two are not the
/// same test.
fn single(
    timestamps: Vec<DateTime<Utc>>,
    array: TypedArray,
    name: &str,
    resolution: Option<Period>,
) -> Result<SingleTimeSeries> {
    let Some(&first) = timestamps.first() else {
        return Err(unsupported(
            "a SingleTimeSeries is anchored at its first timestamp and this group has no rows",
        ));
    };
    let Some(resolution) = resolution else {
        return SingleTimeSeries::from_timestamps(&timestamps, array, name).map_err(unsupported);
    };
    let series = SingleTimeSeries::new(first, resolution, array, name);
    let grid: Vec<DateTime<Utc>> = series.timestamps().collect();
    if grid != timestamps {
        return Err(unsupported(format!(
            "series '{name}' does not sit on a {} grid anchored at {first}",
            resolution.to_iso8601()
        )));
    }
    Ok(series)
}

/// A file that says nothing about who owns a series, and no flag that does.
fn missing_owner(name: &str, column: &str, flag: &str) -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "series '{name}' has no {column}: a file written by `export -f parquet` carries one, \
         a foreign file does not, so pass {flag}"
    ))
}

/// An empty string is how an absent free-form descriptor is written, so that
/// every column can be required. The one documented consequence: a stored empty
/// string reads back as absent.
fn optional(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
}

fn read_keys(batch: &RecordBatch) -> Result<Vec<SeriesKey>> {
    let owner_id = int_column(batch, schema::OWNER_ID)?;
    let owner_category = text_column(batch, schema::OWNER_CATEGORY)?;
    let ts_type = text_column(batch, schema::TIME_SERIES_TYPE)?;
    let name = text_column(batch, schema::NAME)?;
    let resolution = text_column(batch, schema::RESOLUTION)?;
    let interval = text_column(batch, schema::INTERVAL)?;
    let features = text_column(batch, schema::FEATURES)?;
    Ok((0..batch.num_rows())
        .map(|i| SeriesKey {
            owner_id: owner_id.as_ref().map(|c| c[i]),
            owner_category: cell(&owner_category, i),
            time_series_type: cell(&ts_type, i),
            name: cell(&name, i),
            resolution: cell(&resolution, i),
            interval: cell(&interval, i),
            features: cell(&features, i),
        })
        .collect())
}

fn read_fields(batch: &RecordBatch) -> Result<Vec<SeriesFields>> {
    let id = int_column(batch, schema::ID)?;
    let owner_type = text_column(batch, schema::OWNER_TYPE)?;
    let horizon = text_column(batch, schema::HORIZON)?;
    let element_type = text_column(batch, schema::ELEMENT_TYPE)?;
    let time_reference = text_column(batch, schema::TIME_REFERENCE)?;
    let units = text_column(batch, schema::UNITS)?;
    let quantity_kind = text_column(batch, schema::QUANTITY_KIND)?;
    let unit_system = text_column(batch, schema::UNIT_SYSTEM)?;
    let component_field = text_column(batch, schema::COMPONENT_FIELD)?;
    let application_data = text_column(batch, schema::APPLICATION_DATA)?;
    let data_hash = text_column(batch, DATA_HASH)?;
    Ok((0..batch.num_rows())
        .map(|i| SeriesFields {
            owner_type: cell(&owner_type, i),
            horizon: cell(&horizon, i),
            element_type: cell(&element_type, i),
            time_reference: cell(&time_reference, i),
            units: cell(&units, i),
            quantity_kind: cell(&quantity_kind, i),
            unit_system: cell(&unit_system, i),
            component_field: cell(&component_field, i),
            application_data: cell(&application_data, i),
            data_hash: cell(&data_hash, i),
            id: id.as_ref().map_or(0, |c| c[i]),
        })
        .collect())
}

fn cell(column: &Option<Vec<String>>, row: usize) -> String {
    column.as_ref().map_or_else(String::new, |c| c[row].clone())
}

/// A required-text column as owned strings, or `None` when the file has no such
/// column — which a foreign file legitimately does not.
fn text_column(batch: &RecordBatch, name: &str) -> Result<Option<Vec<String>>> {
    let Some(column) = batch.column_by_name(name) else {
        return Ok(None);
    };
    let array = column
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| unsupported(format!("column `{name}` is not text")))?;
    // A null is not an empty string. The format writes an absent descriptor
    // *as* the empty string precisely so that no column is nullable; a null
    // that reached here would otherwise become "absent" silently, and in an
    // identity column would file the rows under a different series.
    refuse_nulls(column, name)?;
    Ok(Some(
        (0..array.len())
            .map(|i| array.value(i).to_string())
            .collect(),
    ))
}

fn refuse_nulls(column: &ArrayRef, name: &str) -> Result<()> {
    if column.null_count() > 0 {
        return Err(unsupported(format!(
            "the `{name}` column has nulls; the store holds none, and the format writes an \
             absent value as the empty string rather than a null"
        )));
    }
    Ok(())
}

fn int_column(batch: &RecordBatch, name: &str) -> Result<Option<Vec<i64>>> {
    let Some(column) = batch.column_by_name(name) else {
        return Ok(None);
    };
    refuse_nulls(column, name)?;
    let values = match column.data_type() {
        DataType::Int64 => {
            let a = downcast::<Int64Array>(column, "int64")?;
            (0..a.len()).map(|i| a.value(i)).collect()
        }
        DataType::Int32 => {
            let a = downcast::<Int32Array>(column, "int32")?;
            (0..a.len()).map(|i| a.value(i) as i64).collect()
        }
        other => {
            return Err(unsupported(format!(
                "column `{name}` is {other}, not an integer"
            )));
        }
    };
    Ok(Some(values))
}

/// The zone the `name` timestamp column carries, `None` for a naive column.
fn arrow_zone(batch: &RecordBatch, name: &str) -> Result<Option<String>> {
    let array = batch
        .column_by_name(name)
        .ok_or_else(|| unsupported(format!("the file has no `{name}` column")))?;
    match array.data_type() {
        DataType::Timestamp(_, zone) => Ok(zone.as_ref().map(|z| z.to_string())),
        other => Err(unsupported(format!(
            "column `{name}` is {other}, not a timestamp"
        ))),
    }
}

/// The spelling a timestamp column's own Arrow zone implies, for a file whose
/// footer and `time_reference` column are both silent.
pub fn reference_from_arrow_zone(zone: Option<&str>) -> Result<TimeReference> {
    match zone {
        None => Ok(TimeReference::Zoneless),
        Some(z) if z.eq_ignore_ascii_case("UTC") => Ok(TimeReference::Utc),
        Some(z) => TimeReference::parse(z)
            .map_err(|e| unsupported(format!("the timestamp column's zone {z:?}: {e}"))),
    }
}

fn downcast<'a, T: 'static>(array: &'a ArrayRef, what: &str) -> Result<&'a T> {
    array
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| unsupported(format!("expected a {what} column")))
}

/// One batch's timestamp column as instants.
///
/// Seconds and milliseconds cross as they are; microseconds and nanoseconds only
/// when every value is a whole millisecond, the same rule the store's write path
/// enforces on every instant it records.
pub fn read_timestamps(batch: &RecordBatch, name: &str) -> Result<Vec<DateTime<Utc>>> {
    let array = batch
        .column_by_name(name)
        .ok_or_else(|| unsupported(format!("the file has no `{name}` column")))?;
    if array.null_count() > 0 {
        return Err(unsupported(format!(
            "the `{name}` column has nulls; the store records an instant for every row"
        )));
    }
    let millis: Vec<i64> = match array.data_type() {
        DataType::Timestamp(TimeUnit::Second, _) => {
            let a = downcast::<TimestampSecondArray>(array, "timestamp[s]")?;
            (0..a.len())
                .map(|i| {
                    a.value(i)
                        .checked_mul(1_000)
                        .ok_or_else(|| unsupported("a timestamp in seconds overflows milliseconds"))
                })
                .collect::<Result<_>>()?
        }
        DataType::Timestamp(TimeUnit::Millisecond, _) => {
            let a = downcast::<TimestampMillisecondArray>(array, "timestamp[ms]")?;
            (0..a.len()).map(|i| a.value(i)).collect()
        }
        DataType::Timestamp(TimeUnit::Microsecond, _) => {
            let a = downcast::<TimestampMicrosecondArray>(array, "timestamp[us]")?;
            rescale((0..a.len()).map(|i| a.value(i)), 1_000, "microsecond", name)?
        }
        DataType::Timestamp(TimeUnit::Nanosecond, _) => {
            let a = downcast::<TimestampNanosecondArray>(array, "timestamp[ns]")?;
            rescale(
                (0..a.len()).map(|i| a.value(i)),
                1_000_000,
                "nanosecond",
                name,
            )?
        }
        other => {
            return Err(unsupported(format!(
                "the `{name}` column is {other}, not a timestamp"
            )));
        }
    };
    millis
        .into_iter()
        .map(|ms| {
            Utc.timestamp_millis_opt(ms)
                .single()
                .ok_or_else(|| unsupported(format!("timestamp {ms} ms is not representable")))
        })
        .collect()
}

fn rescale(
    values: impl Iterator<Item = i64>,
    per_milli: i64,
    unit: &str,
    column: &str,
) -> Result<Vec<i64>> {
    values
        .map(|v| {
            if v % per_milli == 0 {
                Ok(v / per_milli)
            } else {
                Err(unsupported(format!(
                    "the `{column}` column is in {unit}s and {v} is not a whole millisecond; \
                     the store records millisecond instants and will not round one"
                )))
            }
        })
        .collect()
}

/// The `value` column of one batch as the per-step dims plus a flat array.
pub fn batch_values(batch: &RecordBatch) -> Result<(Vec<usize>, TypedArray)> {
    let column = batch
        .column_by_name(VALUE)
        .ok_or_else(|| unsupported(format!("the file has no `{VALUE}` column")))?
        .clone();
    let (dims, leaf) = descend(column)?;
    let mut shape = vec![batch.num_rows()];
    shape.extend_from_slice(&dims);
    Ok((dims, leaf_to_typed(&leaf, shape)?))
}

/// Peel the `FixedSizeList` levels off a value column, outermost first.
///
/// `Struct` and `List` are refused: those are the *decoded* form of a composite
/// element type, which this format does not write and so does not claim to read
/// — and a `List` is ragged, which a per-step shape is not.
fn descend(mut array: ArrayRef) -> Result<(Vec<usize>, ArrayRef)> {
    let mut dims = Vec::new();
    loop {
        match array.data_type().clone() {
            DataType::FixedSizeList(_, size) => {
                if array.null_count() > 0 {
                    return Err(nulls_refused());
                }
                let list = downcast::<FixedSizeListArray>(&array, "fixed-size list")?;
                let size = usize::try_from(size)
                    .map_err(|_| unsupported("a negative fixed-size list width"))?;
                dims.push(size);
                let offset = list.offset() * size;
                array = list.values().slice(offset, list.len() * size);
            }
            DataType::List(_) | DataType::LargeList(_) | DataType::ListView(_) => {
                return Err(unsupported(format!(
                    "the `{VALUE}` column is a variable-length list; a time series' per-step \
                     shape is fixed, so this reads fixed-size lists only"
                )));
            }
            DataType::Struct(_) => {
                return Err(unsupported(format!(
                    "the `{VALUE}` column is a struct; this reads the packed form composite \
                     element types are stored in, not a decoded one"
                )));
            }
            _ => return Ok((dims, array)),
        }
    }
}

fn nulls_refused() -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "the `{VALUE}` column has nulls; the store holds no nulls, and NaN is a value rather \
         than an absence, so this is refused rather than coerced"
    ))
}

fn leaf_to_typed(leaf: &ArrayRef, shape: Vec<usize>) -> Result<TypedArray> {
    if leaf.null_count() > 0 {
        return Err(nulls_refused());
    }
    macro_rules! collect {
        ($arrow:ty) => {{
            let a = downcast::<$arrow>(leaf, stringify!($arrow))?;
            let values: Vec<_> = (0..a.len()).map(|i| a.value(i)).collect();
            TypedArray::from_slice(shape, &values).map_err(unsupported)?
        }};
    }
    Ok(match leaf.data_type() {
        DataType::Float64 => collect!(Float64Array),
        DataType::Float32 => collect!(Float32Array),
        DataType::Int64 => collect!(Int64Array),
        DataType::Int32 => collect!(Int32Array),
        DataType::Int16 => collect!(Int16Array),
        DataType::Int8 => collect!(Int8Array),
        DataType::UInt64 => collect!(UInt64Array),
        DataType::UInt32 => collect!(UInt32Array),
        DataType::UInt16 => collect!(UInt16Array),
        DataType::UInt8 => collect!(UInt8Array),
        DataType::Boolean => collect!(BooleanArray),
        other => {
            return Err(unsupported(format!(
                "the `{VALUE}` column is {other}, which is not one of the store's dtypes"
            )));
        }
    })
}

// ---- Dense forecasts --------------------------------------------------------

/// A group's rows, as the key columns describe them.
struct Rows {
    timestamps: Vec<DateTime<Utc>>,
    /// Empty for a static partition.
    issue: Vec<DateTime<Utc>>,
    /// Empty for a static partition and for `Deterministic`.
    lanes: Vec<LaneValue>,
}

/// A forecast's third axis, whichever column carries it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LaneValue {
    Percentile(f64),
    Scenario(i64),
}

/// The lane column of one batch, or `None` when the partition has none.
fn read_lanes(batch: &RecordBatch) -> Result<Option<Vec<LaneValue>>> {
    if batch.column_by_name(PERCENTILE).is_some() {
        let column = batch.column_by_name(PERCENTILE).expect("just checked");
        if column.null_count() > 0 {
            return Err(unsupported(format!("the `{PERCENTILE}` column has nulls")));
        }
        let a = downcast::<Float64Array>(column, "float64")?;
        return Ok(Some(
            (0..a.len())
                .map(|i| LaneValue::Percentile(a.value(i)))
                .collect(),
        ));
    }
    if let Some(scenario) = int_column(batch, SCENARIO)? {
        return Ok(Some(
            scenario.into_iter().map(LaneValue::Scenario).collect(),
        ));
    }
    Ok(None)
}

/// Rebuild a forecast's cube from its long table.
///
/// Rows are placed by their **coordinates**, not by their order, so a file a
/// query engine sorted or partitioned still reads correctly. Every slot must be
/// filled exactly once: a cube has no hole to leave, and two rows for one slot
/// means they disagree.
///
/// The grid comes from the columns — `resolution`, `interval`, `horizon`, and
/// the issue times themselves — rather than from anything inferred. A window
/// count read off the distinct issue times is a fact about the rows; a horizon
/// guessed from them would not be, because overlapping windows make the step
/// count ambiguous.
fn forecast(
    ts_type: TimeSeriesType,
    key: &SeriesKey,
    fields: &SeriesFields,
    rows: Rows,
    array: TypedArray,
    name: String,
) -> Result<TimeSeriesData> {
    let resolution = required_period(&key.resolution, "resolution", &name)?;
    let interval = required_period(&key.interval, "interval", &name)?;
    let horizon = required_period(&fields.horizon, "horizon", &name)?;
    if rows.issue.len() != rows.timestamps.len() {
        return Err(unsupported(format!(
            "series '{name}' is a forecast but has no `{ISSUE_TIME}` column"
        )));
    }

    // The window grid: its anchor is the first issue time, and its count is how
    // many distinct ones there are. Both are facts about the rows.
    let issues = distinct_sorted(&rows.issue);
    let initial_timestamp = *issues
        .first()
        .ok_or_else(|| unsupported(format!("series '{name}' is a forecast with no rows")))?;
    let count = issues.len();
    let window_of: BTreeMap<DateTime<Utc>, usize> =
        issues.iter().enumerate().map(|(i, t)| (*t, i)).collect();
    for (window, issued) in issues.iter().enumerate() {
        let expected = interval
            .add_to(initial_timestamp, window as i64)
            .ok_or_else(|| unsupported("a forecast window overflows the calendar"))?;
        if expected != *issued {
            return Err(unsupported(format!(
                "series '{name}' has an issue time of {issued}, which is not window {window} \
                 of a {} grid anchored at {initial_timestamp}",
                interval.to_iso8601()
            )));
        }
    }

    let horizon_steps = steps_in(initial_timestamp, resolution, horizon, &name)?;
    let lane_labels = lane_labels(&rows.lanes, ts_type, &name)?;
    let lane_count = lane_labels.count();
    let expected = lane_count * horizon_steps * count;
    if rows.timestamps.len() != expected {
        return Err(unsupported(format!(
            "series '{name}' has {} rows, but its grid holds {expected} values \
             ({lane_count} lanes x {horizon_steps} steps x {count} windows)",
            rows.timestamps.len()
        )));
    }

    // Place each row by its coordinates. `filled` catches a hole and a duplicate
    // in one pass, which is what makes row order irrelevant.
    let per_step = array.element_shape().iter().product::<usize>().max(1);
    let width = array.dtype.size() * per_step;
    let mut bytes = vec![0u8; expected * width];
    let mut filled = vec![false; expected];
    for row in 0..rows.timestamps.len() {
        let window = window_of[&rows.issue[row]];
        let step = step_of(
            rows.issue[row],
            rows.timestamps[row],
            resolution,
            horizon_steps,
            &name,
        )?;
        let lane = lane_labels.index_of(rows.lanes.get(row).copied())?;
        let slot = (lane * horizon_steps + step) * count + window;
        if filled[slot] {
            return Err(unsupported(format!(
                "series '{name}' has two rows for window {window}, step {step}, lane {lane}"
            )));
        }
        filled[slot] = true;
        bytes[slot * width..(slot + 1) * width]
            .copy_from_slice(&array.bytes[row * width..(row + 1) * width]);
    }
    if let Some(slot) = filled.iter().position(|f| !f) {
        let window = slot % count;
        let step = (slot / count) % horizon_steps;
        return Err(unsupported(format!(
            "series '{name}' has no row for window {window}, step {step}; a forecast cube \
             has no hole to put one in"
        )));
    }

    let mut shape = match &lane_labels {
        LaneLabels::None => vec![horizon_steps, count],
        _ => vec![lane_count, horizon_steps, count],
    };
    shape.extend_from_slice(array.element_shape());
    let cube = TypedArray::new(array.dtype, shape, bytes).map_err(unsupported)?;

    Ok(match (ts_type, lane_labels) {
        (TimeSeriesType::Probabilistic, LaneLabels::Percentiles(percentiles)) => {
            TimeSeriesData::Probabilistic(
                infrastore_core::Probabilistic::new(
                    initial_timestamp,
                    resolution,
                    horizon,
                    interval,
                    count,
                    percentiles,
                    cube,
                    name,
                )
                .map_err(unsupported)?,
            )
        }
        (TimeSeriesType::Scenarios, LaneLabels::Scenarios(scenario_count)) => {
            TimeSeriesData::Scenarios(
                infrastore_core::Scenarios::new(
                    initial_timestamp,
                    resolution,
                    horizon,
                    interval,
                    count,
                    scenario_count,
                    cube,
                    name,
                )
                .map_err(unsupported)?,
            )
        }
        // A stored `DeterministicSingleTimeSeries` reads back as the
        // `Deterministic` it is a view of, so it lands as one here too — the
        // import refuses the type by name before this, so only a real
        // `Deterministic` reaches it.
        (_, LaneLabels::None) => TimeSeriesData::Deterministic(
            infrastore_core::Deterministic::new(
                initial_timestamp,
                resolution,
                horizon,
                interval,
                count,
                cube,
                name,
            )
            .map_err(unsupported)?,
        ),
        (ts_type, _) => {
            return Err(unsupported(format!(
                "series '{name}' is a {} but carries the wrong lane column",
                ts_type.as_str()
            )));
        }
    })
}

/// What a partition's lane axis is, and how wide.
enum LaneLabels {
    None,
    Percentiles(Vec<f64>),
    Scenarios(usize),
}

impl LaneLabels {
    fn count(&self) -> usize {
        match self {
            LaneLabels::None => 1,
            LaneLabels::Percentiles(p) => p.len(),
            LaneLabels::Scenarios(n) => *n,
        }
    }

    /// Which slot on the lane axis a row's label names.
    fn index_of(&self, lane: Option<LaneValue>) -> Result<usize> {
        match (self, lane) {
            (LaneLabels::None, _) => Ok(0),
            (LaneLabels::Percentiles(labels), Some(LaneValue::Percentile(p))) => labels
                .iter()
                .position(|q| *q == p)
                .ok_or_else(|| unsupported(format!("percentile {p} is not one of {labels:?}"))),
            (LaneLabels::Scenarios(n), Some(LaneValue::Scenario(s))) => usize::try_from(s)
                .ok()
                .filter(|s| s < n)
                .ok_or_else(|| unsupported(format!("scenario {s} is outside 0..{n}"))),
            _ => Err(unsupported(
                "a row's lane column does not match its partition",
            )),
        }
    }
}

/// The lane axis a group's rows describe.
///
/// Percentiles are sorted **ascending**, which is both order-independent -- the
/// same reason the window grid's anchor is the minimum issue time -- and exactly
/// the order the core requires of a `Probabilistic`, whose constructor refuses
/// percentiles that are not strictly increasing. So there is no stored order for
/// sorting to lose.
fn lane_labels(lanes: &[LaneValue], ts_type: TimeSeriesType, name: &str) -> Result<LaneLabels> {
    match ts_type {
        TimeSeriesType::Probabilistic => {
            let mut labels: Vec<f64> = Vec::new();
            for lane in lanes {
                let LaneValue::Percentile(p) = lane else {
                    return Err(unsupported(format!(
                        "series '{name}' is a Probabilistic but its lane column is not \
                         `{PERCENTILE}`"
                    )));
                };
                labels.push(*p);
            }
            labels.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            labels.dedup();
            Ok(LaneLabels::Percentiles(labels))
        }
        TimeSeriesType::Scenarios => {
            let mut highest = 0i64;
            for lane in lanes {
                let LaneValue::Scenario(s) = lane else {
                    return Err(unsupported(format!(
                        "series '{name}' is a Scenarios but its lane column is not `{SCENARIO}`"
                    )));
                };
                highest = highest.max(*s);
            }
            Ok(LaneLabels::Scenarios(highest as usize + 1))
        }
        _ => Ok(LaneLabels::None),
    }
}

/// The distinct values of a column, **ascending**.
///
/// Sorted rather than in first-appearance order, because the window grid's
/// anchor is its earliest issue time and that has to be true however a query
/// engine left the rows. Taking the first row's would make the whole placement
/// depend on file order, which is the one thing the coordinates exist to avoid.
fn distinct_sorted(values: &[DateTime<Utc>]) -> Vec<DateTime<Utc>> {
    let mut out: Vec<DateTime<Utc>> = values.to_vec();
    out.sort_unstable();
    out.dedup();
    out
}

/// How many `resolution` steps fit in `horizon`.
///
/// Counted by walking the grid rather than dividing: a month is not a fixed
/// number of milliseconds, so the quotient would be wrong for a monthly
/// resolution inside a yearly horizon.
fn steps_in(
    anchor: DateTime<Utc>,
    resolution: Period,
    horizon: Period,
    name: &str,
) -> Result<usize> {
    let end = horizon
        .add_to(anchor, 1)
        .ok_or_else(|| unsupported("the forecast horizon overflows the calendar"))?;
    let mut steps = 0usize;
    loop {
        let at = resolution
            .add_to(anchor, steps as i64)
            .ok_or_else(|| unsupported("the forecast horizon overflows the calendar"))?;
        if at >= end {
            return Ok(steps);
        }
        steps += 1;
        if steps > 1_000_000 {
            return Err(unsupported(format!(
                "series '{name}' claims a horizon of more than a million steps; its \
                 `resolution` and `horizon` columns are probably not the ones that wrote it"
            )));
        }
    }
}

fn step_of(
    issue: DateTime<Utc>,
    target: DateTime<Utc>,
    resolution: Period,
    horizon_steps: usize,
    name: &str,
) -> Result<usize> {
    for step in 0..horizon_steps {
        let at = resolution
            .add_to(issue, step as i64)
            .ok_or_else(|| unsupported("a forecast step overflows the calendar"))?;
        if at == target {
            return Ok(step);
        }
    }
    Err(unsupported(format!(
        "series '{name}' has a target time of {target}, which is not on the \
         {horizon_steps}-step grid starting at {issue}"
    )))
}

fn required_period(text: &str, column: &str, name: &str) -> Result<Period> {
    if text.is_empty() {
        return Err(unsupported(format!(
            "series '{name}' is a forecast and needs a `{column}`: the rows say where each \
             value belongs, not what the grid it belongs to is"
        )));
    }
    Period::from_iso8601(text).map_err(|e| unsupported(format!("{column}: {e}")))
}

/// The stored cube and its leading axes, for hashing.
fn cube_of(data: &TimeSeriesData) -> (&TypedArray, Vec<usize>) {
    let array = match data {
        TimeSeriesData::SingleTimeSeries(s) => &s.data,
        TimeSeriesData::NonSequentialTimeSeries(s) => &s.data,
        TimeSeriesData::PersistentTimeSeries(s) => &s.data,
        TimeSeriesData::Deterministic(f) => &f.data,
        TimeSeriesData::Probabilistic(f) => &f.data,
        TimeSeriesData::Scenarios(f) => &f.data,
    };
    let leading = data
        .time_series_type()
        .leading_dims()
        .min(array.shape.len());
    (array, array.shape[..leading].to_vec())
}

/// Shrink a composite series back to the width its own points need.
///
/// The file padded every composite row to the widest series it shares a file
/// with; this undoes that, which is both the canonical form and what makes the
/// checksum agree with the export's.
fn canonicalize(data: &mut TimeSeriesData) -> Result<()> {
    let element_type = data.element_type();
    if !table::is_composite(element_type) {
        return Ok(());
    }
    let (array, leading) = cube_of(data);
    let shrunk = table::canonical_array(array, element_type, &leading)?;
    match data {
        TimeSeriesData::SingleTimeSeries(s) => s.data = shrunk,
        TimeSeriesData::NonSequentialTimeSeries(s) => s.data = shrunk,
        TimeSeriesData::PersistentTimeSeries(s) => s.data = shrunk,
        TimeSeriesData::Deterministic(f) => f.data = shrunk,
        TimeSeriesData::Probabilistic(f) => f.data = shrunk,
        TimeSeriesData::Scenarios(f) => f.data = shrunk,
    }
    Ok(())
}
