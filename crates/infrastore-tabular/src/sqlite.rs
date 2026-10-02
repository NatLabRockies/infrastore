//! The layout as tables in a SQLite database.
//!
//! Same normalization, same partitions, same columns as the Parquet files: per
//! `(time_series_type, value type, time_reference)` triple, a **values** table
//! holding every distinct array once and a **series** table holding one catalog
//! row per series, joined on the array key `(data_hash, time_axis)`. The column
//! sets are [`layout::required_columns`], so the two containers cannot drift
//! apart.
//!
//! What SQLite cannot say the way Arrow does:
//!
//! - **Instants** (`timestamp`, `issue_time`, `initial_timestamp`) are `INTEGER`
//!   unix milliseconds. The partition's spelling is the `time_reference` column
//!   of its series table, exactly as the Parquet series file carries it.
//! - **`value`** is a plain `REAL`/`INTEGER` for a scalar element, and a JSON
//!   array (nested over the element shape) otherwise. A composite row keeps its
//!   own stored width rather than being re-padded, since a JSON array needs no
//!   common width. `NaN` is `NULL` -- SQLite stores it as one anyway -- so
//!   `value` is the one nullable column. A `u64` past `i64::MAX` is refused.
//!
//! # Names
//!
//! A partition's tables are `<prefix><base>_values` and `<prefix><base>_series`,
//! plus an index `<prefix><base>_values_key`. `<base>` is the Parquet stem with
//! everything outside `[A-Za-z0-9_]` mapped to `_`, so it always begins with the
//! time-series type: `SingleTimeSeries_f64_utc`. That is what lets the import
//! find exactly one export's tables: a pair belongs to prefix `P` when its name
//! is `P` followed by a type name and `_`. The empty prefix therefore does not
//! pick up `run1_SingleTimeSeries_…`, and neither picks up a user's own
//! `foo_values`.
//!
//! # Writing into an existing database
//!
//! Tables are only ever added, never replaced: a name already taken fails the
//! export before anything is written, and the whole export is one transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use infrastore_core::{Dtype, TimeSeriesData, TimeSeriesMetadata, TimeSeriesType, TypedArray};
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params_from_iter};
use serde_json::Value as Json;

use crate::export::{descriptor_row, per_step_shape, plan, refuse_empty, series_rows};
use crate::import::{ImportOptions, LaneValue, SeriesRow, SeriesSink, ValuesGroup, merge_join};
use crate::layout::{self, ArrayKey};
use crate::partition::{PartitionKey, ValueKind};
use crate::schema;
use crate::{Result, unsupported};

const VALUES_SUFFIX: &str = "_values";
const SERIES_SUFFIX: &str = "_series";
const INDEX_SUFFIX: &str = "_values_key";

/// One partition the export wrote: its two tables and what they hold.
#[derive(Debug, Clone)]
pub struct WrittenTables {
    pub values_table: String,
    pub series_table: String,
    pub time_series_type: TimeSeriesType,
    pub value_slug: String,
    pub reference: String,
    pub arrays: usize,
    pub series: usize,
    pub rows: usize,
}

/// One partition in a database: the shared name its two tables extend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlitePartition {
    pub db: PathBuf,
    /// `<prefix><base>`, without the `_values` / `_series` suffix.
    pub base: String,
}

impl SqlitePartition {
    pub fn values_table(&self) -> String {
        format!("{}{VALUES_SUFFIX}", self.base)
    }

    pub fn series_table(&self) -> String {
        format!("{}{SERIES_SUFFIX}", self.base)
    }

    /// What to call this partition in a message.
    pub fn label(&self) -> String {
        format!("{}:{}", self.db.display(), self.base)
    }
}

/// Refuse a prefix that would need quoting, so every table the layout names
/// stays usable as a bare SQL identifier, or that could read as part of
/// another prefix's tables.
///
/// Discovery takes `<prefix><type>_…`, so a prefix must end in `_` and hold no
/// `_`-separated segment that is a type name: otherwise the tables of export
/// `SingleTimeSeries_` (or `Deterministic`, before a `SingleTimeSeries` stem)
/// would also be picked up by an import with a shorter prefix.
pub fn check_prefix(prefix: &str) -> Result<()> {
    if !prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(unsupported(format!(
            "table prefix {prefix:?} may only contain ASCII letters, digits and '_'"
        )));
    }
    if prefix.is_empty() {
        return Ok(());
    }
    let Some(body) = prefix.strip_suffix('_') else {
        return Err(unsupported(format!(
            "table prefix {prefix:?} must end in '_' (e.g. {:?})",
            format!("{prefix}_")
        )));
    };
    if let Some(segment) = body.split('_').find(|s| TimeSeriesType::parse(s).is_some()) {
        return Err(unsupported(format!(
            "table prefix {prefix:?} contains the time-series type name {segment:?}, which \
             would make its tables readable as another prefix's"
        )));
    }
    Ok(())
}

/// Write `series` into the SQLite database at `path`, creating it if absent,
/// naming every table with `prefix`.
///
/// Fails without writing anything if any table or index it would create is
/// already in the database (compared case-insensitively, as SQLite does), or if
/// any selected series is empty. A file this call created is removed again on
/// failure. An empty selection touches nothing, not even to create the file.
pub fn write_sqlite(
    path: &Path,
    series: &[(TimeSeriesMetadata, TimeSeriesData)],
    prefix: &str,
) -> Result<Vec<WrittenTables>> {
    check_prefix(prefix)?;
    refuse_empty(series)?;
    // Nothing to write is nothing written: opening would create the file.
    if series.is_empty() {
        return Ok(Vec::new());
    }
    let existed = path.exists();
    let result = Connection::open(path)
        .map_err(sqlite_err)
        .and_then(|mut conn| write_all(&mut conn, series, prefix));
    if result.is_err() && !existed {
        // Best effort: the connection is closed, so the file is ours to remove.
        let _ = std::fs::remove_file(path);
    }
    result
}

fn write_all(
    conn: &mut Connection,
    series: &[(TimeSeriesMetadata, TimeSeriesData)],
    prefix: &str,
) -> Result<Vec<WrittenTables>> {
    let groups = plan(series)?;
    let bases = table_bases(groups.keys(), prefix);

    // IMMEDIATE so the name check and the writes see the same database.
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sqlite_err)?;
    refuse_collisions(&tx, bases.values())?;

    let mut written = Vec::with_capacity(groups.len());
    for (key, arrays) in &groups {
        let base = &bases[key];
        let ts_type = key.time_series_type;
        let values_table = format!("{base}{VALUES_SUFFIX}");
        let series_table = format!("{base}{SERIES_SUFFIX}");
        let (q_values, q_series) = (quote_ident(&values_table), quote_ident(&series_table));

        let values_columns = layout::required_columns(ts_type, layout::ROLE_VALUES);
        let series_columns = layout::required_columns(ts_type, layout::ROLE_SERIES);
        tx.execute_batch(&format!(
            "CREATE TABLE {q_values} ({});
             CREATE INDEX \"{base}{INDEX_SUFFIX}\" ON {q_values} ({}, {});
             CREATE TABLE {q_series} ({});",
            column_defs(&values_columns, &key.value_kind),
            layout::DATA_HASH,
            layout::TIME_AXIS,
            column_defs(&series_columns, &key.value_kind),
        ))
        .map_err(sqlite_err)?;

        let mut rows = 0usize;
        let mut insert = tx
            .prepare(&insert_sql(&values_table, &values_columns))
            .map_err(sqlite_err)?;
        for (array, members) in arrays {
            // Every series sharing a key has the same values at the same instants.
            let (row, data) = &series[members[0]];
            rows += insert_values(&mut insert, array, row, data, &key.value_kind)?;
        }
        drop(insert);

        let mut insert = tx
            .prepare(&insert_sql(&series_table, &series_columns))
            .map_err(sqlite_err)?;
        let mut count = 0usize;
        for (array, members) in arrays {
            for index in members {
                let (row, data) = &series[*index];
                let mut cells = series_cells(array, row, data);
                let params = series_columns
                    .iter()
                    .map(|c| cells.remove(c).expect("every required column has a cell"));
                insert
                    .execute(params_from_iter(params))
                    .map_err(sqlite_err)?;
                count += 1;
            }
        }
        drop(insert);

        written.push(WrittenTables {
            values_table,
            series_table,
            time_series_type: ts_type,
            value_slug: key.value_kind.slug(),
            reference: layout::reference_literal(key.time_reference.as_ref()),
            arrays: arrays.len(),
            series: count,
            rows,
        });
    }
    tx.commit().map_err(sqlite_err)?;
    Ok(written)
}

/// `<prefix>` plus the partition's stem as a SQL identifier: everything but
/// `[A-Za-z0-9_]` mapped to `_`. Collisions that mapping (or SQLite's
/// case-insensitivity) creates get a numeric suffix, in key order so a re-run
/// names its tables the same way.
fn table_bases<'a>(
    keys: impl Iterator<Item = &'a PartitionKey>,
    prefix: &str,
) -> BTreeMap<PartitionKey, String> {
    let mut taken = BTreeSet::new();
    let mut out = BTreeMap::new();
    for key in keys {
        let stem: String = key
            .stem()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let base = format!("{prefix}{stem}");
        let mut candidate = base.clone();
        let mut n = 1;
        while !taken.insert(candidate.to_lowercase()) {
            n += 1;
            candidate = format!("{base}_{n}");
        }
        out.insert(key.clone(), candidate);
    }
    out
}

/// Every name in the database, lower-cased.
fn existing_names(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut stmt = conn
        .prepare("SELECT lower(name) FROM sqlite_master")
        .map_err(sqlite_err)?;
    stmt.query_map([], |r| r.get(0))
        .map_err(sqlite_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sqlite_err)
}

/// Fail, naming them, if any name the export would create already exists.
fn refuse_collisions<'a>(conn: &Connection, bases: impl Iterator<Item = &'a String>) -> Result<()> {
    let existing = existing_names(conn)?;
    let clashes: Vec<String> = bases
        .flat_map(|b| [VALUES_SUFFIX, INDEX_SUFFIX, SERIES_SUFFIX].map(|s| format!("{b}{s}")))
        .filter(|name| existing.contains(&name.to_lowercase()))
        .collect();
    if clashes.is_empty() {
        return Ok(());
    }
    Err(unsupported(format!(
        "the database already holds {} of the names this export would create ({}); nothing was \
         written. Export into a fresh file, pick another table prefix, or drop those tables \
         first.",
        clashes.len(),
        clashes.join(", ")
    )))
}

fn column_defs(columns: &[&str], kind: &ValueKind) -> String {
    columns
        .iter()
        .map(|&c| match c {
            layout::VALUE => format!("{c} {}", value_affinity(kind)),
            schema::ID => format!("{c} INTEGER PRIMARY KEY"),
            _ => format!("{c} {} NOT NULL", column_affinity(c)),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn column_affinity(column: &str) -> &'static str {
    match column {
        layout::TIMESTAMP
        | layout::ISSUE_TIME
        | layout::SCENARIO
        | schema::OWNER_ID
        | schema::INITIAL_TIMESTAMP
        | schema::LENGTH
        | schema::COUNT => "INTEGER",
        layout::PERCENTILE => "REAL",
        _ => "TEXT",
    }
}

fn value_affinity(kind: &ValueKind) -> &'static str {
    match kind {
        ValueKind::Dense { dtype, shape } if shape.is_empty() => match dtype {
            Dtype::F64 | Dtype::F32 => "REAL",
            _ => "INTEGER",
        },
        _ => "TEXT",
    }
}

fn insert_sql(table: &str, columns: &[&str]) -> String {
    format!(
        "INSERT INTO \"{table}\" ({}) VALUES ({})",
        columns.join(", "),
        vec!["?"; columns.len()].join(", ")
    )
}

/// Insert one array's value rows, returning how many.
fn insert_values(
    insert: &mut rusqlite::Statement<'_>,
    key: &ArrayKey,
    row: &TimeSeriesMetadata,
    data: &TimeSeriesData,
    kind: &ValueKind,
) -> Result<usize> {
    let rows = series_rows(row, data)?;
    let elements = elements(rows.array)?;
    let shape = per_step_shape(row);
    let scalar = matches!(kind, ValueKind::Dense { shape: dims, .. } if dims.is_empty());
    for (k, offset) in rows.offsets.iter().enumerate() {
        let start = offset * rows.per_step;
        let cells = elements.get(start..start + rows.per_step).ok_or_else(|| {
            unsupported(format!("series '{}' is shorter than its shape", row.name))
        })?;
        let value = if scalar {
            // A REAL `-0.0` reads back as `+0.0`; refuse it rather than flip
            // the sign the checksum is taken over.
            if matches!(cells[0], Value::Real(f) if f == 0.0 && f.is_sign_negative()) {
                return Err(unsupported(format!(
                    "series '{}' holds a scalar -0.0, which a SQLite REAL cannot store",
                    row.name
                )));
            }
            cells[0].clone()
        } else {
            Value::Text(nest(cells, &shape).to_string())
        };
        let mut params = vec![
            Value::Text(key.data_hash.clone()),
            Value::Text(key.time_axis.clone()),
            Value::Integer(rows.target[k]),
        ];
        if !rows.issue.is_empty() {
            params.push(Value::Integer(rows.issue[k]));
        }
        if !rows.percentile.is_empty() {
            params.push(Value::Real(rows.percentile[k]));
        }
        if !rows.scenario.is_empty() {
            params.push(Value::Integer(rows.scenario[k]));
        }
        params.push(value);
        insert
            .execute(params_from_iter(params))
            .map_err(sqlite_err)?;
    }
    Ok(rows.offsets.len())
}

/// One series row, keyed by column name. Holds a cell for every column any
/// type's series table has; the caller picks the ones its table carries.
fn series_cells(
    key: &ArrayKey,
    row: &TimeSeriesMetadata,
    data: &TimeSeriesData,
) -> HashMap<&'static str, Value> {
    let grid = layout::grid_of(data);
    let count = grid.count.unwrap_or(0) as i64;
    let mut cells: HashMap<&'static str, Value> = HashMap::from([
        (layout::DATA_HASH, Value::Text(key.data_hash.clone())),
        (layout::TIME_AXIS, Value::Text(key.time_axis.clone())),
        (schema::ID, Value::Integer(row.id.map_or(0, |i| i.get()))),
        (schema::OWNER_ID, Value::Integer(row.owner_id)),
        (schema::LENGTH, Value::Integer(count)),
        (schema::COUNT, Value::Integer(count)),
    ]);
    if let Some(at) = grid.initial_timestamp {
        cells.insert(
            schema::INITIAL_TIMESTAMP,
            Value::Integer(at.timestamp_millis()),
        );
    }
    for (name, text) in descriptor_row(row.time_series_type, row) {
        cells.insert(name, Value::Text(text));
    }
    cells
}

/// Every element of an array as a SQLite value, in row-major order.
fn elements(array: &TypedArray) -> Result<Vec<Value>> {
    macro_rules! ints {
        ($t:ty) => {
            array
                .to_vec::<$t>()
                .map_err(unsupported)?
                .into_iter()
                .map(|v| Value::Integer(i64::from(v)))
                .collect()
        };
    }
    let real = |v: f64| {
        if v.is_nan() {
            Value::Null
        } else {
            Value::Real(v)
        }
    };
    Ok(match array.dtype {
        Dtype::F64 => array
            .to_vec::<f64>()
            .map_err(unsupported)?
            .into_iter()
            .map(real)
            .collect(),
        Dtype::F32 => array
            .to_vec::<f32>()
            .map_err(unsupported)?
            .into_iter()
            .map(|v| real(f64::from(v)))
            .collect(),
        Dtype::I64 => ints!(i64),
        Dtype::I32 => ints!(i32),
        Dtype::I16 => ints!(i16),
        Dtype::I8 => ints!(i8),
        Dtype::U32 => ints!(u32),
        Dtype::U16 => ints!(u16),
        Dtype::U8 => ints!(u8),
        Dtype::Bool => ints!(bool),
        Dtype::U64 => array
            .to_vec::<u64>()
            .map_err(unsupported)?
            .into_iter()
            .map(|v| {
                i64::try_from(v).map(Value::Integer).map_err(|_| {
                    unsupported(format!("u64 value {v} does not fit a SQLite INTEGER"))
                })
            })
            .collect::<Result<_>>()?,
    })
}

/// One row's elements as a JSON array nested over `shape`, outermost first.
fn nest(cells: &[Value], shape: &[usize]) -> Json {
    match shape {
        [] | [_] => Json::Array(cells.iter().map(json_of).collect()),
        [_, inner @ ..] => {
            let stride = inner.iter().product::<usize>().max(1);
            Json::Array(cells.chunks(stride).map(|c| nest(c, inner)).collect())
        }
    }
}

/// How a JSON `value` cell spells the infinities, which JSON numbers cannot.
const INFINITY: &str = "Infinity";
const NEG_INFINITY: &str = "-Infinity";

fn json_of(value: &Value) -> Json {
    match value {
        Value::Integer(i) => Json::from(*i),
        // JSON has no infinity, so it is spelled as a string; `NaN` arrives
        // here already `NULL`.
        Value::Real(f) if f.is_infinite() => {
            Json::from(if *f > 0.0 { INFINITY } else { NEG_INFINITY })
        }
        Value::Real(f) => Json::from(*f),
        _ => Json::Null,
    }
}

// ---- Reading ----------------------------------------------------------------

/// Every partition under `prefix` in the database at `path`, sorted by name.
///
/// A pair belongs to `prefix` when its name is the prefix, a time-series type
/// name, `_`, and the rest; see the module docs. Finding none is an error, since
/// importing nothing is almost always a wrong prefix or a wrong file. So is a
/// values table with no series table beside it.
pub fn sqlite_partitions(path: &Path, prefix: &str) -> Result<Vec<SqlitePartition>> {
    check_prefix(prefix)?;
    let conn = open_readonly(path)?;
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .map_err(sqlite_err)?;
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .map_err(sqlite_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sqlite_err)?;
    let lower: BTreeSet<String> = tables.iter().map(|t| t.to_lowercase()).collect();

    let mut out = Vec::new();
    for table in &tables {
        let Some(base) = table.strip_suffix(VALUES_SUFFIX) else {
            continue;
        };
        let Some(rest) = base.strip_prefix(prefix) else {
            continue;
        };
        let is_ours = rest
            .split_once('_')
            .is_some_and(|(head, _)| TimeSeriesType::parse(head).is_some());
        if !is_ours {
            continue;
        }
        if !lower.contains(&format!("{base}{SERIES_SUFFIX}").to_lowercase()) {
            return Err(unsupported(format!(
                "{} holds {table} but no {base}{SERIES_SUFFIX} beside it; a partition's two \
                 tables come from one export",
                path.display()
            )));
        }
        out.push(SqlitePartition {
            db: path.to_path_buf(),
            base: base.to_string(),
        });
    }
    if out.is_empty() {
        return Err(unsupported(format!(
            "{} holds no exported partitions{}",
            path.display(),
            if prefix.is_empty() {
                " without a table prefix (pass the prefix the export used)".to_string()
            } else {
                format!(" under the table prefix {prefix:?}")
            }
        )));
    }
    out.sort_by(|a, b| a.base.cmp(&b.base));
    Ok(out)
}

/// Stream one partition's series into `sink`, returning how many were handed
/// over. Both tables are read in array-key order and merge-joined, so peak
/// memory is one array, never a partition.
pub fn read_sqlite_partition_with(
    partition: &SqlitePartition,
    options: &ImportOptions,
    sink: SeriesSink<'_>,
) -> Result<usize> {
    let conn = open_readonly(&partition.db)?;
    let values_table = partition.values_table();
    let series_table = partition.series_table();
    // Discovery admits any name after `<prefix><type>_`, so quote what it found.
    let (q_values, q_series) = (quote_ident(&values_table), quote_ident(&series_table));

    // The partition's type and element type are constant across its series
    // table; they decide which columns to expect and how to decode `value`.
    let mut stmt = conn
        .prepare(&format!(
            "SELECT DISTINCT {}, {} FROM {q_series}",
            schema::TIME_SERIES_TYPE,
            schema::ELEMENT_TYPE
        ))
        .map_err(sqlite_err)?;
    let kinds: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(sqlite_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sqlite_err)?;
    let (ts_type, element_type) = match kinds.as_slice() {
        [] => {
            let values: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {q_values}"), [], |r| {
                    r.get(0)
                })
                .map_err(sqlite_err)?;
            if values == 0 {
                return Ok(0);
            }
            return Err(unsupported(format!(
                "{series_table} is empty but {values_table} is not; a partition's two tables \
                 come from one export"
            )));
        }
        [(ts, et)] => (
            schema::decode_time_series_type(ts).map_err(unsupported)?,
            schema::decode_element_type(et).map_err(unsupported)?,
        ),
        _ => {
            return Err(unsupported(format!(
                "{series_table} mixes time-series or element types; a partition holds one of each"
            )));
        }
    };
    let values_columns = check_columns(&conn, &values_table, ts_type, layout::ROLE_VALUES)?;
    let series_columns = check_columns(&conn, &series_table, ts_type, layout::ROLE_SERIES)?;
    let dtype = ValueKind::of(element_type, &[]).leaf_dtype();

    // ---- values ----
    let mut select = vec![layout::DATA_HASH, layout::TIME_AXIS, layout::TIMESTAMP];
    let has_issue = values_columns.contains(layout::ISSUE_TIME);
    let lane = [layout::PERCENTILE, layout::SCENARIO]
        .into_iter()
        .find(|c| values_columns.contains(*c));
    if has_issue {
        select.push(layout::ISSUE_TIME);
    }
    select.extend(lane);
    select.push(layout::VALUE);
    let mut values_stmt = conn
        .prepare(&format!(
            "SELECT {} FROM {q_values} ORDER BY {}, {}, {}",
            select.join(", "),
            layout::DATA_HASH,
            layout::TIME_AXIS,
            layout::TIMESTAMP,
        ))
        .map_err(sqlite_err)?;
    let mut values_rows = values_stmt.query([]).map_err(sqlite_err)?;
    let mut held: Option<(ArrayKey, ValueRow)> = None;
    let next_values = || -> Result<Option<ValuesGroup>> {
        let first = match held.take() {
            Some(first) => first,
            None => match values_rows.next().map_err(sqlite_err)? {
                Some(row) => value_row(row, has_issue, lane, dtype)?,
                None => return Ok(None),
            },
        };
        let (key, row) = first;
        let mut group = ValuesGroup {
            key,
            timestamps: Vec::new(),
            issue: Vec::new(),
            lanes: Vec::new(),
            dims: row.dims.clone(),
            dtype,
            bytes: Vec::new(),
            zone: None,
        };
        push_value(&mut group, row)?;
        while let Some(next) = values_rows.next().map_err(sqlite_err)? {
            let (key, row) = value_row(next, has_issue, lane, dtype)?;
            if key != group.key {
                held = Some((key, row));
                break;
            }
            push_value(&mut group, row)?;
        }
        Ok(Some(group))
    };

    // ---- series ----
    // Every `SeriesRow` field, read from its column when the table has one and
    // as the empty string when the type's table does not (a static series has
    // no `interval`).
    let text_fields = [
        schema::OWNER_TYPE,
        schema::OWNER_CATEGORY,
        schema::TIME_SERIES_TYPE,
        schema::NAME,
        schema::RESOLUTION,
        schema::INTERVAL,
        schema::HORIZON,
        schema::FEATURES,
        schema::ELEMENT_TYPE,
        schema::TIME_REFERENCE,
        schema::UNITS,
        schema::QUANTITY_KIND,
        schema::UNIT_SYSTEM,
        schema::COMPONENT_FIELD,
        schema::APPLICATION_DATA,
    ];
    let mut select: Vec<String> = [
        layout::DATA_HASH,
        layout::TIME_AXIS,
        schema::ID,
        schema::OWNER_ID,
    ]
    .map(String::from)
    .to_vec();
    select.extend(text_fields.iter().map(|c| {
        if series_columns.contains(*c) {
            c.to_string()
        } else {
            "''".to_string()
        }
    }));
    let mut series_stmt = conn
        .prepare(&format!(
            "SELECT {} FROM {q_series} ORDER BY {}, {}, {}",
            select.join(", "),
            layout::DATA_HASH,
            layout::TIME_AXIS,
            schema::ID,
        ))
        .map_err(sqlite_err)?;
    let mut series_rows = series_stmt.query([]).map_err(sqlite_err)?;
    let mut held_row: Option<SeriesRow> = None;
    let next_rows = || -> Result<Option<Vec<SeriesRow>>> {
        let first = match held_row.take() {
            Some(first) => first,
            None => match series_rows.next().map_err(sqlite_err)? {
                Some(row) => series_row(row)?,
                None => return Ok(None),
            },
        };
        let mut group = vec![first];
        while let Some(next) = series_rows.next().map_err(sqlite_err)? {
            let row = series_row(next)?;
            if row.key != group[0].key {
                held_row = Some(row);
                break;
            }
            group.push(row);
        }
        Ok(Some(group))
    };

    merge_join(next_values, next_rows, &BTreeMap::new(), options, sink)
}

fn open_readonly(path: &Path) -> Result<Connection> {
    if !path.is_file() {
        return Err(unsupported(format!("{} is not a file", path.display())));
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sqlite_err)
}

/// `name` as a quoted SQL identifier.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The table's columns, after checking it carries every one the layout
/// requires of it. Extra columns are allowed — a user may have added some.
fn check_columns(
    conn: &Connection,
    table: &str,
    ts_type: TimeSeriesType,
    role: &str,
) -> Result<BTreeSet<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info(?1)")
        .map_err(sqlite_err)?;
    let columns: BTreeSet<String> = stmt
        .query_map([table], |r| r.get(0))
        .map_err(sqlite_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(sqlite_err)?;
    let missing: Vec<&str> = layout::required_columns(ts_type, role)
        .into_iter()
        .filter(|c| !columns.contains(*c))
        .collect();
    if missing.is_empty() {
        return Ok(columns);
    }
    Err(unsupported(format!(
        "{table} is missing column(s) a {} {role} table carries: {}",
        ts_type.as_str(),
        missing.join(", ")
    )))
}

/// One values-table row, decoded.
struct ValueRow {
    timestamp: DateTime<Utc>,
    issue: Option<DateTime<Utc>>,
    lane: Option<LaneValue>,
    dims: Vec<usize>,
    bytes: Vec<u8>,
}

fn value_row(
    row: &rusqlite::Row<'_>,
    has_issue: bool,
    lane: Option<&str>,
    dtype: Dtype,
) -> Result<(ArrayKey, ValueRow)> {
    let key = ArrayKey {
        data_hash: row.get(0).map_err(sqlite_err)?,
        time_axis: row.get(1).map_err(sqlite_err)?,
    };
    let mut i = 2;
    let mut next = || {
        i += 1;
        i - 1
    };
    let timestamp = instant(row.get(next()).map_err(sqlite_err)?)?;
    let issue = has_issue
        .then(|| row.get(next()).map_err(sqlite_err).and_then(instant))
        .transpose()?;
    let lane = match lane {
        Some(layout::PERCENTILE) => {
            Some(LaneValue::Percentile(row.get(next()).map_err(sqlite_err)?))
        }
        Some(_) => Some(LaneValue::Scenario(row.get(next()).map_err(sqlite_err)?)),
        None => None,
    };
    let cell: Value = row.get(next()).map_err(sqlite_err)?;
    let mut dims = Vec::new();
    let mut numbers = Vec::new();
    match cell {
        Value::Text(text) => {
            let json: Json = serde_json::from_str(&text)
                .map_err(|e| unsupported(format!("a `{}` cell is not JSON: {e}", layout::VALUE)))?;
            flatten(&json, 0, &mut dims, &mut None, &mut numbers)?;
        }
        Value::Integer(n) => numbers.push(Num::Int(n)),
        Value::Real(r) => numbers.push(Num::Real(r)),
        Value::Null => numbers.push(Num::Null),
        Value::Blob(_) => {
            return Err(unsupported(format!(
                "a `{}` cell is a blob; the layout writes numbers or JSON text",
                layout::VALUE
            )));
        }
    }
    let mut bytes = Vec::with_capacity(numbers.len() * dtype.size());
    for n in numbers {
        push_number(dtype, n, &mut bytes)?;
    }
    Ok((
        key,
        ValueRow {
            timestamp,
            issue,
            lane,
            dims,
            bytes,
        },
    ))
}

fn push_value(group: &mut ValuesGroup, row: ValueRow) -> Result<()> {
    if row.dims != group.dims {
        return Err(unsupported(format!(
            "two `{}` cells of one array have different shapes ({:?} and {:?})",
            layout::VALUE,
            group.dims,
            row.dims
        )));
    }
    group.timestamps.push(row.timestamp);
    group.issue.extend(row.issue);
    group.lanes.extend(row.lane);
    group.bytes.extend(row.bytes);
    Ok(())
}

fn series_row(row: &rusqlite::Row<'_>) -> Result<SeriesRow> {
    let text = |i: usize| -> Result<String> { row.get(i).map_err(sqlite_err) };
    Ok(SeriesRow {
        key: ArrayKey {
            data_hash: text(0)?,
            time_axis: text(1)?,
        },
        id: row.get(2).map_err(sqlite_err)?,
        owner_id: Some(row.get(3).map_err(sqlite_err)?),
        owner_type: text(4)?,
        owner_category: text(5)?,
        time_series_type: text(6)?,
        name: text(7)?,
        resolution: text(8)?,
        interval: text(9)?,
        horizon: text(10)?,
        features: text(11)?,
        element_type: text(12)?,
        time_reference: text(13)?,
        units: text(14)?,
        quantity_kind: text(15)?,
        unit_system: text(16)?,
        component_field: text(17)?,
        application_data: text(18)?,
    })
}

fn instant(ms: i64) -> Result<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .ok_or_else(|| unsupported(format!("timestamp {ms} ms is not representable")))
}

/// One leaf of a `value` cell.
#[derive(Clone, Copy)]
enum Num {
    Int(i64),
    Real(f64),
    Null,
}

/// Walk a JSON `value` cell, recording the per-step shape from its nesting and
/// refusing a ragged one: every leaf at one depth, every array at a depth as
/// long as the first one seen there.
fn flatten(
    json: &Json,
    depth: usize,
    dims: &mut Vec<usize>,
    leaf_depth: &mut Option<usize>,
    out: &mut Vec<Num>,
) -> Result<()> {
    let ragged = || unsupported(format!("a `{}` cell is a ragged JSON array", layout::VALUE));
    match json {
        Json::Array(items) => {
            if leaf_depth.is_some_and(|l| depth >= l) {
                return Err(ragged());
            }
            match dims.get(depth) {
                None => dims.push(items.len()),
                Some(&n) if n == items.len() => {}
                Some(_) => return Err(ragged()),
            }
            for item in items {
                flatten(item, depth + 1, dims, leaf_depth, out)?;
            }
        }
        leaf => {
            if *leaf_depth.get_or_insert(depth) != depth || dims.len() != depth {
                return Err(ragged());
            }
            out.push(match leaf {
                Json::Null => Num::Null,
                Json::Bool(b) => Num::Int(i64::from(*b)),
                Json::Number(n) => n
                    .as_i64()
                    .map_or_else(|| Num::Real(n.as_f64().unwrap_or(f64::NAN)), Num::Int),
                Json::String(s) if s == INFINITY => Num::Real(f64::INFINITY),
                Json::String(s) if s == NEG_INFINITY => Num::Real(f64::NEG_INFINITY),
                _ => {
                    return Err(unsupported(format!(
                        "a `{}` cell holds {leaf}, not a number",
                        layout::VALUE
                    )));
                }
            });
        }
    }
    Ok(())
}

/// Encode one leaf as `dtype`, little-endian. `NULL` is `NaN` for a float and
/// an error otherwise; an integer is never rounded into place.
fn push_number(dtype: Dtype, n: Num, out: &mut Vec<u8>) -> Result<()> {
    macro_rules! int {
        ($t:ty) => {{
            let Num::Int(i) = n else {
                return Err(unsupported(format!(
                    "a {} array holds a value that is not an integer",
                    dtype.as_str()
                )));
            };
            let v = <$t>::try_from(i)
                .map_err(|_| unsupported(format!("{i} does not fit a {}", dtype.as_str())))?;
            out.extend_from_slice(&v.to_le_bytes());
        }};
    }
    let real = match n {
        Num::Int(i) => i as f64,
        Num::Real(r) => r,
        Num::Null => f64::NAN,
    };
    match dtype {
        Dtype::F64 => out.extend_from_slice(&real.to_le_bytes()),
        Dtype::F32 => out.extend_from_slice(&(real as f32).to_le_bytes()),
        Dtype::I64 => int!(i64),
        Dtype::I32 => int!(i32),
        Dtype::I16 => int!(i16),
        Dtype::I8 => int!(i8),
        Dtype::U64 => int!(u64),
        Dtype::U32 => int!(u32),
        Dtype::U16 => int!(u16),
        Dtype::U8 => int!(u8),
        Dtype::Bool => match n {
            Num::Int(0) => out.push(0),
            Num::Int(1) => out.push(1),
            _ => return Err(unsupported("a bool array holds a value other than 0 or 1")),
        },
    }
    Ok(())
}

/// SQLite errors describe the database, not the data handed in.
fn sqlite_err(e: rusqlite::Error) -> infrastore_core::TimeSeriesError {
    infrastore_core::TimeSeriesError::Io(std::io::Error::other(format!("sqlite: {e}")))
}
