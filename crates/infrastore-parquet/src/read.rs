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
use crate::table::{self, DATA_HASH, TIMESTAMP, VALUE};
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

/// Read one file into the series it holds.
///
/// The row groups are streamed; the series are collected, because the caller
/// files a whole file in one transaction (§2.8) and so needs them together. A
/// file is one partition, so this is bounded by one partition's worth.
pub fn read_file(path: &Path, options: &ImportOptions) -> Result<Vec<ImportedSeries>> {
    let file = std::fs::File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(parquet_err)?;
    let schema = builder.schema().clone();
    let footer: BTreeMap<String, String> = schema
        .metadata()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    check_format(&footer, path)?;

    let mut grouper = Grouper::new(options, &footer);
    let reader = builder.build().map_err(parquet_err)?;
    for batch in reader {
        let batch = batch.map_err(arrow_err)?;
        grouper.push_batch(&batch)?;
    }
    grouper.finish()
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
    values: Vec<u8>,
    dtype: Option<Dtype>,
    element_dims: Vec<usize>,
    seen: HashSet<SeriesKey>,
    out: Vec<ImportedSeries>,
}

impl<'a> Grouper<'a> {
    fn new(options: &'a ImportOptions, footer: &'a BTreeMap<String, String>) -> Self {
        Self {
            options,
            footer,
            current: None,
            timestamps: Vec::new(),
            values: Vec::new(),
            dtype: None,
            element_dims: Vec::new(),
            seen: HashSet::new(),
            out: Vec::new(),
        }
    }

    fn push_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let stamps = read_timestamps(batch, TIMESTAMP)?;
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
            let start = row * width * stride;
            self.values
                .extend_from_slice(&values.bytes[start..start + width * stride]);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<ImportedSeries>> {
        self.flush()?;
        Ok(self.out)
    }

    fn flush(&mut self) -> Result<()> {
        let Some((key, fields)) = self.current.take() else {
            return Ok(());
        };
        let timestamps = std::mem::take(&mut self.timestamps);
        let bytes = std::mem::take(&mut self.values);
        let dtype = self.dtype.expect("a flushed group has seen a batch");

        let mut shape = vec![timestamps.len()];
        shape.extend_from_slice(&self.element_dims);
        let array = TypedArray::new(dtype, shape, bytes).map_err(unsupported)?;
        let series = self.build(&key, &fields, timestamps, array)?;
        self.out.push(series);
        Ok(())
    }

    fn build(
        &self,
        key: &SeriesKey,
        fields: &SeriesFields,
        timestamps: Vec<DateTime<Utc>>,
        array: TypedArray,
    ) -> Result<ImportedSeries> {
        let ts_type = self.resolve_type(key, &timestamps)?;
        let element_type = self.resolve_element_type(fields, &array)?;
        let reference = self.resolve_reference(fields)?;
        let name = self.resolve_name(key)?;

        // A composite row was padded to the file's width; shrink it back to the
        // width its own points need, which is both the canonical form and what
        // makes the checksum below agree with the export's.
        let array = table::canonical_array(&array, element_type, &[timestamps.len()])?;
        self.verify_checksum(key, fields, &array, element_type, &timestamps)?;

        // Resolved before `assemble` takes the name.
        let owner_id = self.resolve_owner_id(key, &name)?;
        let owner_type = self.resolve_owner_type(fields, &name)?;
        let mut data = self.assemble(ts_type, key, timestamps, array, name)?;
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
        data.set_descriptors(descriptors);

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
            // A foreign file: the column's zone is all there is.
            None => Ok(None),
        }
    }

    /// Compare the file's `data_hash` against the group as re-encoded.
    ///
    /// Skipped when the column is absent, which is what a foreign file and an
    /// edited one both look like: a user who changes values in DuckDB drops the
    /// column rather than recomputing it.
    fn verify_checksum(
        &self,
        key: &SeriesKey,
        fields: &SeriesFields,
        array: &TypedArray,
        element_type: ElementType,
        timestamps: &[DateTime<Utc>],
    ) -> Result<()> {
        if self.options.skip_checksum || fields.data_hash.is_empty() {
            return Ok(());
        }
        let actual = table::canonical_hash(array, element_type, &[timestamps.len()])?;
        if actual != fields.data_hash {
            return Err(unsupported(format!(
                "series '{}' does not match its recorded data_hash: the file says {} and its \
                 rows hash to {actual}. If the values were edited, drop the `{DATA_HASH}` \
                 column.",
                key.name, fields.data_hash
            )));
        }
        Ok(())
    }

    fn assemble(
        &self,
        ts_type: TimeSeriesType,
        key: &SeriesKey,
        timestamps: Vec<DateTime<Utc>>,
        array: TypedArray,
        name: String,
    ) -> Result<TimeSeriesData> {
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
                "the long-table import does not yet cover {}",
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
    Ok(Some(
        (0..array.len())
            .map(|i| {
                if array.is_null(i) {
                    String::new()
                } else {
                    array.value(i).to_string()
                }
            })
            .collect(),
    ))
}

fn int_column(batch: &RecordBatch, name: &str) -> Result<Option<Vec<i64>>> {
    let Some(column) = batch.column_by_name(name) else {
        return Ok(None);
    };
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
