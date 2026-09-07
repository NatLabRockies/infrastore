//! Reading a Parquet file back into a time series.
//!
//! Two files reach this: one this crate wrote, whose footer describes the whole
//! catalog row, and a **foreign** one written by anything else, which may carry
//! no footer at all. The rules are the same for both — the footer is consulted
//! where it exists and inferred from the Arrow schema where it does not — so
//! there is one import rather than a strict path and a lenient one.
//!
//! What is *not* read back is the row's `id`. No `add_*` in this project accepts
//! one, because "never reissued" is a guarantee of the catalog's
//! `AUTOINCREMENT`, and a caller free to name an id could re-file a retired one.
//! The value is surfaced in [`ImportedSeries::recorded_id`] so a `--dry-run` can
//! say which row the file came from, and then ignored.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

use arrow::array::{
    Array, ArrayRef, BooleanArray, FixedSizeListArray, Float32Array, Float64Array, Int8Array,
    Int16Array, Int32Array, Int64Array, RecordBatch, TimestampMicrosecondArray,
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

use crate::export::{TIMESTAMP_COLUMN, VALUE_COLUMN};
use crate::schema;
use crate::{Result, arrow_err, parquet_err, unsupported};

/// One series read out of a Parquet file, plus whatever the footer said about
/// the catalog row it came from.
///
/// The owner fields are `Option` because a file written by Python's
/// `to_arrow()` has none: that is a method on a value object, and a
/// `SingleTimeSeries` built in Python is not filed anywhere. A caller supplies
/// them — from a descriptor, from flags — exactly as it does for a CSV.
#[derive(Debug, Clone)]
pub struct ImportedSeries {
    /// The catalog id the file recorded, for reporting only. Never used to file
    /// the row.
    pub recorded_id: Option<i64>,
    pub owner_id: Option<i64>,
    pub owner_type: Option<String>,
    pub owner_category: Option<OwnerCategory>,
    pub features: Features,
    pub data: TimeSeriesData,
}

/// What the caller wants asserted or supplied on top of what the file says.
///
/// The distinction between the two fields is the project's usual one. A
/// **declaration** fills in what the file does not say; an **assertion** states
/// what it does say, and a contradicting assertion is an error rather than an
/// override — the same rule `element_type=` follows in every binding.
#[derive(Debug, Default, Clone)]
pub struct ImportOptions {
    /// Which of the six types the rows describe. Declares when the footer is
    /// silent; asserts when it is not.
    pub time_series_type: Option<TimeSeriesType>,
    /// The logical element type. Declares when the footer is silent; asserts
    /// when it is not. This is how `tuple(3,f64)` is named for a foreign file:
    /// the bytes cannot say whether a `FixedSizeList<double>[3]` is a tuple or a
    /// dense row, so inference takes the weaker reading and this states the
    /// stronger one.
    pub element_type: Option<ElementType>,
    /// The name to file the series under. Declares when the footer is silent;
    /// **overrides** when it is not, because a name is a caller's choice rather
    /// than a fact about the values.
    pub name: Option<String>,
    /// The timestamp spelling. Declares when neither the footer nor the Arrow
    /// zone says; overrides both when given.
    pub time_reference: Option<TimeReference>,
}

/// Read one Parquet file.
pub fn read_series(path: &Path, options: &ImportOptions) -> Result<ImportedSeries> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(parquet_err)?;
    let schema = builder.schema().clone();
    let footer: BTreeMap<String, String> = schema
        .metadata()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let batches: Vec<RecordBatch> = builder
        .build()
        .map_err(parquet_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(arrow_err)?;
    // One batch, so every column offset is zero and the two columns are
    // guaranteed the same length by `RecordBatch` itself.
    let batch = arrow::compute::concat_batches(&schema, &batches).map_err(arrow_err)?;

    let zone = timestamp_zone(&batch)?;
    let timestamps = read_timestamps(&batch)?;
    let (dims, leaf) = descend(column(&batch, VALUE_COLUMN)?)?;
    let array = build_array(&dims, &leaf, timestamps.len())?;

    build_series(timestamps, array, &footer, options, zone.as_deref())
}

/// The timestamp column's own Arrow zone, if it has one.
fn timestamp_zone(batch: &RecordBatch) -> Result<Option<String>> {
    match column(batch, TIMESTAMP_COLUMN)?.data_type() {
        DataType::Timestamp(_, zone) => Ok(zone.as_ref().map(|z| z.to_string())),
        _ => Ok(None),
    }
}

/// The named column, or a message saying what this import expects to find.
fn column(batch: &RecordBatch, name: &str) -> Result<ArrayRef> {
    batch
        .column_by_name(name)
        .cloned()
        .ok_or_else(|| unsupported(format!("the file has no `{name}` column")))
}

/// The timestamp column as instants.
///
/// Seconds and milliseconds cross as they are. Microseconds and nanoseconds are
/// accepted **only when every value is a whole millisecond**, which is the same
/// rule the store's write path enforces on every instant it records: unix
/// milliseconds are the store's precision, and silently rounding a finer
/// timestamp would move it.
fn read_timestamps(batch: &RecordBatch) -> Result<Vec<DateTime<Utc>>> {
    let array = column(batch, TIMESTAMP_COLUMN)?;
    if array.null_count() > 0 {
        return Err(unsupported(format!(
            "the `{TIMESTAMP_COLUMN}` column has nulls; the store records an \
             instant for every row"
        )));
    }
    let millis: Vec<i64> = match array.data_type() {
        DataType::Timestamp(TimeUnit::Second, _) => {
            let a = downcast::<TimestampSecondArray>(&array, "timestamp[s]")?;
            (0..a.len())
                .map(|i| {
                    a.value(i)
                        .checked_mul(1_000)
                        .ok_or_else(|| unsupported("a timestamp in seconds overflows milliseconds"))
                })
                .collect::<Result<_>>()?
        }
        DataType::Timestamp(TimeUnit::Millisecond, _) => {
            let a = downcast::<TimestampMillisecondArray>(&array, "timestamp[ms]")?;
            (0..a.len()).map(|i| a.value(i)).collect()
        }
        DataType::Timestamp(TimeUnit::Microsecond, _) => {
            let a = downcast::<TimestampMicrosecondArray>(&array, "timestamp[us]")?;
            rescale((0..a.len()).map(|i| a.value(i)), 1_000, "microsecond")?
        }
        DataType::Timestamp(TimeUnit::Nanosecond, _) => {
            let a = downcast::<TimestampNanosecondArray>(&array, "timestamp[ns]")?;
            rescale((0..a.len()).map(|i| a.value(i)), 1_000_000, "nanosecond")?
        }
        other => {
            return Err(unsupported(format!(
                "the `{TIMESTAMP_COLUMN}` column is {other}, not a timestamp"
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

/// Divide sub-millisecond timestamps down, refusing any that is not a whole
/// millisecond.
fn rescale(values: impl Iterator<Item = i64>, per_milli: i64, unit: &str) -> Result<Vec<i64>> {
    values
        .map(|v| {
            if v % per_milli == 0 {
                Ok(v / per_milli)
            } else {
                Err(unsupported(format!(
                    "the `{TIMESTAMP_COLUMN}` column is in {unit}s and {v} is not a whole \
                     millisecond; the store records millisecond instants and will not round one"
                )))
            }
        })
        .collect()
}

fn downcast<'a, T: 'static>(array: &'a ArrayRef, what: &str) -> Result<&'a T> {
    array
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| unsupported(format!("expected a {what} column")))
}

/// Peel the `FixedSizeList` levels off the value column, returning the per-step
/// dims (outermost first) and the flat leaf.
///
/// `Struct` and `List` are refused: those are the *decoded* form of a composite
/// element type, which this version deliberately does not produce and therefore
/// does not claim to read. A `List` is refused for a second reason too — it is
/// ragged, and a time series' per-step shape is not.
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
                    "the `{VALUE_COLUMN}` column is a variable-length list; a time series' \
                     per-step shape is fixed, so this import reads fixed-size lists only"
                )));
            }
            DataType::Struct(_) => {
                return Err(unsupported(format!(
                    "the `{VALUE_COLUMN}` column is a struct; this version reads the packed \
                     form composite element types are stored in, not a decoded one"
                )));
            }
            _ => return Ok((dims, array)),
        }
    }
}

fn nulls_refused() -> infrastore_core::TimeSeriesError {
    unsupported(format!(
        "the `{VALUE_COLUMN}` column has nulls; the store holds no nulls, and NaN is a value \
         rather than an absence, so this is refused rather than coerced"
    ))
}

/// The leaf primitives as a `TypedArray` shaped `[rows, *dims]`.
fn build_array(dims: &[usize], leaf: &ArrayRef, rows: usize) -> Result<TypedArray> {
    if leaf.null_count() > 0 {
        return Err(nulls_refused());
    }
    let mut shape = vec![rows];
    shape.extend_from_slice(dims);

    /// Read every element through `value(i)` rather than the raw buffer: a
    /// concatenated or sliced array carries an offset, and `value` accounts for
    /// it whatever the arrow version does with `values()`.
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
                "the `{VALUE_COLUMN}` column is {other}, which is not one of the store's dtypes"
            )));
        }
    })
}

/// Assemble the series the footer and options between them describe.
fn build_series(
    timestamps: Vec<DateTime<Utc>>,
    array: TypedArray,
    footer: &BTreeMap<String, String>,
    options: &ImportOptions,
    zone: Option<&str>,
) -> Result<ImportedSeries> {
    let ts_type = resolve_time_series_type(&timestamps, footer, options)?;
    let element_type = resolve_element_type(&array, footer, options)?;
    let name = resolve_name(footer, options)?;
    let reference = resolve_time_reference(footer, options, zone)?;
    let resolution = footer
        .get(schema::RESOLUTION)
        .map(|iso| Period::from_iso8601(iso).map_err(|e| unsupported(format!("resolution: {e}"))))
        .transpose()?;

    let data = match ts_type {
        TimeSeriesType::SingleTimeSeries => {
            TimeSeriesData::SingleTimeSeries(single(timestamps, array, &name, resolution)?)
        }
        TimeSeriesType::NonSequentialTimeSeries => TimeSeriesData::NonSequentialTimeSeries(
            NonSequentialTimeSeries::new(timestamps, array, name.clone()).map_err(unsupported)?,
        ),
        TimeSeriesType::PersistentTimeSeries => TimeSeriesData::PersistentTimeSeries(
            PersistentTimeSeries::new(timestamps, array, name.clone()).map_err(unsupported)?,
        ),
        other => {
            return Err(unsupported(format!(
                "Parquet import covers the static types; {} is a forecast",
                other.as_str()
            )));
        }
    };
    let mut data = data;
    apply_descriptors(&mut data, element_type, reference, footer)?;

    Ok(ImportedSeries {
        recorded_id: footer
            .get(schema::ID)
            .and_then(|text| text.parse::<i64>().ok()),
        owner_id: footer
            .get(schema::OWNER_ID)
            .map(|text| {
                text.parse::<i64>()
                    .map_err(|e| unsupported(format!("{}: {e}", schema::OWNER_ID)))
            })
            .transpose()?,
        owner_type: footer.get(schema::OWNER_TYPE).cloned(),
        owner_category: footer
            .get(schema::OWNER_CATEGORY)
            .map(|text| schema::decode_owner_category(text).map_err(unsupported))
            .transpose()?,
        features: footer
            .get(schema::FEATURES)
            .map(|text| schema::decode_features(text).map_err(unsupported))
            .transpose()?
            .unwrap_or_default(),
        data,
    })
}

/// Build the regular series, checking that its rows really sit on a grid.
///
/// With a resolution from the footer, the timestamps are checked against the
/// grid that resolution generates — which is not the same as checking successive
/// differences, because `Period::Months` clamps to month end. Without one,
/// `Period::infer` derives the step and fails if there is none.
fn single(
    timestamps: Vec<DateTime<Utc>>,
    array: TypedArray,
    name: &str,
    resolution: Option<Period>,
) -> Result<SingleTimeSeries> {
    let Some(&first) = timestamps.first() else {
        return Err(unsupported(
            "a SingleTimeSeries is anchored at its first timestamp, and this file has no rows; \
             give the anchor another way, or import it as a NonSequentialTimeSeries",
        ));
    };
    let Some(resolution) = resolution else {
        return SingleTimeSeries::from_timestamps(&timestamps, array, name).map_err(unsupported);
    };
    let series = SingleTimeSeries::new(first, resolution, array, name);
    let grid: Vec<DateTime<Utc>> = series.timestamps().collect();
    if grid != timestamps {
        let at = grid
            .iter()
            .zip(&timestamps)
            .position(|(a, b)| a != b)
            .unwrap_or(grid.len().min(timestamps.len()));
        return Err(unsupported(format!(
            "the timestamps do not sit on a {} grid anchored at {first}: row {at} is not where \
             the grid puts it",
            resolution.to_iso8601()
        )));
    }
    Ok(series)
}

/// The type to file the rows under.
///
/// A footer names it. Without one, a grid reads as `SingleTimeSeries` and
/// anything else as `NonSequentialTimeSeries` — the same shape of inference
/// `add` already does when it detects a CSV's layout from its header.
/// `PersistentTimeSeries` is **never inferred**: it is structurally identical to
/// `NonSequentialTimeSeries` and differs only in read semantics, so guessing it
/// would be guessing what the values mean.
fn resolve_time_series_type(
    timestamps: &[DateTime<Utc>],
    footer: &BTreeMap<String, String>,
    options: &ImportOptions,
) -> Result<TimeSeriesType> {
    let declared = footer
        .get(schema::TIME_SERIES_TYPE)
        .map(|text| schema::decode_time_series_type(text).map_err(unsupported))
        .transpose()?;
    match (declared, options.time_series_type) {
        (Some(from_file), Some(asserted)) if from_file != asserted => Err(unsupported(format!(
            "the file declares {}, but {} was asserted",
            from_file.as_str(),
            asserted.as_str()
        ))),
        (Some(from_file), _) => Ok(from_file),
        (None, Some(asserted)) => Ok(asserted),
        (None, None) => Ok(if Period::infer(timestamps).is_ok() {
            TimeSeriesType::SingleTimeSeries
        } else {
            TimeSeriesType::NonSequentialTimeSeries
        }),
    }
}

/// The logical element type.
///
/// The footer's wins and a contradicting option is an error. Without a footer
/// the type is inferred from the Arrow type alone, which gives the **weaker**
/// reading: a `FixedSizeList<double>[3]` becomes a scalar `f64` with element
/// shape `[3]`, not `tuple(3,f64)`, because the bytes cannot tell the two apart
/// and dense is the claim that assumes less. `element_type` on the option states
/// the stronger one.
fn resolve_element_type(
    array: &TypedArray,
    footer: &BTreeMap<String, String>,
    options: &ImportOptions,
) -> Result<ElementType> {
    let declared = footer
        .get(schema::ELEMENT_TYPE)
        .map(|text| schema::decode_element_type(text).map_err(unsupported))
        .transpose()?;
    match (declared, options.element_type) {
        (Some(from_file), Some(asserted)) if from_file != asserted => Err(unsupported(format!(
            "the file declares element_type {from_file}, but {asserted} was asserted"
        ))),
        (Some(from_file), _) => Ok(from_file),
        (None, Some(asserted)) => Ok(asserted),
        (None, None) => Ok(ElementType::Scalar(array.dtype)),
    }
}

fn resolve_name(footer: &BTreeMap<String, String>, options: &ImportOptions) -> Result<String> {
    options
        .name
        .clone()
        .or_else(|| footer.get(schema::NAME).cloned())
        .ok_or_else(|| {
            unsupported(
                "the file records no series name and none was given; a name is part of a \
                 series' identity",
            )
        })
}

/// The timestamp spelling.
///
/// The option wins, then the footer, then the Arrow zone. The zone is last
/// because it cannot express everything the store records: a column with no zone
/// is what both `zoneless` and *unspecified* would produce, which is why the
/// footer spells it out. For a foreign file with no footer, a zoneless column
/// reads as `Zoneless` — a naive timestamp is a wall clock, which is the same
/// reading Python and the CLI take.
fn resolve_time_reference(
    footer: &BTreeMap<String, String>,
    options: &ImportOptions,
    zone: Option<&str>,
) -> Result<Option<TimeReference>> {
    if let Some(reference) = &options.time_reference {
        return Ok(Some(reference.clone()));
    }
    if let Some(text) = footer.get(schema::TIME_REFERENCE) {
        return Ok(Some(
            schema::decode_time_reference(text).map_err(unsupported)?,
        ));
    }
    reference_from_arrow_zone(zone).map(Some)
}

/// The spelling the timestamp column's own Arrow zone implies, for a file whose
/// footer is silent.
pub fn reference_from_arrow_zone(zone: Option<&str>) -> Result<TimeReference> {
    match zone {
        None => Ok(TimeReference::Zoneless),
        Some(z) if z.eq_ignore_ascii_case("UTC") => Ok(TimeReference::Utc),
        Some(z) => TimeReference::parse(z)
            .map_err(|e| unsupported(format!("the timestamp column's zone {z:?}: {e}"))),
    }
}

/// Write the descriptors onto the series the enum holds.
fn apply_descriptors(
    data: &mut TimeSeriesData,
    element_type: ElementType,
    reference: Option<TimeReference>,
    footer: &BTreeMap<String, String>,
) -> Result<()> {
    let unit_system = footer
        .get(schema::UNIT_SYSTEM)
        .map(|text| schema::decode_unit_system(text).map_err(unsupported))
        .transpose()?;
    let descriptors = infrastore_core::Descriptors {
        element_type,
        units: footer.get(schema::UNITS).cloned(),
        quantity_kind: footer.get(schema::QUANTITY_KIND).cloned(),
        unit_system,
        time_reference: reference,
        component_field: footer.get(schema::COMPONENT_FIELD).cloned(),
        application_data: footer.get(schema::APPLICATION_DATA).cloned(),
    };
    data.set_descriptors(descriptors);
    Ok(())
}

/// The physical dtype the store would give a leaf of this Arrow type, for a
/// caller reporting what a file holds without reading it.
pub fn dtype_of(data_type: &DataType) -> Option<Dtype> {
    Some(match data_type {
        DataType::Float64 => Dtype::F64,
        DataType::Float32 => Dtype::F32,
        DataType::Int64 => Dtype::I64,
        DataType::Int32 => Dtype::I32,
        DataType::Int16 => Dtype::I16,
        DataType::Int8 => Dtype::I8,
        DataType::UInt64 => Dtype::U64,
        DataType::UInt32 => Dtype::U32,
        DataType::UInt16 => Dtype::U16,
        DataType::UInt8 => Dtype::U8,
        DataType::Boolean => Dtype::Bool,
        _ => return None,
    })
}
