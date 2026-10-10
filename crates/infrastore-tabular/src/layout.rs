//! The normalized layout's Arrow-free core: the array key a partition's arrays
//! table spells once, the per-type time axis, the column sets each of the three
//! tables carries, and the content hash a values group is keyed by.
//!
//! Every writer of the layout — Parquet files, SQLite tables — builds its
//! columns from [`required_columns`], so the formats cannot drift apart.

use chrono::{DateTime, SecondsFormat, Utc};
use infrastore_core::{
    ElementType, Period, TimeReference, TimeSeriesData, TimeSeriesType, TypedArray, array_hash,
    hash_hex, timestamps_hash,
};

use crate::Result;
use crate::schema;

/// The values half of a partition.
pub const ROLE_VALUES: &str = "values";
/// The series half.
pub const ROLE_SERIES: &str = "series";
/// The arrays table: one row per distinct array, spelling its key once.
pub const ROLE_ARRAYS: &str = "arrays";

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
/// What a values or series row names its array by: the `id` of its row in the
/// arrays table. An integer because it repeats on every value row, and the key
/// it stands for is some hundred bytes of text. **Local to one export** — ids
/// are assigned as the tables are written and mean nothing across two.
pub const ARRAY_ID: &str = "array_id";
/// Half the array key: the hex content hash of the array — see
/// [`canonical_hash`]. Also a checksum on import.
pub const DATA_HASH: &str = "data_hash";
/// The other half: what determines the timestamps a value row sits at, spelled
/// per type — see [`time_axis_of`].
pub const TIME_AXIS: &str = "time_axis";

/// The identity of one stored array within a partition.
///
/// Written **once per array**, in the arrays table, beside the integer `id` the
/// values and series rows carry instead ([`ARRAY_ID`]). A reader resolves the id
/// back to this, so the join and the checksum still run on the key.
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
        TimeSeriesData::SingleTimeSeries(s) => {
            grid_axis(s.length, s.initial_timestamp, s.resolution)
        }
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

/// A `SingleTimeSeries`' `time_axis` from its parts, so a whole-series export
/// can spell it off the catalog row without reading the values.
pub fn grid_axis(length: usize, initial: DateTime<Utc>, resolution: Period) -> String {
    format!("R{length}/{}/{}", instant(initial), resolution.to_iso8601())
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

/// Every column a table of this role and type must carry, in schema order.
///
/// The single source of truth for "the format promises every column is
/// required": [`PartitionSchema::new`] builds these fields and the reader checks
/// for them by name before it reads a row, so a file of ours that is missing one
/// is refused naming the column rather than silently defaulting it. A unit test
/// below holds the two in step.
///
/// Only files carrying the [`FORMAT`] marker are held to it. A foreign file is
/// allowed to carry almost nothing — that is what the inline options are for.
pub fn required_columns(ts_type: TimeSeriesType, role: &str) -> Vec<&'static str> {
    if role == ROLE_ARRAYS {
        return vec![schema::ID, DATA_HASH, TIME_AXIS];
    }
    if role == ROLE_VALUES {
        let mut names = vec![ARRAY_ID, TIMESTAMP];
        if ts_type.is_forecast() {
            names.push(ISSUE_TIME);
        }
        match ts_type {
            TimeSeriesType::Probabilistic => names.push(PERCENTILE),
            TimeSeriesType::Scenarios => names.push(SCENARIO),
            _ => {}
        }
        names.push(VALUE);
        return names;
    }
    let mut names = vec![
        ARRAY_ID,
        schema::ID,
        schema::OWNER_ID,
        schema::OWNER_TYPE,
        schema::OWNER_CATEGORY,
        schema::TIME_SERIES_TYPE,
        schema::NAME,
    ];
    if ts_type == TimeSeriesType::SingleTimeSeries || ts_type.is_forecast() {
        names.push(schema::INITIAL_TIMESTAMP);
        names.push(schema::RESOLUTION);
    }
    if ts_type == TimeSeriesType::SingleTimeSeries {
        names.push(schema::LENGTH);
    }
    if ts_type.is_forecast() {
        names.push(schema::INTERVAL);
        names.push(schema::HORIZON);
        names.push(schema::COUNT);
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

/// Every column a table of this role carries in **any** partition, in schema
/// order: the superset of [`required_columns`] over the types.
///
/// What a view across partitions selects. A partition's own table holds only
/// the columns its type has, so the rest read as `NULL` there.
pub fn all_columns(role: &str) -> Vec<&'static str> {
    if role == ROLE_ARRAYS {
        return vec![schema::ID, DATA_HASH, TIME_AXIS];
    }
    if role == ROLE_VALUES {
        return vec![ARRAY_ID, TIMESTAMP, ISSUE_TIME, PERCENTILE, SCENARIO, VALUE];
    }
    vec![
        ARRAY_ID,
        schema::ID,
        schema::OWNER_ID,
        schema::OWNER_TYPE,
        schema::OWNER_CATEGORY,
        schema::TIME_SERIES_TYPE,
        schema::NAME,
        schema::INITIAL_TIMESTAMP,
        schema::RESOLUTION,
        schema::LENGTH,
        schema::INTERVAL,
        schema::HORIZON,
        schema::COUNT,
        schema::FEATURES,
        schema::ELEMENT_TYPE,
        schema::ELEMENT_SHAPE,
        schema::TIME_REFERENCE,
        schema::UNITS,
        schema::QUANTITY_KIND,
        schema::UNIT_SYSTEM,
        schema::COMPONENT_FIELD,
        schema::APPLICATION_DATA,
    ]
}

/// How a reference is written in the `time_reference` column and footer:
/// its storage string, or [`schema::UNSPECIFIED_REFERENCE`] when there is none.
pub fn reference_literal(reference: Option<&TimeReference>) -> String {
    reference.map_or_else(
        || schema::UNSPECIFIED_REFERENCE.to_string(),
        TimeReference::as_storage_string,
    )
}

/// The hex hash written in the arrays table's `data_hash` column, and
/// recomputed on import.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A view across partitions selects [`all_columns`]; a column a type's own
    /// table carries and the view does not would silently vanish from it.
    #[test]
    fn all_columns_holds_every_types_columns_in_the_same_order() {
        for ts_type in [
            TimeSeriesType::SingleTimeSeries,
            TimeSeriesType::NonSequentialTimeSeries,
            TimeSeriesType::PersistentTimeSeries,
            TimeSeriesType::Deterministic,
            TimeSeriesType::DeterministicSingleTimeSeries,
            TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios,
        ] {
            for role in [ROLE_VALUES, ROLE_SERIES, ROLE_ARRAYS] {
                let all = all_columns(role);
                let mut rest = all.iter();
                for column in required_columns(ts_type, role) {
                    assert!(
                        rest.any(|c| *c == column),
                        "{} {role}: `{column}` is missing from, or out of order in, {all:?}",
                        ts_type.as_str()
                    );
                }
            }
        }
    }
}
