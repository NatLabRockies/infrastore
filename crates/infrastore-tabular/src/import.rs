//! Turning a partition's rows back into series: the half of the import that
//! does not care what container the rows came out of.
//!
//! A reader (Parquet files, SQLite tables) produces [`ValuesGroup`]s and
//! [`SeriesRow`]s in array-key order and hands them to [`merge_join`], which
//! pairs them up and [`build`]s each series.
//!
//! Either dangling side is an error. A series row whose key names no values
//! group has no array to read; a values group no series row claims is an array
//! nothing would file. Both mean the halves came from different exports, or one
//! was truncated, and neither is something to guess about.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use infrastore_core::{
    Dtype, ElementType, Features, NonSequentialTimeSeries, OwnerCategory, Period,
    PersistentTimeSeries, SingleTimeSeries, TimeReference, TimeSeriesData, TimeSeriesType,
    TypedArray, UnitSystem,
};

use crate::layout::{self, ARRAY_ID, ArrayKey, DATA_HASH, ISSUE_TIME, PERCENTILE, SCENARIO, VALUE};
use crate::schema;
use crate::{Result, unsupported};

/// Walk two key-ordered streams together, filing every series row against the
/// values group carrying its key.
///
/// `footer` is the partition-level fallback for a column a row leaves empty;
/// a container with nowhere to put one passes an empty map.
pub fn merge_join(
    mut next_values: impl FnMut() -> Result<Option<ValuesGroup>>,
    mut next_rows: impl FnMut() -> Result<Option<Vec<SeriesRow>>>,
    footer: &BTreeMap<String, String>,
    options: &ImportOptions,
    sink: SeriesSink<'_>,
) -> Result<usize> {
    let mut filed = 0usize;
    let mut group = next_values()?;
    let mut batch = next_rows()?;
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
            sink(build(&held, row, footer, options)?)?;
            filed += 1;
        }
        group = next_values()?;
        batch = next_rows()?;
    }
}

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

/// A place each completed series goes as soon as its rows are read.
///
/// The whole point of the layout is that a partition can be far larger than
/// memory, so the reader never holds more than the array it is in the middle of.
/// A caller filing a partition in one transaction opens the transaction, hands
/// this a closure that adds each series, and commits when the call returns.
pub type SeriesSink<'a> = &'a mut dyn FnMut(ImportedSeries) -> Result<()>;

/// A values group no series row claims.
fn unclaimed(key: &ArrayKey) -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "the values file holds an array ({}) that no series row names; a partition's values \
         and series come from one export and must describe the same arrays",
        short(key)
    ))
}

/// A series row whose key names no values group.
fn unbacked(key: &ArrayKey) -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "a series row names an array ({}) the values file does not hold; a partition's values \
         and series come from one export and must describe the same arrays",
        short(key)
    ))
}

/// A key that comes back after another key's rows.
pub fn reappeared(key: &ArrayKey, half: &str) -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "the {half} file returns to array ({}) after another array's rows; both halves must be \
         sorted by `{ARRAY_ID}`",
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

/// One array's rows, as the values file holds them.
pub struct ValuesGroup {
    pub key: ArrayKey,
    pub timestamps: Vec<DateTime<Utc>>,
    /// Empty for a static partition.
    pub issue: Vec<DateTime<Utc>>,
    /// Empty for a static partition and for `Deterministic`.
    pub lanes: Vec<LaneValue>,
    /// The per-step shape, from the `value` column's nesting.
    pub dims: Vec<usize>,
    pub dtype: Dtype,
    pub bytes: Vec<u8>,
    /// The `timestamp` column's own Arrow zone, the last resort for a foreign
    /// file that names no `time_reference` anywhere.
    pub zone: Option<String>,
}

impl ValuesGroup {
    pub fn array(&self) -> Result<TypedArray> {
        let mut shape = vec![self.timestamps.len()];
        shape.extend_from_slice(&self.dims);
        TypedArray::new(self.dtype, shape, self.bytes.clone()).map_err(unsupported)
    }
}

/// One catalog row out of a series file.
///
/// Every field is the column's text as written; the empty string is how an
/// absent one is spelled, so nothing here distinguishes "absent" from "empty" —
/// [`optional`] does that where it matters.
#[derive(Debug, Clone)]
pub struct SeriesRow {
    pub key: ArrayKey,
    pub id: i64,
    pub owner_id: Option<i64>,
    pub owner_type: String,
    pub owner_category: String,
    pub time_series_type: String,
    pub name: String,
    pub resolution: String,
    pub interval: String,
    pub horizon: String,
    pub features: String,
    pub element_type: String,
    pub time_reference: String,
    pub units: String,
    pub quantity_kind: String,
    pub unit_system: String,
    pub component_field: String,
    pub application_data: String,
}

impl SeriesRow {
    /// The row a foreign file implies: nothing at all, so every resolver falls
    /// through to the inline options and the inference rules.
    pub fn bare(key: ArrayKey) -> Self {
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

/// Assemble one series from its values group and its catalog row.
pub fn build(
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
                .map(|text| {
                    UnitSystem::parse(&text).ok_or_else(|| {
                        unsupported(format!("unknown {} {text:?}", schema::UNIT_SYSTEM))
                    })
                })
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
            None => OwnerCategory::parse(&row.owner_category).ok_or_else(|| {
                unsupported(format!(
                    "unknown {} {:?}",
                    schema::OWNER_CATEGORY,
                    row.owner_category
                ))
            })?,
        },
        features: match &options.features {
            Some(f) => f.clone(),
            None if row.features.is_empty() => Features::new(),
            // The plain `{"model_year":2030}` form, each value's kind inferred
            // from its JSON type — the inverse of `schema::encode_features`.
            None => {
                let value: serde_json::Value = serde_json::from_str(&row.features)
                    .map_err(|e| unsupported(format!("{} is not JSON: {e}", schema::FEATURES)))?;
                let object = value.as_object().ok_or_else(|| {
                    unsupported(format!("{} must be a JSON object", schema::FEATURES))
                })?;
                infrastore_core::features_from_plain(object)
                    .map_err(|e| unsupported(e.to_string()))?
            }
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
        // Unspecified is the absence of a reference, so it is decoded here
        // rather than in `TimeReference::parse`, which must keep refusing the
        // literal -- see `schema::UNSPECIFIED_REFERENCE`.
        Some(text) if text == schema::UNSPECIFIED_REFERENCE => Ok(None),
        Some(text) => TimeReference::parse(&text)
            .map(Some)
            .map_err(|e| unsupported(format!("{} {text:?}: {e}", schema::TIME_REFERENCE))),
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
    let actual = layout::canonical_hash(array, data.element_type(), &leading)?;
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
        "series '{name}' has no {column}: an export carries one, a foreign \
         file does not, so pass {flag}"
    ))
}

/// An empty string is how an absent free-form descriptor is written, so that
/// every column can be required. The one documented consequence: a stored empty
/// string reads back as absent.
fn optional(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
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
pub enum LaneValue {
    Percentile(f64),
    Scenario(i64),
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
    if !layout::is_composite(element_type) {
        return Ok(());
    }
    let (array, leading) = cube_of(data);
    let shrunk = layout::canonical_array(array, element_type, &leading)?;
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

/// An `array_id` no row of the arrays table carries.
pub fn dangling(id: i64, half: &str) -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "a {half} row names `{ARRAY_ID}` {id}, which the arrays table does not hold; a \
         partition's tables come from one export"
    ))
}
