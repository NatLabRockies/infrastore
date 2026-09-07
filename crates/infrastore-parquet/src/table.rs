//! The long table's shape: its columns, its footer, and the canonical form its
//! `data_hash` is taken over.
//!
//! One row per value, many series per file. Every catalog column is a table
//! column, so a reader that opens the file in DuckDB has the whole row without
//! attaching the SQLite catalog — and every column is **required**, which is
//! what the partitioning in [`crate::partition`] buys.

use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use infrastore_core::{
    ElementType, TimeReference, TimeSeriesType, TypedArray, array_hash, hash_hex,
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
/// The series' content hash, hex, as a checksum — see [`canonical_hash`].
pub const DATA_HASH: &str = "data_hash";

/// Footer key marking a file as this format, and saying which version of it.
///
/// Read before anything else, so a file from a later build is refused by
/// version rather than by whichever column it happens to be missing.
pub const FORMAT: &str = "infrastore.format";
/// The value [`FORMAT`] carries.
pub const FORMAT_V1: &str = "long_table_v1";
/// Footer key asserting the property the import depends on: every series' rows
/// are contiguous. Stated rather than assumed, so a file that has been through a
/// tool that reordered rows can say so.
pub const ROWS_CONTIGUOUS: &str = "rows_contiguous_by_series";

/// Rows per row group, targeted rather than enforced — groups are cut at series
/// boundaries where they can be, so a group is at most this plus the tail of one
/// series, and a series larger than this spans several.
///
/// A million rows of a scalar `f64` series is 8 MB of values before compression,
/// which is a comfortable read unit and small enough that row-group statistics
/// on `id`, `owner_id`, and `name` are worth consulting.
pub const ROW_GROUP_TARGET: usize = 1_000_000;

/// The Arrow schema one partition writes, plus what the partition settled on.
pub struct TableSchema {
    pub key: PartitionKey,
    /// The width every composite row in this file is padded to; `None` for the
    /// non-composite kinds, whose width is fixed by the value kind itself.
    pub composite_width: Option<usize>,
    pub schema: SchemaRef,
}

impl TableSchema {
    /// Build the schema for `key`.
    ///
    /// `composite_width` must be the widest stored width among the partition's
    /// series, and is required exactly when the kind is composite.
    pub fn new(key: PartitionKey, composite_width: Option<usize>) -> Result<Self> {
        let value_type = value_data_type(&key.value_kind, composite_width)?;
        let stamp = timestamp_type(key.time_reference.as_ref());
        let ts_type = key.time_series_type;

        let mut fields: Vec<Field> = Vec::new();
        // Key columns first, in the order rows are sorted by, so the file reads
        // the way it is ordered.
        fields.push(Field::new(TIMESTAMP, stamp.clone(), false));
        if ts_type.is_forecast() {
            fields.push(Field::new(ISSUE_TIME, stamp, false));
        }
        match ts_type {
            TimeSeriesType::Probabilistic => {
                fields.push(Field::new(PERCENTILE, DataType::Float64, false));
            }
            TimeSeriesType::Scenarios => {
                fields.push(Field::new(SCENARIO, DataType::Int64, false));
            }
            _ => {}
        }
        fields.push(Field::new(VALUE, value_type, false));

        // Then the catalog row, one column each.
        fields.push(Field::new(schema::ID, DataType::Int64, false));
        fields.push(Field::new(DATA_HASH, DataType::Utf8, false));
        fields.push(Field::new(schema::OWNER_ID, DataType::Int64, false));
        for name in [
            schema::OWNER_TYPE,
            schema::OWNER_CATEGORY,
            schema::TIME_SERIES_TYPE,
            schema::NAME,
        ] {
            fields.push(Field::new(name, DataType::Utf8, false));
        }
        // The temporal descriptors a type actually has. Absent from the files
        // whose type never carries them, rather than present and empty: a column
        // that is always the empty string is noise a reader has to learn to
        // ignore.
        if ts_type == TimeSeriesType::SingleTimeSeries || ts_type.is_forecast() {
            fields.push(Field::new(schema::RESOLUTION, DataType::Utf8, false));
        }
        if ts_type.is_forecast() {
            fields.push(Field::new(schema::INTERVAL, DataType::Utf8, false));
            fields.push(Field::new(schema::HORIZON, DataType::Utf8, false));
        }
        for name in [
            schema::FEATURES,
            schema::ELEMENT_TYPE,
            schema::TIME_REFERENCE,
            schema::UNITS,
            schema::QUANTITY_KIND,
            schema::UNIT_SYSTEM,
            schema::COMPONENT_FIELD,
            schema::APPLICATION_DATA,
        ] {
            fields.push(Field::new(name, DataType::Utf8, false));
        }

        let metadata = footer(&key, composite_width);
        Ok(Self {
            key,
            composite_width,
            schema: Arc::new(Schema::new_with_metadata(fields, metadata)),
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

    /// The per-step element shape rows in this file carry.
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
