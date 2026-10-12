//! Reading a partition's files back, as a merge join.
//!
//! A partition is three files sharing a stem: `<stem>.values.parquet` holds
//! every distinct array once, one row per value, `<stem>.series.parquet` holds
//! one catalog row per series naming the array it reads, and
//! `<stem>.arrays.parquet` spells each array's key -- the pair
//! `(data_hash, time_axis)` -- once, under the integer `id` the other two carry
//! as `array_id`. The arrays file is read whole (it has one row per distinct
//! array); the other two are sorted by `array_id`, so the import walks them
//! together: take the next values group, file every series row naming that
//! array, move on. Peak memory is the arrays file, one values group and one row
//! group of each file, never a partition.
//!
//! Either dangling side is an error. A series row whose key names no values
//! group has no array to read; a values group no series row claims is an array
//! nothing would file. Both mean the halves came from different exports, or one
//! was truncated, and neither is something to guess about.
//!
//! A key that reappears after another key's rows, in either file, is **refused**
//! rather than stitched back together. Holding a whole file in memory to allow
//! that would give up the one property that makes a multi-gigabyte partition
//! importable.
//!
//! A values file with no series file beside it is a **foreign** file: anything
//! with a `timestamp` and a `value` column. It carries no catalog rows, so the
//! inline options supply what a series row would have.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use arrow::array::{
    Array, ArrayRef, BooleanArray, FixedSizeListArray, Float32Array, Float64Array, Int8Array,
    Int16Array, Int32Array, Int64Array, RecordBatch, StringArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, TimeUnit};
use chrono::{DateTime, TimeZone, Utc};
use infrastore_core::TypedArray;
pub use infrastore_tabular::import::reference_from_arrow_zone;
pub use infrastore_tabular::import::{ImportOptions, ImportedSeries, SeriesSink};
use infrastore_tabular::import::{
    LaneValue, SeriesRow, ValuesGroup, build, dangling, merge_join, reappeared,
};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};

use crate::partition;
use crate::schema;
use crate::table::{
    self, ARRAY_ID, ArrayKey, DATA_HASH, ISSUE_TIME, PERCENTILE, SCENARIO, TIME_AXIS, TIMESTAMP,
    VALUE,
};
use crate::{Result, arrow_err, parquet_err, unsupported};

/// One partition on disk: its values and series halves, or a lone values file.
///
/// The arrays file is not named here: it is `<stem>.arrays.parquet` beside the
/// values file, always.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionFiles {
    /// The shared stem, or the whole file name for a foreign file.
    pub stem: String,
    pub values: PathBuf,
    /// `None` for a **foreign** file — a values file with no series file beside
    /// it, which carries no catalog rows and needs the inline options instead.
    pub series: Option<PathBuf>,
}

impl PartitionFiles {
    /// What to call this partition in a message: the stem, or the lone file.
    pub fn label(&self) -> String {
        match &self.series {
            Some(_) => self.stem.clone(),
            None => self.values.display().to_string(),
        }
    }
}

/// The partitions at `path`, which may be a directory, one file, or a stem.
///
/// Sorted, so a directory import is deterministic and a failure part-way through
/// names the same partition every run.
///
/// A `.series.parquet` with no `.values.parquet` beside it is an **error**: its
/// catalog rows name arrays that are not there, which is a truncated export
/// rather than anything importable. The mirror case is not an error, because a
/// values file alone is exactly what a foreign file looks like.
pub fn partitions(path: &Path) -> Result<Vec<PartitionFiles>> {
    // A stem: nothing at the path itself, but a values file beside it.
    if !path.exists()
        && let Some(name) = path.file_name().and_then(|n| n.to_str())
    {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let values = dir.join(partition::values_name(name));
        if values.is_file() {
            let series = dir.join(partition::series_name(name));
            return Ok(vec![PartitionFiles {
                stem: name.to_string(),
                values,
                series: series.is_file().then_some(series),
            }]);
        }
    }
    if path.is_file() {
        return Ok(vec![one_file(path)?]);
    }
    if !path.is_dir() {
        return Err(unsupported(format!(
            "{} is neither a Parquet file, a partition stem, nor a directory of them",
            path.display()
        )));
    }

    let mut names: Vec<String> = std::fs::read_dir(path)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
        })
        .collect();
    names.sort();

    // A series half whose values half is missing, checked before anything is
    // read so the message is about the truncation rather than about a column.
    for name in &names {
        if let Some(stem) = name.strip_suffix(partition::SERIES_SUFFIX)
            && !path.join(partition::values_name(stem)).is_file()
        {
            return Err(unsupported(format!(
                "{name} has no {} beside it: its rows name arrays that are not there",
                partition::values_name(stem)
            )));
        }
    }

    let mut out: Vec<PartitionFiles> = Vec::new();
    for name in &names {
        if name.ends_with(partition::SERIES_SUFFIX) {
            continue; // paired below, from its values half
        }
        if let Some(stem) = name.strip_suffix(partition::ARRAYS_SUFFIX) {
            if !path.join(partition::values_name(stem)).is_file() {
                return Err(unsupported(format!(
                    "{name} has no {} beside it: it names arrays that are not there",
                    partition::values_name(stem)
                )));
            }
            continue; // read with its values half
        }
        out.push(one_file(&path.join(name))?);
    }
    if out.is_empty() {
        return Err(unsupported(format!(
            "{} holds no .parquet files",
            path.display()
        )));
    }
    Ok(out)
}

/// One named file: its partner if it has one, else a foreign file.
fn one_file(path: &Path) -> Result<PartitionFiles> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| unsupported(format!("{} has no file name", path.display())))?;
    if let Some(stem) = name
        .strip_suffix(partition::SERIES_SUFFIX)
        .or_else(|| name.strip_suffix(partition::ARRAYS_SUFFIX))
    {
        // Naming the series or arrays file is naming the partition.
        let values = path.with_file_name(partition::values_name(stem));
        if !values.is_file() {
            return Err(unsupported(format!(
                "{name} has no {} beside it: its rows name arrays that are not there",
                partition::values_name(stem)
            )));
        }
        let series = path.with_file_name(partition::series_name(stem));
        return Ok(PartitionFiles {
            stem: stem.to_string(),
            values,
            series: series.is_file().then_some(series),
        });
    }
    let stem = partition::stem_of(name).unwrap_or(name);
    let series = path.with_file_name(partition::series_name(stem));
    Ok(PartitionFiles {
        stem: stem.to_string(),
        values: path.to_path_buf(),
        series: series.is_file().then_some(series),
    })
}

/// Stream one partition's series into `sink`, returning how many were handed
/// over.
pub fn read_partition_with(
    files: &PartitionFiles,
    options: &ImportOptions,
    sink: SeriesSink<'_>,
) -> Result<usize> {
    let arrays_path = files
        .values
        .with_file_name(partition::arrays_name(&files.stem));
    let Some(series_path) = &files.series else {
        return read_foreign_with(&files.values, &arrays_path, options, sink);
    };
    if !arrays_path.is_file() {
        // A v1 export never had an arrays file, so refuse it by version before
        // reporting the file as missing.
        let file = std::fs::File::open(&files.values)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(parquet_err)?;
        let footer = builder.schema().metadata().clone().into_iter().collect();
        check_format(&footer, &files.values)?;
        return Err(unsupported(format!(
            "{} has no {} beside it: its rows name arrays by an id only that file resolves",
            files.values.display(),
            partition::arrays_name(&files.stem)
        )));
    }
    let arrays = read_arrays(&arrays_path)?;
    let mut values = ValuesReader::open(&files.values, Some(arrays.keys.clone()))?;
    let mut rows = SeriesReader::open(series_path, arrays.keys.clone())?;
    check_pair(
        &values.footer,
        &rows.footer,
        &files.values,
        series_path,
        table::ROLE_SERIES,
    )?;
    check_pair(
        &values.footer,
        &arrays.footer,
        &files.values,
        &arrays_path,
        table::ROLE_ARRAYS,
    )?;
    let footer = values.footer.clone();

    merge_join(
        || values.next_group(),
        || rows.next_group(),
        &footer,
        options,
        sink,
    )
}

/// Read a **foreign** values file: one with no series file beside it, and so no
/// catalog rows at all. One series per distinct `array_id` if the column is
/// there, one series in total if it is not. An arrays file beside it, if there
/// is one, resolves each id to its key, so the checksum still runs.
fn read_foreign_with(
    path: &Path,
    arrays_path: &Path,
    options: &ImportOptions,
    sink: SeriesSink<'_>,
) -> Result<usize> {
    let arrays = arrays_path
        .is_file()
        .then(|| read_arrays(arrays_path))
        .transpose()?;
    let mut values = ValuesReader::open(path, arrays.map(|a| a.keys))?;
    let footer = values.footer.clone();
    let mut filed = 0usize;
    while let Some(group) = values.next_group()? {
        let row = SeriesRow::bare(group.key.clone());
        sink(build(&group, &row, &footer, options)?)?;
        filed += 1;
    }
    Ok(filed)
}

/// Refuse a file this build cannot read, by version rather than by whichever
/// column it turns out to be missing.
///
/// A file with no marker at all is **foreign** and allowed: reading a stranger's
/// Parquet is the whole point of the fallbacks below.
fn check_format(footer: &BTreeMap<String, String>, path: &Path) -> Result<()> {
    match footer.get(table::FORMAT) {
        None => Ok(()),
        Some(v) if v == table::FORMAT_V2 => Ok(()),
        Some(other) => Err(unsupported(format!(
            "{} is {other}, which this build does not read (it reads {}); export it again \
             with a build that matches",
            path.display(),
            table::FORMAT_V2
        ))),
    }
}

/// Refuse two files that do not describe the same partition: the values file
/// and one of the other two, whose role `expected_role` names.
///
/// The merge join pairs by **file name**, and a name is easy to arrange by
/// accident: copying one half of one export next to the other half of another
/// leaves two files whose stems match and whose contents do not. If they happen
/// to share array keys the join succeeds and the checksum passes -- the values
/// really do hash to what the values file says -- while every catalog field
/// comes from the wrong export. A UTC values file paired with an `unspecified`
/// series file changes a series' `time_reference` and says nothing.
///
/// So the footers are compared first. Each file carries the whole
/// `PartitionKey`, written by one export, plus the role it plays; a
/// disagreement in either is refused naming the field and both files.
///
/// Two **unmarked** files are left alone: a pair of foreign files that happen to
/// be named as a partition is not something this format can have opinions about.
/// One marked and one not is refused, because our export writes the marker on
/// both.
fn check_pair(
    values: &BTreeMap<String, String>,
    series: &BTreeMap<String, String>,
    values_path: &Path,
    series_path: &Path,
    expected_role: &str,
) -> Result<()> {
    let names = || {
        (
            values_path.display().to_string(),
            series_path.display().to_string(),
        )
    };
    match (
        values.contains_key(table::FORMAT),
        series.contains_key(table::FORMAT),
    ) {
        (false, false) => return Ok(()),
        (true, true) => {}
        (marked, _) => {
            let (v, s) = names();
            let (with, without) = if marked { (v, s) } else { (s, v) };
            return Err(unsupported(format!(
                "{with} is an infrastore partition file and {without} is not; a partition's \
                 files are written together, so this pair was assembled by hand"
            )));
        }
    }
    for (footer, expected, path) in [
        (values, table::ROLE_VALUES, values_path),
        (series, expected_role, series_path),
    ] {
        let role = footer.get(table::ROLE).map(String::as_str);
        if role != Some(expected) {
            return Err(unsupported(format!(
                "{} says it is the `{}` half of a partition, but it is being read as the \
                 `{expected}` half",
                path.display(),
                role.unwrap_or("unnamed"),
            )));
        }
    }
    for key in table::PARTITION_KEYS {
        let mine = values.get(key).map(String::as_str).unwrap_or_default();
        let theirs = series.get(key).map(String::as_str).unwrap_or_default();
        if mine != theirs {
            let (v, s) = names();
            return Err(unsupported(format!(
                "the two halves disagree about `{key}`: {v} says {mine:?} and {s} says \
                 {theirs:?}. A partition's halves are one export's, so these came from two."
            )));
        }
    }
    Ok(())
}

/// Refuse a file of **ours** that is missing a column the format requires.
///
/// Every column in both halves is required -- that is what the partitioning
/// buys -- and the reader used to treat almost all of them as optional, so a
/// series file with `features` projected away imported an empty feature set and
/// changed every series' identity without a word. Checked up front, by name,
/// against the whole per-type column set.
///
/// Only files carrying the format marker are held to it: a foreign file is
/// allowed to carry two columns and let the inline options say the rest.
fn check_columns(
    arrow: &arrow::datatypes::Schema,
    footer: &BTreeMap<String, String>,
    path: &Path,
    role: &str,
) -> Result<()> {
    if !footer.contains_key(table::FORMAT) {
        return Ok(());
    }
    let Some(declared) = footer
        .get(schema::TIME_SERIES_TYPE)
        .filter(|t| !t.is_empty())
    else {
        return Err(unsupported(format!(
            "{} is an infrastore partition file with no `{}` in its footer, so there is no \
             telling which columns it should have",
            path.display(),
            schema::TIME_SERIES_TYPE
        )));
    };
    let ts_type = schema::decode_time_series_type(declared).map_err(unsupported)?;
    let missing: Vec<&str> = table::required_columns(ts_type, role)
        .into_iter()
        .filter(|name| arrow.field_with_name(name).is_err())
        .collect();
    if !missing.is_empty() {
        return Err(unsupported(format!(
            "{} is the {role} half of a {} partition and is missing {}: every column of this \
             format is required, so a reader never has to guess what an absent one meant",
            path.display(),
            ts_type.as_str(),
            missing
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

/// Streams a values file, yielding one array's rows at a time.
struct ValuesReader {
    reader: ParquetRecordBatchReader,
    footer: BTreeMap<String, String>,
    /// What each `array_id` stands for; `None` for a foreign file with no arrays
    /// file beside it.
    arrays: Option<Rc<HashMap<i64, ArrayKey>>>,
    zone: Option<String>,
    /// The `array_id` the open group's rows carry.
    open_id: Option<i64>,
    open: Option<ValuesGroup>,
    ready: VecDeque<ValuesGroup>,
    seen: HashSet<ArrayKey>,
    done: bool,
}

impl ValuesReader {
    fn open(path: &Path, arrays: Option<Rc<HashMap<i64, ArrayKey>>>) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(parquet_err)?;
        let footer: BTreeMap<String, String> = builder
            .schema()
            .metadata()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        check_format(&footer, path)?;
        check_columns(builder.schema(), &footer, path, table::ROLE_VALUES)?;
        Ok(Self {
            reader: builder.build().map_err(parquet_err)?,
            footer,
            arrays,
            zone: None,
            open_id: None,
            open: None,
            ready: VecDeque::new(),
            seen: HashSet::new(),
            done: false,
        })
    }

    /// The next completed group, reading row groups until one closes.
    fn next_group(&mut self) -> Result<Option<ValuesGroup>> {
        loop {
            if let Some(group) = self.ready.pop_front() {
                return Ok(Some(group));
            }
            if self.done {
                return Ok(self.open.take());
            }
            match self.reader.next() {
                None => self.done = true,
                Some(batch) => {
                    let batch = batch.map_err(arrow_err)?;
                    self.push_batch(&batch)?;
                }
            }
        }
    }

    fn push_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        if self.zone.is_none() {
            self.zone = arrow_zone(batch, TIMESTAMP)?;
        }
        // Absent for a foreign file, which is then one group with an empty key.
        let ids = int_column(batch, ARRAY_ID)?;
        let stamps = read_timestamps(batch, TIMESTAMP)?;
        let issued = batch
            .column_by_name(ISSUE_TIME)
            .map(|_| read_timestamps(batch, ISSUE_TIME))
            .transpose()?;
        let lanes = read_lanes(batch)?;
        let (dims, values) = batch_values(batch)?;
        let width: usize = dims.iter().product::<usize>().max(1);
        let stride = values.dtype.size();

        for row in 0..batch.num_rows() {
            let id = ids.as_ref().map(|c| c[row]);
            if self.open.is_none() || self.open_id != id {
                let key = match (id, &self.arrays) {
                    (Some(id), Some(arrays)) => arrays
                        .get(&id)
                        .cloned()
                        .ok_or_else(|| dangling(id, table::ROLE_VALUES))?,
                    // Nothing to resolve it against: the id still groups the
                    // rows, and the empty hash is what skips the checksum.
                    (id, _) => ArrayKey {
                        data_hash: String::new(),
                        time_axis: id.map(|id| id.to_string()).unwrap_or_default(),
                    },
                };
                self.open_id = id;
                if let Some(group) = self.open.take() {
                    self.ready.push_back(group);
                }
                if !self.seen.insert(key.clone()) {
                    return Err(reappeared(&key, "values"));
                }
                self.open = Some(ValuesGroup {
                    key,
                    timestamps: Vec::new(),
                    issue: Vec::new(),
                    lanes: Vec::new(),
                    dims: dims.clone(),
                    dtype: values.dtype,
                    bytes: Vec::new(),
                    zone: self.zone.clone(),
                });
            }
            let group = self.open.as_mut().expect("just opened");
            if group.dims != dims || group.dtype != values.dtype {
                return Err(unsupported(
                    "two row groups disagree about the value column's type",
                ));
            }
            group.timestamps.push(stamps[row]);
            if let Some(issued) = &issued {
                group.issue.push(issued[row]);
            }
            if let Some(lanes) = &lanes {
                group.lanes.push(lanes[row]);
            }
            let start = row * width * stride;
            group
                .bytes
                .extend_from_slice(&values.bytes[start..start + width * stride]);
        }
        Ok(())
    }
}

/// Streams a series file, yielding every row of one array key at a time.
struct SeriesReader {
    reader: ParquetRecordBatchReader,
    /// Kept rather than dropped after the version check: [`check_pair`] compares
    /// it against the values half's.
    footer: BTreeMap<String, String>,
    arrays: Rc<HashMap<i64, ArrayKey>>,
    open: Vec<SeriesRow>,
    ready: VecDeque<Vec<SeriesRow>>,
    seen: HashSet<ArrayKey>,
    done: bool,
}

impl SeriesReader {
    fn open(path: &Path, arrays: Rc<HashMap<i64, ArrayKey>>) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(parquet_err)?;
        let footer: BTreeMap<String, String> = builder
            .schema()
            .metadata()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        check_format(&footer, path)?;
        check_columns(builder.schema(), &footer, path, table::ROLE_SERIES)?;
        Ok(Self {
            reader: builder.build().map_err(parquet_err)?,
            footer,
            arrays,
            open: Vec::new(),
            ready: VecDeque::new(),
            seen: HashSet::new(),
            done: false,
        })
    }

    fn next_group(&mut self) -> Result<Option<Vec<SeriesRow>>> {
        loop {
            if let Some(group) = self.ready.pop_front() {
                return Ok(Some(group));
            }
            if self.done {
                return Ok((!self.open.is_empty()).then(|| std::mem::take(&mut self.open)));
            }
            match self.reader.next() {
                None => self.done = true,
                Some(batch) => {
                    let batch = batch.map_err(arrow_err)?;
                    for row in read_series_rows(&batch, &self.arrays)? {
                        if self.open.first().is_none_or(|r| r.key != row.key) {
                            if !self.open.is_empty() {
                                self.ready.push_back(std::mem::take(&mut self.open));
                            }
                            if !self.seen.insert(row.key.clone()) {
                                return Err(reappeared(&row.key, "series"));
                            }
                        }
                        self.open.push(row);
                    }
                }
            }
        }
    }
}

/// One batch of a series file as catalog rows.
fn read_series_rows(
    batch: &RecordBatch,
    arrays: &HashMap<i64, ArrayKey>,
) -> Result<Vec<SeriesRow>> {
    let array_ids = int_column(batch, ARRAY_ID)?.ok_or_else(|| {
        unsupported(format!(
            "the series file has no `{ARRAY_ID}` column, which names the array every row is \
             filed under"
        ))
    })?;
    let id = int_column(batch, schema::ID)?;
    let owner_id = int_column(batch, schema::OWNER_ID)?;
    let owner_type = text_column(batch, schema::OWNER_TYPE)?;
    let owner_category = text_column(batch, schema::OWNER_CATEGORY)?;
    let ts_type = text_column(batch, schema::TIME_SERIES_TYPE)?;
    let name = text_column(batch, schema::NAME)?;
    let resolution = text_column(batch, schema::RESOLUTION)?;
    let interval = text_column(batch, schema::INTERVAL)?;
    let horizon = text_column(batch, schema::HORIZON)?;
    let features = text_column(batch, schema::FEATURES)?;
    let element_type = text_column(batch, schema::ELEMENT_TYPE)?;
    let time_reference = text_column(batch, schema::TIME_REFERENCE)?;
    let units = text_column(batch, schema::UNITS)?;
    let quantity_kind = text_column(batch, schema::QUANTITY_KIND)?;
    let unit_system = text_column(batch, schema::UNIT_SYSTEM)?;
    let component_field = text_column(batch, schema::COMPONENT_FIELD)?;
    let application_data = text_column(batch, schema::APPLICATION_DATA)?;

    (0..batch.num_rows())
        .map(|i| {
            let key = arrays
                .get(&array_ids[i])
                .cloned()
                .ok_or_else(|| dangling(array_ids[i], table::ROLE_SERIES))?;
            Ok(SeriesRow {
                key,
                id: id.as_ref().map_or(0, |c| c[i]),
                owner_id: owner_id.as_ref().map(|c| c[i]),
                owner_type: cell(&owner_type, i),
                owner_category: cell(&owner_category, i),
                time_series_type: cell(&ts_type, i),
                name: cell(&name, i),
                resolution: cell(&resolution, i),
                interval: cell(&interval, i),
                horizon: cell(&horizon, i),
                features: cell(&features, i),
                element_type: cell(&element_type, i),
                time_reference: cell(&time_reference, i),
                units: cell(&units, i),
                quantity_kind: cell(&quantity_kind, i),
                unit_system: cell(&unit_system, i),
                component_field: cell(&component_field, i),
                application_data: cell(&application_data, i),
            })
        })
        .collect()
}

/// A partition's arrays file, whole: one row per distinct array, so even a very
/// large partition's is small.
struct Arrays {
    footer: BTreeMap<String, String>,
    keys: Rc<HashMap<i64, ArrayKey>>,
}

/// Read an arrays file, checking that `id` and the key it stands for ascend
/// together. That is what makes the other two files, sorted by `array_id`,
/// sorted by key as well -- the order the merge join walks them in.
fn read_arrays(path: &Path) -> Result<Arrays> {
    let file = std::fs::File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(parquet_err)?;
    let footer: BTreeMap<String, String> = builder
        .schema()
        .metadata()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    check_format(&footer, path)?;
    check_columns(builder.schema(), &footer, path, table::ROLE_ARRAYS)?;
    let missing = |name: &str| {
        unsupported(format!(
            "{} has no `{name}` column; an arrays file is `{}`, `{DATA_HASH}`, `{TIME_AXIS}`",
            path.display(),
            schema::ID
        ))
    };
    let mut keys = HashMap::new();
    let mut last: Option<(i64, ArrayKey)> = None;
    for batch in builder.build().map_err(parquet_err)? {
        let batch = batch.map_err(arrow_err)?;
        let ids = int_column(&batch, schema::ID)?.ok_or_else(|| missing(schema::ID))?;
        let hashes = text_column(&batch, DATA_HASH)?.ok_or_else(|| missing(DATA_HASH))?;
        let axes = text_column(&batch, TIME_AXIS)?.ok_or_else(|| missing(TIME_AXIS))?;
        for ((id, data_hash), time_axis) in ids.into_iter().zip(hashes).zip(axes) {
            let key = ArrayKey {
                data_hash,
                time_axis,
            };
            if last.as_ref().is_some_and(|(i, k)| *i >= id || *k >= key) {
                return Err(unsupported(format!(
                    "{} is not sorted: `{}` and (`{DATA_HASH}`, `{TIME_AXIS}`) must both \
                     strictly ascend, row by row",
                    path.display(),
                    schema::ID
                )));
            }
            keys.insert(id, key.clone());
            last = Some((id, key));
        }
    }
    Ok(Arrays {
        footer,
        keys: Rc::new(keys),
    })
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
