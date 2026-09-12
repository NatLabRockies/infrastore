//! Reading a partition's two files back, as a merge join.
//!
//! A partition is a **pair** sharing a stem: `<stem>.values.parquet` holds every
//! distinct array once, one row per value, and `<stem>.series.parquet` holds one
//! catalog row per series naming the array it reads. Both are sorted by the
//! array key -- the pair `(data_hash, time_axis)` -- so the import walks them
//! together: take the next values group, file every series row carrying that
//! key, move on. Peak memory is one values group plus one row group of each
//! file, never a partition.
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

use std::collections::{BTreeMap, HashSet, VecDeque};
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
    TypedArray, UnitSystem,
};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};

use crate::partition;
use crate::schema;
use crate::table::{
    self, ArrayKey, DATA_HASH, ISSUE_TIME, PERCENTILE, SCENARIO, TIME_AXIS, TIMESTAMP, VALUE,
};
use crate::{Result, arrow_err, parquet_err, unsupported};

/// One series read out of a partition, with the catalog fields a caller needs
/// to file it.
#[derive(Debug, Clone)]
pub struct ImportedSeries {
    /// The id the file recorded for this series' row, for reporting only.
    ///
    /// Never used to file the row: "never reissued" is a guarantee of the
    /// catalog's `AUTOINCREMENT`, and a caller free to name an id could re-file
    /// a retired one.
    pub recorded_id: Option<i64>,
    /// The array this series read, for reporting only -- a `--dry-run` says how
    /// many *distinct* arrays a partition holds, which is the whole difference
    /// between this layout and a denormalized one.
    ///
    /// `None` for a foreign file that carries no key columns.
    pub array: Option<ArrayKey>,
    pub owner_id: i64,
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub features: Features,
    pub data: TimeSeriesData,
}

/// What the caller asserts or supplies on top of the files.
///
/// Two kinds of field, and the difference is the project's usual one.
///
/// An **override** replaces the corresponding column for every series in the
/// partition: the owner, the name, the feature set, and the five free-form
/// descriptors. An override of `Some("")` clears a descriptor rather than
/// storing an empty one, since the empty string is how this format spells
/// "absent" anyway.
///
/// An **assertion** states something the file cannot, and a file that
/// contradicts it is an error rather than being silently replaced:
/// `time_series_type`, `element_type` (whether a `FixedSizeList<double>[3]` is a
/// tuple or a dense row — the bytes cannot say), `element_shape` and
/// `resolution`. On a foreign file, which says nothing to contradict, an
/// assertion is simply the answer.
#[derive(Debug, Default, Clone)]
pub struct ImportOptions {
    pub time_series_type: Option<TimeSeriesType>,
    pub element_type: Option<ElementType>,
    /// Assertion: the per-step shape the `value` column carries.
    pub element_shape: Option<Vec<usize>>,
    /// Assertion, and the grid for a foreign file whose rows walk one.
    pub resolution: Option<Period>,
    pub name: Option<String>,
    pub owner_id: Option<i64>,
    pub owner_type: Option<String>,
    pub owner_category: Option<OwnerCategory>,
    pub time_reference: Option<TimeReference>,
    pub features: Option<Features>,
    pub units: Option<String>,
    pub quantity_kind: Option<String>,
    pub unit_system: Option<UnitSystem>,
    pub component_field: Option<String>,
    pub application_data: Option<String>,
    /// Waive the `data_hash` comparison, leaving the pair as nothing but a join
    /// key. `--no-checksum` on the CLI; the alternative is to recompute the hash
    /// after editing values.
    pub skip_checksum: bool,
}

/// One partition on disk: its two halves, or a lone values file.
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
    if let Some(stem) = name.strip_suffix(partition::SERIES_SUFFIX) {
        // Naming the series half is naming the partition, so read the pair.
        let values = path.with_file_name(partition::values_name(stem));
        if !values.is_file() {
            return Err(unsupported(format!(
                "{name} has no {} beside it: its rows name arrays that are not there",
                partition::values_name(stem)
            )));
        }
        return Ok(PartitionFiles {
            stem: stem.to_string(),
            values,
            series: Some(path.to_path_buf()),
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

/// A place each completed series goes as soon as its rows are read.
///
/// The whole point of the layout is that a partition can be far larger than
/// memory, so the reader never holds more than the array it is in the middle of.
/// A caller filing a partition in one transaction opens the transaction, hands
/// this a closure that adds each series, and commits when the call returns.
pub type SeriesSink<'a> = &'a mut dyn FnMut(ImportedSeries) -> Result<()>;

/// Stream one partition's series into `sink`, returning how many were handed
/// over.
pub fn read_partition_with(
    files: &PartitionFiles,
    options: &ImportOptions,
    sink: SeriesSink<'_>,
) -> Result<usize> {
    let Some(series_path) = &files.series else {
        return read_foreign_with(&files.values, options, sink);
    };
    let mut values = ValuesReader::open(&files.values)?;
    let mut rows = SeriesReader::open(series_path)?;
    check_pair(&values.footer, &rows.footer, files)?;
    let footer = values.footer.clone();

    let mut filed = 0usize;
    let mut group = values.next_group()?;
    let mut batch = rows.next_group()?;
    loop {
        match (group.as_ref(), batch.as_ref()) {
            (None, None) => return Ok(filed),
            (Some(g), None) => return Err(unclaimed(&g.key)),
            (None, Some(b)) => return Err(unbacked(&b[0].key)),
            (Some(g), Some(b)) => match g.key.cmp(&b[0].key) {
                std::cmp::Ordering::Less => return Err(unclaimed(&g.key)),
                std::cmp::Ordering::Greater => return Err(unbacked(&b[0].key)),
                std::cmp::Ordering::Equal => {}
            },
        }
        let held = group.take().expect("matched just above");
        for row in &batch.take().expect("matched just above") {
            sink(build(&held, row, &footer, options)?)?;
            filed += 1;
        }
        group = values.next_group()?;
        batch = rows.next_group()?;
    }
}

/// [`read_partition_with`], collecting.
///
/// For a caller that wants the partition in hand — a test, a dry run over a
/// small pair. A load should stream instead: this holds every series until the
/// end.
pub fn read_partition(
    files: &PartitionFiles,
    options: &ImportOptions,
) -> Result<Vec<ImportedSeries>> {
    let mut out = Vec::new();
    read_partition_with(files, options, &mut |series| {
        out.push(series);
        Ok(())
    })?;
    Ok(out)
}

/// Read a **foreign** values file: one with no series file beside it, and so no
/// catalog rows at all. One series per distinct key if the key columns are
/// there, one series in total if they are not.
fn read_foreign_with(path: &Path, options: &ImportOptions, sink: SeriesSink<'_>) -> Result<usize> {
    let mut values = ValuesReader::open(path)?;
    let footer = values.footer.clone();
    let mut filed = 0usize;
    while let Some(group) = values.next_group()? {
        let row = SeriesRow::bare(group.key.clone());
        sink(build(&group, &row, &footer, options)?)?;
        filed += 1;
    }
    Ok(filed)
}

/// A values group no series row claims.
fn unclaimed(key: &ArrayKey) -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "the values file holds an array ({}) that no series row names; a partition's two \
         halves come from one export and must describe the same arrays",
        short(key)
    ))
}

/// A series row whose key names no values group.
fn unbacked(key: &ArrayKey) -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "a series row names an array ({}) the values file does not hold; a partition's two \
         halves come from one export and must describe the same arrays",
        short(key)
    ))
}

/// A key that comes back after another key's rows.
fn reappeared(key: &ArrayKey, half: &str) -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "the {half} file returns to array ({}) after another key's rows; both halves must be \
         sorted by `{DATA_HASH}`, `{TIME_AXIS}`",
        short(key)
    ))
}

/// A key, shortened for a message. The full hash is in the file.
fn short(key: &ArrayKey) -> String {
    format!(
        "{}, {}",
        key.data_hash.chars().take(12).collect::<String>(),
        key.time_axis
    )
}

/// Refuse a file this build cannot read, by version rather than by whichever
/// column it turns out to be missing.
///
/// A file with no marker at all is **foreign** and allowed: reading a stranger's
/// Parquet is the whole point of the fallbacks below.
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

/// Refuse two halves that do not describe the same partition.
///
/// The merge join pairs by **file name**, and a name is easy to arrange by
/// accident: copying one half of one export next to the other half of another
/// leaves two files whose stems match and whose contents do not. If they happen
/// to share array keys the join succeeds and the checksum passes -- the values
/// really do hash to what the values file says -- while every catalog field
/// comes from the wrong export. A UTC values file paired with an `unspecified`
/// series file changes a series' `time_reference` and says nothing.
///
/// So the footers are compared first. Each half carries the whole
/// `PartitionKey`, written twice by one export, plus the role it plays; a
/// disagreement in either is refused naming the field and both files.
///
/// Two **unmarked** files are left alone: a pair of foreign files that happen to
/// be named as a partition is not something this format can have opinions about.
/// One marked and one not is refused, because our export writes the marker on
/// both.
fn check_pair(
    values: &BTreeMap<String, String>,
    series: &BTreeMap<String, String>,
    files: &PartitionFiles,
) -> Result<()> {
    let series_path = files.series.as_ref().expect("only called on a pair");
    let names = || {
        (
            files.values.display().to_string(),
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
                "{with} is an infrastore partition file and {without} is not; a partition's two \
                 halves are written together, so this pair was assembled by hand"
            )));
        }
    }
    for (footer, expected, path) in [
        (values, table::ROLE_VALUES, &files.values),
        (series, table::ROLE_SERIES, series_path),
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

/// One array's rows, as the values file holds them.
struct ValuesGroup {
    key: ArrayKey,
    timestamps: Vec<DateTime<Utc>>,
    /// Empty for a static partition.
    issue: Vec<DateTime<Utc>>,
    /// Empty for a static partition and for `Deterministic`.
    lanes: Vec<LaneValue>,
    /// The per-step shape, from the `value` column's nesting.
    dims: Vec<usize>,
    dtype: Dtype,
    bytes: Vec<u8>,
    /// The `timestamp` column's own Arrow zone, the last resort for a foreign
    /// file that names no `time_reference` anywhere.
    zone: Option<String>,
}

impl ValuesGroup {
    fn array(&self) -> Result<TypedArray> {
        let mut shape = vec![self.timestamps.len()];
        shape.extend_from_slice(&self.dims);
        TypedArray::new(self.dtype, shape, self.bytes.clone()).map_err(unsupported)
    }
}

/// Streams a values file, yielding one array's rows at a time.
struct ValuesReader {
    reader: ParquetRecordBatchReader,
    footer: BTreeMap<String, String>,
    zone: Option<String>,
    open: Option<ValuesGroup>,
    ready: VecDeque<ValuesGroup>,
    seen: HashSet<ArrayKey>,
    done: bool,
}

impl ValuesReader {
    fn open(path: &Path) -> Result<Self> {
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
            zone: None,
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
        let hashes = text_column(batch, DATA_HASH)?;
        let axes = text_column(batch, TIME_AXIS)?;
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
            let key = ArrayKey {
                data_hash: cell(&hashes, row),
                time_axis: cell(&axes, row),
            };
            if self.open.as_ref().is_none_or(|g| g.key != key) {
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

/// One catalog row out of a series file.
///
/// Every field is the column's text as written; the empty string is how an
/// absent one is spelled, so nothing here distinguishes "absent" from "empty" —
/// [`optional`] does that where it matters.
#[derive(Debug, Clone)]
struct SeriesRow {
    key: ArrayKey,
    id: i64,
    owner_id: Option<i64>,
    owner_type: String,
    owner_category: String,
    time_series_type: String,
    name: String,
    resolution: String,
    interval: String,
    horizon: String,
    features: String,
    element_type: String,
    time_reference: String,
    units: String,
    quantity_kind: String,
    unit_system: String,
    component_field: String,
    application_data: String,
}

impl SeriesRow {
    /// The row a foreign file implies: nothing at all, so every resolver falls
    /// through to the inline options and the inference rules.
    fn bare(key: ArrayKey) -> Self {
        Self {
            key,
            id: 0,
            owner_id: None,
            owner_type: String::new(),
            owner_category: String::new(),
            time_series_type: String::new(),
            name: String::new(),
            resolution: String::new(),
            interval: String::new(),
            horizon: String::new(),
            features: String::new(),
            element_type: String::new(),
            time_reference: String::new(),
            units: String::new(),
            quantity_kind: String::new(),
            unit_system: String::new(),
            component_field: String::new(),
            application_data: String::new(),
        }
    }
}

/// Streams a series file, yielding every row of one array key at a time.
struct SeriesReader {
    reader: ParquetRecordBatchReader,
    /// Kept rather than dropped after the version check: [`check_pair`] compares
    /// it against the values half's.
    footer: BTreeMap<String, String>,
    open: Vec<SeriesRow>,
    ready: VecDeque<Vec<SeriesRow>>,
    seen: HashSet<ArrayKey>,
    done: bool,
}

impl SeriesReader {
    fn open(path: &Path) -> Result<Self> {
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
                    for row in read_series_rows(&batch)? {
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
fn read_series_rows(batch: &RecordBatch) -> Result<Vec<SeriesRow>> {
    let hashes = required_text(batch, DATA_HASH)?;
    let axes = required_text(batch, TIME_AXIS)?;
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

    Ok((0..batch.num_rows())
        .map(|i| SeriesRow {
            key: ArrayKey {
                data_hash: hashes[i].clone(),
                time_axis: axes[i].clone(),
            },
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
        .collect())
}

/// A text column a series file cannot be without: half of the array key.
fn required_text(batch: &RecordBatch, name: &str) -> Result<Vec<String>> {
    text_column(batch, name)?.ok_or_else(|| {
        unsupported(format!(
            "the series file has no `{name}` column, which is half the array key every row is \
             filed under"
        ))
    })
}

/// Assemble one series from its values group and its catalog row.
fn build(
    group: &ValuesGroup,
    row: &SeriesRow,
    footer: &BTreeMap<String, String>,
    options: &ImportOptions,
) -> Result<ImportedSeries> {
    let array = group.array()?;
    let ts_type = resolve_type(row, &group.timestamps, footer, options)?;
    let element_type = resolve_element_type(row, &array, footer, options)?;
    let reference = resolve_reference(row, footer, options, group.zone.as_deref())?;
    let name = resolve_name(row, options)?;
    let owner_id = resolve_owner_id(row, &name, options)?;
    let owner_type = resolve_owner_type(row, &name, options)?;
    check_element_shape(&group.dims, options)?;
    let resolution = resolve_resolution(row, &name, options)?;

    let rows = Rows {
        timestamps: group.timestamps.clone(),
        issue: group.issue.clone(),
        lanes: group.lanes.clone(),
    };
    let mut data = assemble(ts_type, row, resolution, rows, array, name.clone())?;
    let descriptors = infrastore_core::Descriptors {
        element_type,
        units: override_text(options.units.as_deref(), &row.units),
        quantity_kind: override_text(options.quantity_kind.as_deref(), &row.quantity_kind),
        unit_system: match options.unit_system {
            Some(system) => Some(system),
            None => optional(&row.unit_system)
                .map(|text| schema::decode_unit_system(&text).map_err(unsupported))
                .transpose()?,
        },
        time_reference: reference,
        component_field: override_text(options.component_field.as_deref(), &row.component_field),
        application_data: override_text(options.application_data.as_deref(), &row.application_data),
    };
    // Before canonicalizing: the constructor resolved the element type from the
    // array's dtype, and only the declared one says whether these rows are a
    // curve or a dense block of doubles.
    data.set_descriptors(descriptors);

    // A composite row was padded to the partition's width; shrink it back to the
    // width its own points need. Unconditional, not part of the checksum: the
    // narrower array is the right one to store whether or not there is a hash to
    // compare it against.
    canonicalize(&mut data)?;
    verify_checksum(&group.key, &data, &name, options)?;

    Ok(ImportedSeries {
        recorded_id: (row.id != 0).then_some(row.id),
        array: (!row.key.data_hash.is_empty()).then(|| row.key.clone()),
        owner_id,
        owner_type,
        owner_category: match &options.owner_category {
            Some(c) => *c,
            None if row.owner_category.is_empty() => OwnerCategory::Component,
            None => schema::decode_owner_category(&row.owner_category).map_err(unsupported)?,
        },
        features: match &options.features {
            Some(f) => f.clone(),
            None if row.features.is_empty() => Features::new(),
            None => schema::decode_features(&row.features).map_err(unsupported)?,
        },
        data,
    })
}

/// The series' name: the option, then the column.
///
/// A name is part of a series' `KeyIdentity`, so there is nothing sensible to
/// default it to — an empty one would file every nameless series in a foreign
/// file under the same identity.
fn resolve_name(row: &SeriesRow, options: &ImportOptions) -> Result<String> {
    if let Some(name) = &options.name {
        return Ok(name.clone());
    }
    if row.name.is_empty() {
        return Err(unsupported(
            "the file records no series name and none was given; a name is part of a series' \
             identity, so pass --name",
        ));
    }
    Ok(row.name.clone())
}

/// The owning component's id: the option, then the column.
///
/// Refused rather than defaulted for the same reason as the name. A foreign file
/// has no `owner_id` column, and owner 0 is a real owner rather than a sentinel,
/// so silently choosing it would file the series somewhere the caller never
/// named.
fn resolve_owner_id(row: &SeriesRow, name: &str, options: &ImportOptions) -> Result<i64> {
    options
        .owner_id
        .or(row.owner_id)
        .ok_or_else(|| missing_owner(name, "owner_id", "--owner-id"))
}

/// The owner's type name, on the same rule: a series owned by `""` is not
/// something any consumer means.
fn resolve_owner_type(row: &SeriesRow, name: &str, options: &ImportOptions) -> Result<String> {
    if let Some(owner_type) = &options.owner_type {
        return Ok(owner_type.clone());
    }
    if row.owner_type.is_empty() {
        return Err(missing_owner(name, "owner_type", "--owner-type"));
    }
    Ok(row.owner_type.clone())
}

/// The type to file the group under: the column, then the footer, then the
/// option, then inference.
///
/// A grid reads as `SingleTimeSeries` and anything else as
/// `NonSequentialTimeSeries`. `PersistentTimeSeries` is **never inferred**: it is
/// structurally identical to the irregular type and differs only in what the
/// values mean between rows, so guessing it would be guessing that.
fn resolve_type(
    row: &SeriesRow,
    timestamps: &[DateTime<Utc>],
    footer: &BTreeMap<String, String>,
    options: &ImportOptions,
) -> Result<TimeSeriesType> {
    let declared = if row.time_series_type.is_empty() {
        footer
            .get(schema::TIME_SERIES_TYPE)
            .filter(|t| !t.is_empty())
            .map(|t| schema::decode_time_series_type(t).map_err(unsupported))
            .transpose()?
    } else {
        Some(schema::decode_time_series_type(&row.time_series_type).map_err(unsupported)?)
    };
    if let Some(declared) = declared {
        if declared == TimeSeriesType::DeterministicSingleTimeSeries {
            return Err(unsupported(
                "a DeterministicSingleTimeSeries is derived from a stored SingleTimeSeries \
                 rather than added; import the SingleTimeSeries and run \
                 `transform_single_time_series`",
            ));
        }
        if let Some(asserted) = options.time_series_type
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
    if let Some(asserted) = options.time_series_type {
        return Ok(asserted);
    }
    Ok(if Period::infer(timestamps).is_ok() {
        TimeSeriesType::SingleTimeSeries
    } else {
        TimeSeriesType::NonSequentialTimeSeries
    })
}

/// The logical element type. The file's wins; a contradicting option is an
/// error. Without one, the Arrow type alone gives the **weaker** reading — a
/// `FixedSizeList<double>[3]` is dense `f64` with shape `[3]`, not
/// `tuple(3,f64)`, because the bytes cannot say and dense assumes less.
fn resolve_element_type(
    row: &SeriesRow,
    array: &TypedArray,
    footer: &BTreeMap<String, String>,
    options: &ImportOptions,
) -> Result<ElementType> {
    let declared = if row.element_type.is_empty() {
        footer
            .get(schema::ELEMENT_TYPE)
            .filter(|t| !t.is_empty())
            .map(|t| schema::decode_element_type(t).map_err(unsupported))
            .transpose()?
    } else {
        Some(schema::decode_element_type(&row.element_type).map_err(unsupported)?)
    };
    match (declared, options.element_type) {
        (Some(from_file), Some(asserted)) if from_file != asserted => Err(unsupported(format!(
            "the file declares element_type {from_file}, but {asserted} was asserted"
        ))),
        (Some(from_file), _) => Ok(from_file),
        (None, Some(asserted)) => Ok(asserted),
        (None, None) => Ok(ElementType::Scalar(array.dtype)),
    }
}

/// The timestamp spelling: the option, then the column, then the footer, then
/// the timestamp column's own Arrow zone.
///
/// The literal `unspecified` decodes to *no* reference and beats the zone, which
/// is what keeps a round trip from inventing a `utc` the series never claimed —
/// see Finding 7.12.
fn resolve_reference(
    row: &SeriesRow,
    footer: &BTreeMap<String, String>,
    options: &ImportOptions,
    zone: Option<&str>,
) -> Result<Option<TimeReference>> {
    if let Some(reference) = &options.time_reference {
        return Ok(Some(reference.clone()));
    }
    let text = if row.time_reference.is_empty() {
        footer
            .get(schema::TIME_REFERENCE)
            .filter(|t| !t.is_empty())
            .cloned()
    } else {
        Some(row.time_reference.clone())
    };
    match text {
        Some(text) => schema::decode_time_reference(&text).map_err(unsupported),
        // A foreign file: the column's zone is all there is, and it does say
        // something — a naive column is a wall clock, a zoned one names its
        // spelling. Only the literal `unspecified` means *none*.
        None => reference_from_arrow_zone(zone).map(Some),
    }
}

/// A free-form descriptor: the option when there is one, else the column.
///
/// Either way the empty string is *absent*, which is what this format writes for
/// an unset descriptor — so `--units ''` clears one rather than storing a series
/// whose units are the empty string.
fn override_text(option: Option<&str>, column: &str) -> Option<String> {
    optional(option.unwrap_or(column))
}

/// Check an asserted `--element-shape` against the shape the `value` column
/// carries.
///
/// An assertion rather than an override because the column's nesting states it
/// exactly: there is no reading here the bytes cannot give, only a claim to
/// agree or disagree with. It earns its place on a foreign file, where it says
/// out loud what a `FixedSizeList<double>[3]` was meant to be.
fn check_element_shape(dims: &[usize], options: &ImportOptions) -> Result<()> {
    let Some(asserted) = &options.element_shape else {
        return Ok(());
    };
    if asserted != dims {
        return Err(unsupported(format!(
            "the `{VALUE}` column carries a per-step shape of {dims:?}, but {asserted:?} was \
             asserted"
        )));
    }
    Ok(())
}

/// The grid step: the column, checked against an assertion, or the assertion
/// alone when the file records none.
///
/// A foreign file's rows imply a step but do not state one, so `--resolution`
/// there *names* the grid — and [`single`] then checks the rows really walk it,
/// against the grid that resolution generates rather than against successive
/// differences.
fn resolve_resolution(
    row: &SeriesRow,
    name: &str,
    options: &ImportOptions,
) -> Result<Option<Period>> {
    let recorded = if row.resolution.is_empty() {
        None
    } else {
        Some(
            Period::from_iso8601(&row.resolution)
                .map_err(|e| unsupported(format!("resolution: {e}")))?,
        )
    };
    match (recorded, options.resolution) {
        (Some(from_file), Some(asserted)) if from_file != asserted => Err(unsupported(format!(
            "series '{name}' records a resolution of {}, but {} was asserted",
            from_file.to_iso8601(),
            asserted.to_iso8601()
        ))),
        (Some(from_file), _) => Ok(Some(from_file)),
        (None, asserted) => Ok(asserted),
    }
}

/// Compare the recorded `data_hash` against the group as re-encoded.
///
/// The key doubles as a checksum, which matters more here than it would in one
/// denormalized table: a values file is the shape people open in DuckDB, and
/// DuckDB makes it easy to write one back. A mismatch names the key;
/// `--no-checksum` waives the comparison and leaves the pair as nothing but a
/// join key.
fn verify_checksum(
    key: &ArrayKey,
    data: &TimeSeriesData,
    name: &str,
    options: &ImportOptions,
) -> Result<()> {
    if options.skip_checksum || key.data_hash.is_empty() {
        return Ok(());
    }
    let (array, leading) = cube_of(data);
    let actual = table::canonical_hash(array, data.element_type(), &leading)?;
    if actual != key.data_hash {
        return Err(unsupported(format!(
            "series '{name}' does not match its recorded {DATA_HASH}: the file says {} and its \
             rows hash to {actual}. Values edited in a query engine need the hash recomputed, \
             or --no-checksum to waive the comparison.",
            key.data_hash
        )));
    }
    Ok(())
}

fn assemble(
    ts_type: TimeSeriesType,
    row: &SeriesRow,
    resolution: Option<Period>,
    rows: Rows,
    array: TypedArray,
    name: String,
) -> Result<TimeSeriesData> {
    if ts_type.is_forecast() {
        return forecast(ts_type, row, resolution, rows, array, name);
    }
    let Rows { timestamps, .. } = rows;
    match ts_type {
        TimeSeriesType::SingleTimeSeries => Ok(TimeSeriesData::SingleTimeSeries(single(
            timestamps, array, &name, resolution,
        )?)),
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

/// Rebuild a forecast's cube from its values group.
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
    row: &SeriesRow,
    resolution: Option<Period>,
    rows: Rows,
    array: TypedArray,
    name: String,
) -> Result<TimeSeriesData> {
    let resolution = resolution.ok_or_else(|| missing_grid("resolution", &name))?;
    let interval = required_period(&row.interval, "interval", &name)?;
    let horizon = required_period(&row.horizon, "horizon", &name)?;
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
        return Err(missing_grid(column, name));
    }
    Period::from_iso8601(text).map_err(|e| unsupported(format!("{column}: {e}")))
}

fn missing_grid(column: &str, name: &str) -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "series '{name}' is a forecast and needs a `{column}`: the rows say where each value \
         belongs, not what the grid it belongs to is"
    ))
}

/// The stored cube and its leading axes, for hashing.
fn cube_of(data: &TimeSeriesData) -> (&TypedArray, Vec<usize>) {
    let array = data.array();
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
