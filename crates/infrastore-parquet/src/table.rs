//! A partition's two Arrow schemas and its footer. The Arrow-free half of the
//! layout -- the array key, the time axis, the column sets -- lives in
//! [`infrastore_tabular::layout`] and is re-exported here.
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
use infrastore_core::{TimeReference, TimeSeriesType};
pub use infrastore_tabular::layout::*;

use crate::partition::{PartitionKey, ValueKind};
use crate::schema;
use crate::{Result, unsupported};

/// Footer key marking a file as this format, and saying which version of it.
///
/// Read before anything else, so a file from a later build is refused by
/// version rather than by whichever column it happens to be missing.
pub const FORMAT: &str = "infrastore.format";
/// The value [`FORMAT`] carries.
pub const FORMAT_V1: &str = "normalized_v1";
/// Footer key saying which half of a partition a file is.
pub const ROLE: &str = "infrastore.role";
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

/// The footer keys that describe the **partition**, as opposed to the file.
///
/// Both halves of a pair carry them and must agree: they are the same
/// `PartitionKey` written twice, so a disagreement means the two files came from
/// different exports.
pub const PARTITION_KEYS: [&str; 4] = [
    schema::TIME_SERIES_TYPE,
    schema::ELEMENT_TYPE,
    schema::ELEMENT_SHAPE,
    schema::TIME_REFERENCE,
];

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

#[cfg(test)]
mod tests {
    use super::*;
    use infrastore_core::Dtype;

    /// [`required_columns`] is what the reader checks for; the schemas are what
    /// the writer emits. They are two lists of the same thing, so this holds
    /// them in step -- a column added to one and not the other would otherwise
    /// be an export no import accepts, or a promise the reader does not keep.
    #[test]
    fn the_required_columns_are_exactly_what_the_schemas_carry() {
        for ts_type in [
            TimeSeriesType::SingleTimeSeries,
            TimeSeriesType::NonSequentialTimeSeries,
            TimeSeriesType::PersistentTimeSeries,
            TimeSeriesType::Deterministic,
            TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios,
        ] {
            let key = PartitionKey {
                time_series_type: ts_type,
                value_kind: ValueKind::Dense {
                    dtype: Dtype::F64,
                    shape: Vec::new(),
                },
                time_reference: Some(TimeReference::Utc),
            };
            let table = PartitionSchema::new(key, None).expect("the schemas should build");
            for (role, schema) in [(ROLE_VALUES, &table.values), (ROLE_SERIES, &table.series)] {
                let written: Vec<&str> =
                    schema.fields().iter().map(|f| f.name().as_str()).collect();
                assert_eq!(
                    written,
                    required_columns(ts_type, role),
                    "{} {role}",
                    ts_type.as_str()
                );
            }
        }
    }
}
