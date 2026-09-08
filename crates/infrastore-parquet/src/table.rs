//! A partition's two schemas, its footer, and the array key that joins them.
//!
//! A partition is a **values** file holding every distinct array once, one row
//! per value, and a **series** file holding one catalog row per series naming the
//! array it reads. Both are keyed by `(data_hash, time_axis)` and sorted by it,
//! which is what lets the import walk them as a merge join.
//!
//! Every column in both is **required**, which is what the partitioning in
//! [`crate::partition`] buys.

use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use chrono::{DateTime, SecondsFormat, Utc};
use infrastore_core::{
    ElementType, Period, TimeReference, TimeSeriesData, TimeSeriesType, TypedArray, array_hash,
    hash_hex, timestamps_hash,
};

use crate::partition::{PartitionKey, ValueKind};
use crate::schema;
use crate::{Result, unsupported};

/// Key columns.
pub const TIMESTAMP: &str = "timestamp";
/// A forecast's issue time: which window the row belongs to.
pub const ISSUE_TIME: &str = "issue_time";
/// `Probabilistic` only.
pub const PERCENTILE: &str = "percentile";
/// `Scenarios` only, zero-based.
pub const SCENARIO: &str = "scenario";
/// The value.
pub const VALUE: &str = "value";
/// Half the array key: the hex content hash of the array — see
/// [`canonical_hash`]. Also a checksum on import.
pub const DATA_HASH: &str = "data_hash";
/// The other half: what determines the timestamps a value row sits at, spelled
/// per type — see [`time_axis_of`].
pub const TIME_AXIS: &str = "time_axis";

/// Footer key marking a file as this format, and saying which version of it.
///
/// Read before anything else, so a file from a later build is refused by
/// version rather than by whichever column it happens to be missing.
pub const FORMAT: &str = "infrastore.format";
/// The value [`FORMAT`] carries.
pub const FORMAT_V1: &str = "normalized_v1";
/// Footer key saying which half of a partition a file is.
pub const ROLE: &str = "infrastore.role";
/// The values half.
pub const ROLE_VALUES: &str = "values";
/// The series half.
pub const ROLE_SERIES: &str = "series";
/// Footer key asserting the property the import depends on: rows are contiguous
/// per array key, in both files. Stated rather than assumed, so a file that has
/// been through a tool that reordered rows can say so.
pub const ROWS_CONTIGUOUS: &str = "rows_contiguous_by_key";

/// Rows per row group in the values file, targeted rather than enforced — groups
/// are cut at array-key boundaries where they can be, so a group is at most this
/// plus the tail of one array, and an array larger than this spans several.
///
/// A million rows of a scalar `f64` array is 8 MB of values before compression,
/// which is a comfortable read unit and small enough that row-group statistics on
/// `data_hash` are worth consulting. The series file is small and needs no
/// policy: it has one row per series, not per value.
pub const ROW_GROUP_TARGET: usize = 1_000_000;

/// The identity of one stored array within a partition.
///
/// **Both halves are needed.** `data_hash` covers the array bytes and not the
/// time axis: the same 8760-value profile anchored on two different years is one
/// stored array with two different timestamp columns, and for the irregular
/// types the project is explicit that two series with identical values on
/// different axes share one array and only the catalog's `timestamps_hash` tells
/// them apart.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArrayKey {
    pub data_hash: String,
    pub time_axis: String,
}

/// One string per type, spelling what decides where a value row sits in time —
/// because that, not the array bytes, is what a shared array does *not* carry:
///
/// | Type | `time_axis` |
/// | --- | --- |
/// | `SingleTimeSeries` | `R<length>/<initial>/<resolution>`, an ISO 8601 repeating interval |
/// | the two irregular types | the `timestamps_hash` of its own axis, hex — the catalog's key for it |
/// | dense forecasts | `R<count>/<initial>/<interval>/<horizon>/<resolution>` |
///
/// The instant is spelled in **UTC** whatever the partition's `time_reference`;
/// the reference is a partition key, so nothing is lost by not repeating it.
///
/// A forecast needs its horizon as well as its interval because the horizon's own
/// step decides how many `target_time` rows a window has — two forecasts sharing
/// an array, an anchor and an interval but not a horizon are different tables.
///
/// Read off the **values being exported** rather than off the catalog row. The
/// two agree for a whole-series export, and where they do not the values are
/// right: `export --time-range` hands back a slice whose anchor and length are
/// its own, and the catalog's are the unsliced series'. The row is also allowed
/// to carry less than the axis needs — `list_metadata` leaves `timestamps`
/// unpopulated, since materializing every irregular axis to list a catalog would
/// be absurd — while the values always carry all of it.
pub fn time_axis_of(data: &TimeSeriesData) -> Result<String> {
    Ok(match data {
        TimeSeriesData::SingleTimeSeries(s) => format!(
            "R{}/{}/{}",
            s.length,
            instant(s.initial_timestamp),
            s.resolution.to_iso8601()
        ),
        TimeSeriesData::NonSequentialTimeSeries(s) => hash_hex(&timestamps_hash(&s.timestamps)),
        TimeSeriesData::PersistentTimeSeries(s) => hash_hex(&timestamps_hash(&s.timestamps)),
        TimeSeriesData::Deterministic(f) => forecast_axis(
            f.count,
            f.initial_timestamp,
            f.interval,
            f.horizon,
            f.resolution,
        ),
        TimeSeriesData::Probabilistic(f) => forecast_axis(
            f.count,
            f.initial_timestamp,
            f.interval,
            f.horizon,
            f.resolution,
        ),
        TimeSeriesData::Scenarios(f) => forecast_axis(
            f.count,
            f.initial_timestamp,
            f.interval,
            f.horizon,
            f.resolution,
        ),
    })
}

fn forecast_axis(
    count: usize,
    initial: DateTime<Utc>,
    interval: Period,
    horizon: Period,
    resolution: Period,
) -> String {
    format!(
        "R{count}/{}/{}/{}/{}",
        instant(initial),
        interval.to_iso8601(),
        horizon.to_iso8601(),
        resolution.to_iso8601()
    )
}

/// Where a series' own grid columns come from: the values, for the reason
/// [`time_axis_of`] gives.
///
/// `initial_timestamp` and `count` are `None` for the two irregular types, which
/// have neither.
pub struct Grid {
    pub initial_timestamp: Option<DateTime<Utc>>,
    /// The `length` column for a `SingleTimeSeries`, the `count` column for a
    /// forecast, and nothing for the irregular types.
    pub count: Option<usize>,
}

/// The grid a series' own values describe.
pub fn grid_of(data: &TimeSeriesData) -> Grid {
    match data {
        TimeSeriesData::SingleTimeSeries(s) => Grid {
            initial_timestamp: Some(s.initial_timestamp),
            count: Some(s.length),
        },
        TimeSeriesData::NonSequentialTimeSeries(_) | TimeSeriesData::PersistentTimeSeries(_) => {
            Grid {
                initial_timestamp: None,
                count: None,
            }
        }
        TimeSeriesData::Deterministic(f) => Grid {
            initial_timestamp: Some(f.initial_timestamp),
            count: Some(f.count),
        },
        TimeSeriesData::Probabilistic(f) => Grid {
            initial_timestamp: Some(f.initial_timestamp),
            count: Some(f.count),
        },
        TimeSeriesData::Scenarios(f) => Grid {
            initial_timestamp: Some(f.initial_timestamp),
            count: Some(f.count),
        },
    }
}

/// An instant as it appears inside a `time_axis`: UTC, `Z`-suffixed, with
/// sub-second digits only when there are any.
fn instant(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// The two Arrow schemas one partition writes, plus what the partition settled
/// on.
pub struct PartitionSchema {
    pub key: PartitionKey,
    /// The width every composite row in this partition is padded to; `None` for
    /// the non-composite kinds, whose width is fixed by the value kind itself.
    pub composite_width: Option<usize>,
    pub values: SchemaRef,
    pub series: SchemaRef,
}

impl PartitionSchema {
    /// Build both schemas for `key`.
    ///
    /// `composite_width` must be the widest stored width among the partition's
    /// series, and is required exactly when the kind is composite.
    pub fn new(key: PartitionKey, composite_width: Option<usize>) -> Result<Self> {
        let value_type = value_data_type(&key.value_kind, composite_width)?;
        let stamp = timestamp_type(key.time_reference.as_ref());
        let ts_type = key.time_series_type;

        // ---- values ----
        //
        // The array key first, because that is what the rows are sorted by, then
        // the time columns, then the value. The file reads the way it is ordered.
        let mut values: Vec<Field> = vec![
            Field::new(DATA_HASH, DataType::Utf8, false),
            Field::new(TIME_AXIS, DataType::Utf8, false),
            Field::new(TIMESTAMP, stamp.clone(), false),
        ];
        if ts_type.is_forecast() {
            values.push(Field::new(ISSUE_TIME, stamp.clone(), false));
        }
        match ts_type {
            TimeSeriesType::Probabilistic => {
                values.push(Field::new(PERCENTILE, DataType::Float64, false));
            }
            TimeSeriesType::Scenarios => {
                values.push(Field::new(SCENARIO, DataType::Int64, false));
            }
            _ => {}
        }
        values.push(Field::new(VALUE, value_type, false));

        // ---- series ----
        let mut series: Vec<Field> = vec![
            Field::new(DATA_HASH, DataType::Utf8, false),
            Field::new(TIME_AXIS, DataType::Utf8, false),
            Field::new(schema::ID, DataType::Int64, false),
            Field::new(schema::OWNER_ID, DataType::Int64, false),
        ];
        for name in [
            schema::OWNER_TYPE,
            schema::OWNER_CATEGORY,
            schema::TIME_SERIES_TYPE,
            schema::NAME,
        ] {
            series.push(Field::new(name, DataType::Utf8, false));
        }
        // The temporal descriptors a type actually has. Absent from the
        // partitions whose type never carries them, rather than present and
        // empty: a column that is always the empty string is noise a reader has
        // to learn to ignore. `initial_timestamp` and `length` are also inside
        // `time_axis`, and are here as real columns so a reader need not parse
        // one.
        if ts_type == TimeSeriesType::SingleTimeSeries || ts_type.is_forecast() {
            series.push(Field::new(schema::INITIAL_TIMESTAMP, stamp, false));
            series.push(Field::new(schema::RESOLUTION, DataType::Utf8, false));
        }
        if ts_type == TimeSeriesType::SingleTimeSeries {
            series.push(Field::new(schema::LENGTH, DataType::Int64, false));
        }
        if ts_type.is_forecast() {
            series.push(Field::new(schema::INTERVAL, DataType::Utf8, false));
            series.push(Field::new(schema::HORIZON, DataType::Utf8, false));
            series.push(Field::new(schema::COUNT, DataType::Int64, false));
        }
        for name in [
            schema::FEATURES,
            schema::ELEMENT_TYPE,
            schema::ELEMENT_SHAPE,
            schema::TIME_REFERENCE,
            schema::UNITS,
            schema::QUANTITY_KIND,
            schema::UNIT_SYSTEM,
            schema::COMPONENT_FIELD,
            schema::APPLICATION_DATA,
        ] {
            series.push(Field::new(name, DataType::Utf8, false));
        }

        Ok(Self {
            values: Arc::new(Schema::new_with_metadata(
                values,
                footer(&key, composite_width, ROLE_VALUES),
            )),
            series: Arc::new(Schema::new_with_metadata(
                series,
                footer(&key, composite_width, ROLE_SERIES),
            )),
            key,
            composite_width,
        })
    }

    /// The width one row of the `value` column occupies, in elements.
    pub fn element_width(&self) -> usize {
        match (&self.key.value_kind, self.composite_width) {
            (ValueKind::Composite(_), Some(w)) => w,
            (ValueKind::Composite(_), None) => 1,
            (ValueKind::Tuple { arity, .. }, _) => *arity,
            (ValueKind::Dense { shape, .. }, _) => shape.iter().product::<usize>().max(1),
        }
    }

    /// The per-step element shape rows in this partition carry.
    pub fn element_shape(&self) -> Vec<usize> {
        match (&self.key.value_kind, self.composite_width) {
            (ValueKind::Composite(_), Some(w)) => vec![w],
            (ValueKind::Composite(_), None) => vec![],
            (ValueKind::Tuple { arity, .. }, _) => vec![*arity],
            (ValueKind::Dense { shape, .. }, _) => shape.clone(),
        }
    }
}

/// The footer: the partition key, exactly, plus the contiguity assertion.
///
/// Exact because the filename is not — [`crate::partition::sanitize`] is
/// one-way, so a reader that needs the zone back reads it here.
fn footer(
    key: &PartitionKey,
    composite_width: Option<usize>,
    role: &str,
) -> std::collections::HashMap<String, String> {
    let element_type = key.value_kind.element_type();
    let shape = match (&key.value_kind, composite_width) {
        (ValueKind::Composite(_), Some(w)) => vec![w],
        (ValueKind::Tuple { arity, .. }, _) => vec![*arity],
        (ValueKind::Dense { shape, .. }, _) => shape.clone(),
        (ValueKind::Composite(_), None) => vec![],
    };
    [
        (FORMAT.to_string(), FORMAT_V1.to_string()),
        (ROLE.to_string(), role.to_string()),
        (ROWS_CONTIGUOUS.to_string(), "true".to_string()),
        (
            schema::TIME_SERIES_TYPE.to_string(),
            key.time_series_type.as_str().to_string(),
        ),
        (schema::ELEMENT_TYPE.to_string(), element_type.to_string()),
        (
            schema::ELEMENT_SHAPE.to_string(),
            schema::encode_element_shape(&shape),
        ),
        (
            schema::TIME_REFERENCE.to_string(),
            reference_literal(key.time_reference.as_ref()),
        ),
    ]
    .into_iter()
    .collect()
}

/// How a reference is written in the `time_reference` column and footer:
/// its storage string, or [`schema::UNSPECIFIED_REFERENCE`] when there is none.
pub fn reference_literal(reference: Option<&TimeReference>) -> String {
    reference.map_or_else(
        || schema::UNSPECIFIED_REFERENCE.to_string(),
        TimeReference::as_storage_string,
    )
}

/// The `timestamp` (and `issue_time`) column's Arrow type.
///
/// The reference is a partition key, so this states the file's spelling exactly.
/// An unset reference writes a UTC-zoned column — Arrow has no third spelling —
/// and the `time_reference` column says `unspecified` so the round trip does not
/// invent a claim; see Finding 7.12.
pub fn timestamp_type(reference: Option<&TimeReference>) -> DataType {
    let zone: Option<String> = match reference {
        None | Some(TimeReference::Utc) => Some("UTC".to_string()),
        Some(TimeReference::Zoneless) => None,
        Some(r) => Some(r.as_storage_string()),
    };
    DataType::Timestamp(TimeUnit::Millisecond, zone.map(Into::into))
}

/// The `value` column's Arrow type: nested `FixedSizeList`s over the element
/// shape, innermost first, which is the order the flat row-major buffer is
/// already in.
pub fn value_data_type(kind: &ValueKind, composite_width: Option<usize>) -> Result<DataType> {
    let (leaf, dims) = match kind {
        ValueKind::Dense { dtype, shape } => (arrow_dtype(*dtype), shape.clone()),
        ValueKind::Tuple { arity, dtype } => (arrow_dtype(*dtype), vec![*arity]),
        ValueKind::Composite(_) => {
            let width = composite_width.ok_or_else(|| {
                unsupported("a composite partition needs the width its file settled on")
            })?;
            (DataType::Float64, vec![width])
        }
    };
    let mut ty = leaf;
    for dim in dims.iter().rev() {
        let size = i32::try_from(*dim)
            .map_err(|_| unsupported(format!("element dimension {dim} does not fit an i32")))?;
        ty = DataType::FixedSizeList(Arc::new(Field::new("item", ty, false)), size);
    }
    Ok(ty)
}

/// The Arrow primitive for one of the store's dtypes. Total: every dtype the
/// store has is an Arrow primitive.
pub fn arrow_dtype(dtype: infrastore_core::Dtype) -> DataType {
    use infrastore_core::Dtype;
    match dtype {
        Dtype::F64 => DataType::Float64,
        Dtype::F32 => DataType::Float32,
        Dtype::I64 => DataType::Int64,
        Dtype::I32 => DataType::Int32,
        Dtype::I16 => DataType::Int16,
        Dtype::I8 => DataType::Int8,
        Dtype::U64 => DataType::UInt64,
        Dtype::U32 => DataType::UInt32,
        Dtype::U16 => DataType::UInt16,
        Dtype::U8 => DataType::UInt8,
        Dtype::Bool => DataType::Boolean,
    }
}

/// The hex hash written in the `data_hash` column, and recomputed on import.
///
/// For every kind but the composite ones this is the catalog's own array hash:
/// a round trip re-encodes the same bytes, so the two agree.
///
/// **Composite kinds are canonicalized first.** Their stored width varies per
/// series and a file re-pads them all to its widest (see
/// [`ValueKind`][crate::partition::ValueKind]), so the packed bytes are not
/// stable across a round trip and hashing them would make an untouched export
/// fail its own checksum. Decoding and re-encoding drops the padding — `encode`
/// derives the width from the widest timestep — so the hash is over the points
/// the row *means* rather than over the slots it happens to occupy. The
/// consequence, worth knowing: for a composite series this column is not the
/// `data_hash` the catalog holds, and `id` is the way back to that.
pub fn canonical_hash(
    array: &TypedArray,
    element_type: ElementType,
    leading_dims: &[usize],
) -> Result<String> {
    Ok(hash_hex(&array_hash(&canonical_array(
        array,
        element_type,
        leading_dims,
    )?)))
}

/// The array [`canonical_hash`] hashes: the stored one, or its minimum-width
/// re-encoding for a composite kind.
pub fn canonical_array(
    array: &TypedArray,
    element_type: ElementType,
    leading_dims: &[usize],
) -> Result<TypedArray> {
    if !is_composite(element_type) {
        return Ok(array.clone());
    }
    let decoded = infrastore_core::decode(array, element_type, leading_dims.len())?;
    if matches!(decoded, infrastore_core::DecodedValues::Raw) {
        // Nothing to canonicalize: the values are already the stored elements.
        return Ok(array.clone());
    }
    infrastore_core::encode(&decoded, leading_dims)
}

/// Whether an element type is one of the four function-data kinds.
pub fn is_composite(element_type: ElementType) -> bool {
    !matches!(
        element_type,
        ElementType::Scalar(_) | ElementType::Tuple { .. }
    )
}
