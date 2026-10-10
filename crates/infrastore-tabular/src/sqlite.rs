//! The layout as tables in a SQLite database.
//!
//! Same normalization, same partitions, same columns as the Parquet files: per
//! `(time_series_type, value type, time_reference)` triple, a **values** table
//! holding every distinct array once, a **series** table holding one catalog
//! row per series, and an **arrays** table spelling each array's key
//! `(data_hash, time_axis)` once. The arrays table's `id` is its rowid, and the
//! other two name an array by it (`array_id`) -- SQLite does not compress, so
//! the key's text on every value row would dwarf the values. The column sets
//! are [`layout::required_columns`], so the two containers cannot drift apart.
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
//! A partition's tables are `<prefix><base>_values`, `<prefix><base>_series`
//! and `<prefix><base>_arrays`, plus an index `<prefix><base>_values_key` on
//! the values table's `array_id`. `<base>` is the Parquet stem with
//! everything outside `[A-Za-z0-9_]` mapped to `_`, so it always begins with the
//! time-series type: `SingleTimeSeries_f64_utc`. That is what lets the import
//! find exactly one export's tables: a pair belongs to prefix `P` when its name
//! is `P` followed by a type name and `_`. The empty prefix therefore does not
//! pick up `run1_SingleTimeSeries_…`, and neither picks up a user's own
//! `foo_values`.
//!
//! # Fixed names
//!
//! Which partitions an export writes depends on the data, so a consumer cannot
//! name their tables ahead of time. Beside them the export keeps three
//! **views** whose names it can: `<prefix>all_values`, `<prefix>all_series`
//! and `<prefix>all_arrays`, each a `UNION ALL` over every partition under the
//! prefix, led by a `partition_name` column and carrying every column any
//! partition has (`NULL` where a partition's type has none). `array_id` is per
//! partition, so a join across the views is on `(partition_name, array_id)`.
//!
//! # Writing into an existing database
//!
//! Tables are only ever added, never replaced: a name already taken fails the
//! export before anything is written, and the whole export is one transaction.
//! The three views are the exception that is not one: they are derived, so an
//! export under a prefix that already has them replaces them to span the new
//! partitions too. A view or table of that name the export did not write is a
//! collision like any other.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use infrastore_core::{
    AddRequest, Dtype, ListFilter, ReadWindow, Store, TimeRange, TimeSeriesData, TimeSeriesError,
    TimeSeriesId, TimeSeriesMetadata, TimeSeriesType, TypedArray,
};
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params_from_iter};
use serde_json::Value as Json;

use crate::export::{
    array_key, descriptor_row, per_step_shape, plan_keys, refuse_empty_counts, retain_exportable,
    row_count, series_rows,
};
use crate::import::{
    ImportOptions, LaneValue, SeriesRow, SeriesSink, ValuesGroup, dangling, merge_join,
};
use crate::layout::{self, ArrayKey};
use crate::partition::{PartitionKey, ValueKind};
use crate::schema;
use crate::{Result, unsupported};

const VALUES_SUFFIX: &str = "_values";
const SERIES_SUFFIX: &str = "_series";
const ARRAYS_SUFFIX: &str = "_arrays";

/// The three views an export keeps under each prefix, by the role of the
/// tables each one spans and the suffix those tables carry. Their names are
/// **fixed** -- `<prefix>all_values`, `<prefix>all_series`,
/// `<prefix>all_arrays` -- where a partition's own tables are named for a
/// partition a consumer cannot know in advance.
const VIEWS: [(&str, &str, &str); 3] = [
    (layout::ROLE_VALUES, VALUES_SUFFIX, "all_values"),
    (layout::ROLE_SERIES, SERIES_SUFFIX, "all_series"),
    (layout::ROLE_ARRAYS, ARRAYS_SUFFIX, "all_arrays"),
];
/// The views' first column: which partition a row came from, as its tables'
/// shared name without the prefix. `array_id` (and the arrays table's `id`) is
/// per partition, so a join across the views is on this **and** the id.
pub const PARTITION_NAME: &str = "partition_name";
/// In a view's SQL, what marks it as one this export wrote and may replace.
const VIEW_MARKER: &str = "/* infrastore: every partition under this prefix */";
const INDEX_SUFFIX: &str = "_values_key";

/// One partition the export wrote: its three tables and what they hold.
#[derive(Debug, Clone)]
pub struct WrittenTables {
    pub values_table: String,
    pub series_table: String,
    pub arrays_table: String,
    pub time_series_type: TimeSeriesType,
    pub value_slug: String,
    pub reference: String,
    pub arrays: usize,
    pub series: usize,
    pub rows: usize,
}

/// One partition in a database: the shared name its three tables extend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlitePartition {
    pub db: PathBuf,
    /// `<prefix><base>`, without the `_values` / `_series` / `_arrays` suffix.
    pub base: String,
}

impl SqlitePartition {
    pub fn values_table(&self) -> String {
        format!("{}{VALUES_SUFFIX}", self.base)
    }

    pub fn series_table(&self) -> String {
        format!("{}{SERIES_SUFFIX}", self.base)
    }

    pub fn arrays_table(&self) -> String {
        format!("{}{ARRAYS_SUFFIX}", self.base)
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
    if prefix.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(unsupported(format!(
            "table prefix {prefix:?} must not start with a digit, which would make its \
             table names need quoting"
        )));
    }
    if prefix.to_ascii_lowercase().starts_with("sqlite_") {
        return Err(unsupported(format!(
            "table prefix {prefix:?} starts with \"sqlite_\", which SQLite reserves for its own \
             tables"
        )));
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

/// What the writer needs of one series before its values are in hand: enough to
/// plan every table and to write the series row, so the values themselves are
/// only needed once, for the array's own rows.
struct Planned<'a> {
    row: &'a TimeSeriesMetadata,
    key: ArrayKey,
    grid: layout::Grid,
    /// How many value rows the series contributes.
    rows: usize,
}

impl<'a> Planned<'a> {
    fn of(row: &'a TimeSeriesMetadata, data: &TimeSeriesData) -> Result<Self> {
        Ok(Self {
            row,
            key: array_key(row, data)?,
            grid: layout::grid_of(data),
            rows: row_count(row, data)?,
        })
    }

    /// The same, off the catalog row alone -- possible exactly when the row
    /// says everything the values would: a whole `SingleTimeSeries` whose
    /// stored bytes are what gets hashed (a composite kind is re-encoded first,
    /// see [`layout::canonical_hash`]). [`export_store`] holds the values to
    /// this when it does read them.
    fn from_catalog(row: &'a TimeSeriesMetadata) -> Option<Self> {
        if row.time_series_type != TimeSeriesType::SingleTimeSeries
            || layout::is_composite(row.element_type)
        {
            return None;
        }
        let (initial, resolution, length) = (row.initial_timestamp?, row.resolution?, row.length?);
        Some(Self {
            row,
            key: ArrayKey {
                data_hash: infrastore_core::hash_hex(&row.data_hash),
                time_axis: layout::grid_axis(length, initial, resolution),
            },
            grid: layout::Grid {
                initial_timestamp: Some(initial),
                count: Some(length),
            },
            rows: length,
        })
    }
}

/// Hands the writer the values of the series at the given positions, in that
/// one call per series, in whatever order suits the source. The writer asks for
/// one series per distinct array, a partition at a time, so a source need never
/// hold more than it chooses to read at once.
type Values<'a> =
    &'a mut dyn FnMut(&[usize], &mut dyn FnMut(usize, &TimeSeriesData) -> Result<()>) -> Result<()>;

/// Write `series` into the SQLite database at `path`, creating it if absent,
/// naming every table with `prefix`.
///
/// Fails without writing anything if any table or index it would create is
/// already in the database (compared case-insensitively, as SQLite does), or if
/// any selected series is empty. A file this call created is removed again on
/// failure while it still holds no tables. An empty selection touches nothing,
/// not even to create the file.
///
/// For values already in memory; [`export_store`] is the same export streamed
/// out of a store.
pub fn write_sqlite(
    path: &Path,
    series: &[(TimeSeriesMetadata, TimeSeriesData)],
    prefix: &str,
) -> Result<Vec<WrittenTables>> {
    check_prefix(prefix)?;
    let planned = series
        .iter()
        .map(|(row, data)| Planned::of(row, data))
        .collect::<Result<Vec<_>>>()?;
    write_planned(path, &planned, prefix, &mut |positions, each| {
        positions.iter().try_for_each(|&i| each(i, &series[i].1))
    })
}

fn write_planned(
    path: &Path,
    series: &[Planned<'_>],
    prefix: &str,
    values: Values<'_>,
) -> Result<Vec<WrittenTables>> {
    refuse_empty_counts(series.iter().map(|p| (p.row, p.rows)))?;
    // Nothing to write is nothing written: opening would create the file.
    if series.is_empty() {
        return Ok(Vec::new());
    }
    // Claim an absent destination atomically: an empty file is an empty
    // database, and only a file this call created is ever removed.
    let created = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e.into()),
    };
    let mut conn = match Connection::open(path) {
        Ok(conn) => conn,
        Err(e) => {
            if created {
                let _ = std::fs::remove_file(path);
            }
            return Err(e.into());
        }
    };
    let result = write_all(&mut conn, series, prefix, values);
    // Another exporter may have opened the file this call created and committed
    // into it, so remove it only while it still holds no schema at all.
    // ponytail: a commit landing between this check and the removal is lost;
    // staging and renaming into place would close that window.
    let untouched = created
        && result.is_err()
        && conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                r.get::<_, i64>(0)
            })
            .is_ok_and(|n| n == 0);
    drop(conn);
    if untouched {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn write_all(
    conn: &mut Connection,
    series: &[Planned<'_>],
    prefix: &str,
    values: Values<'_>,
) -> Result<Vec<WrittenTables>> {
    let groups = plan_keys(series.iter().map(|p| (p.row, p.key.clone())));
    let bases = table_bases(groups.keys(), prefix);

    // IMMEDIATE so the name check and the writes see the same database.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    refuse_collisions(&tx, bases.values(), prefix)?;

    let mut written = Vec::with_capacity(groups.len());
    // Every series reading an array that holds a scalar `-0.0`, across all
    // partitions: one failure that names them all, not one per attempt.
    let mut negative_zero: Vec<&TimeSeriesMetadata> = Vec::new();
    for (key, arrays) in &groups {
        let base = &bases[key];
        let ts_type = key.time_series_type;
        let values_table = format!("{base}{VALUES_SUFFIX}");
        let series_table = format!("{base}{SERIES_SUFFIX}");
        let arrays_table = format!("{base}{ARRAYS_SUFFIX}");
        let (q_values, q_series, q_arrays) = (
            quote_ident(&values_table),
            quote_ident(&series_table),
            quote_ident(&arrays_table),
        );

        let values_columns = layout::required_columns(ts_type, layout::ROLE_VALUES);
        let series_columns = layout::required_columns(ts_type, layout::ROLE_SERIES);
        let arrays_columns = layout::required_columns(ts_type, layout::ROLE_ARRAYS);
        tx.execute_batch(&format!(
            "CREATE TABLE {q_arrays} ({}, UNIQUE ({}, {}));
             CREATE TABLE {q_values} ({});
             CREATE TABLE {q_series} ({});",
            column_defs(&arrays_columns, &key.value_kind, &q_arrays),
            layout::DATA_HASH,
            layout::TIME_AXIS,
            column_defs(&values_columns, &key.value_kind, &q_arrays),
            column_defs(&series_columns, &key.value_kind, &q_arrays),
        ))?;

        // The arrays table first: ids in key order, from 1, so a re-run of the
        // same export numbers its arrays the same way.
        let mut insert = tx.prepare(&insert_sql(&arrays_table, &arrays_columns))?;
        let mut array_ids = HashMap::with_capacity(arrays.len());
        for (id, (array, members)) in (1i64..).zip(arrays) {
            insert.execute((id, &array.data_hash, &array.time_axis))?;
            // Every series sharing a key has the same values at the same
            // instants, so the first stands for the array.
            array_ids.insert(members[0], (id, members));
        }
        drop(insert);

        let mut rows = 0usize;
        let mut insert = tx.prepare(&insert_sql(&values_table, &values_columns))?;
        let firsts: Vec<usize> = arrays.values().map(|members| members[0]).collect();
        values(&firsts, &mut |position, data| {
            let one = &series[position];
            let (id, members) = array_ids[&position];
            match insert_values(&mut insert, id, one.row, data, &key.value_kind)? {
                Some(inserted) => rows += inserted,
                None => negative_zero.extend(members.iter().map(|&m| series[m].row)),
            }
            Ok(())
        })?;
        drop(insert);
        // After the rows rather than before: a streamed export hands the arrays
        // over in storage order, and keying an index one array at a time is far
        // slower than sorting it once. The name was checked above.
        tx.execute_batch(&format!(
            "CREATE INDEX \"{base}{INDEX_SUFFIX}\" ON {q_values} ({});",
            layout::ARRAY_ID,
        ))?;

        let mut insert = tx.prepare(&insert_sql(&series_table, &series_columns))?;
        let mut count = 0usize;
        for (id, members) in (1i64..).zip(arrays.values()) {
            for position in members {
                let mut cells = series_cells(&series[*position], id);
                let params = series_columns
                    .iter()
                    .map(|c| cells.remove(c).expect("every required column has a cell"));
                insert.execute(params_from_iter(params))?;
                count += 1;
            }
        }
        drop(insert);

        written.push(WrittenTables {
            values_table,
            series_table,
            arrays_table,
            time_series_type: ts_type,
            value_slug: key.value_kind.slug(),
            reference: layout::reference_literal(key.time_reference.as_ref()),
            arrays: arrays.len(),
            series: count,
            rows,
        });
    }
    if !negative_zero.is_empty() {
        // In catalog order, whatever order the arrays were read in.
        negative_zero.sort_by_key(|row| row.id);
        let negative_zero: Vec<String> = negative_zero
            .iter()
            .map(|row| format!("'{}' (owner {})", row.name, row.owner_id))
            .collect();
        // Dropping the transaction rolls back everything written on the way.
        return Err(unsupported(format!(
            "{} of the selected series hold a scalar -0.0, which a SQLite REAL cannot store (it \
             reads back as +0.0): {}. Nothing was written. Narrow the selection past them, or \
             export to Parquet.",
            negative_zero.len(),
            negative_zero.join(", ")
        )));
    }
    rebuild_views(&tx, prefix)?;
    tx.commit()?;
    Ok(written)
}

/// (Re)create the prefix's three fixed-name views over every partition now
/// under it -- the ones this export wrote and any an earlier one did.
fn rebuild_views(conn: &Connection, prefix: &str) -> Result<()> {
    let bases = partition_bases(conn, prefix, "the database")?;
    for (role, suffix, view) in VIEWS {
        let columns = layout::all_columns(role);
        let mut selects = Vec::with_capacity(bases.len());
        for base in &bases {
            let table = format!("{base}{suffix}");
            let present = table_columns(conn, &table)?;
            let cells: Vec<String> = columns
                .iter()
                .map(|&c| {
                    if present.contains(c) {
                        c.to_string()
                    } else {
                        format!("NULL AS {c}")
                    }
                })
                .collect();
            selects.push(format!(
                "SELECT '{}' AS {PARTITION_NAME}, {} FROM {}",
                base[prefix.len()..].replace('\'', "''"),
                cells.join(", "),
                quote_ident(&table)
            ));
        }
        // ponytail: one UNION ALL term per partition, and SQLite caps a compound
        // SELECT at 500 terms by default; chain views if an export ever has more.
        let q_view = quote_ident(&format!("{prefix}{view}"));
        conn.execute_batch(&format!(
            "DROP VIEW IF EXISTS {q_view};
             CREATE VIEW {q_view} AS {VIEW_MARKER} {};",
            selects.join(" UNION ALL ")
        ))?;
    }
    Ok(())
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
    let mut stmt = conn.prepare("SELECT lower(name) FROM sqlite_master")?;
    stmt.query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()
        .map_err(infrastore_core::TimeSeriesError::from)
}

/// Fail, naming them, if any name the export would create already exists.
///
/// The prefix's views are exempt when they are this export's own: those are
/// replaced. Anything else under one of their names is a collision.
fn refuse_collisions<'a>(
    conn: &Connection,
    bases: impl Iterator<Item = &'a String>,
    prefix: &str,
) -> Result<()> {
    let existing = existing_names(conn)?;
    let mut clashes: Vec<String> = bases
        .flat_map(|b| {
            [VALUES_SUFFIX, INDEX_SUFFIX, SERIES_SUFFIX, ARRAYS_SUFFIX].map(|s| format!("{b}{s}"))
        })
        .filter(|name| existing.contains(&name.to_lowercase()))
        .collect();
    let mut ours = conn.prepare(
        "SELECT count(*) FROM sqlite_master \
         WHERE lower(name) = lower(?1) AND type = 'view' AND instr(sql, ?2) > 0",
    )?;
    for (_, _, view) in VIEWS {
        let name = format!("{prefix}{view}");
        if existing.contains(&name.to_lowercase())
            && ours.query_row((&name, VIEW_MARKER), |r| r.get::<_, i64>(0))? == 0
        {
            clashes.push(name);
        }
    }
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

/// `q_arrays` is the partition's arrays table, quoted, for `array_id` to
/// reference.
fn column_defs(columns: &[&str], kind: &ValueKind, q_arrays: &str) -> String {
    columns
        .iter()
        .map(|&c| match c {
            layout::VALUE => format!("{c} {}", value_affinity(kind)),
            // The rowid, in the series table and the arrays table alike.
            schema::ID => format!("{c} INTEGER PRIMARY KEY"),
            layout::ARRAY_ID => format!("{c} INTEGER NOT NULL REFERENCES {q_arrays} (id)"),
            _ => format!("{c} {} NOT NULL", column_affinity(c)),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn column_affinity(column: &str) -> &'static str {
    match column {
        layout::ARRAY_ID
        | layout::TIMESTAMP
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

/// Insert one array's value rows, returning how many -- or `None`, inserting
/// nothing, when the array holds a scalar `-0.0`.
///
/// A REAL `-0.0` reads back as `+0.0`, which would flip the sign the checksum
/// is taken over, so the export refuses it. Reported rather than raised so the
/// caller can go on to find every such array and name them all at once.
fn insert_values(
    insert: &mut rusqlite::Statement<'_>,
    array_id: i64,
    row: &TimeSeriesMetadata,
    data: &TimeSeriesData,
    kind: &ValueKind,
) -> Result<Option<usize>> {
    let rows = series_rows(row, data)?;
    let elements = elements(rows.array)?;
    let shape = per_step_shape(row);
    let scalar = matches!(kind, ValueKind::Dense { shape: dims, .. } if dims.is_empty());
    if scalar
        && elements
            .iter()
            .any(|cell| matches!(cell, Value::Real(f) if *f == 0.0 && f.is_sign_negative()))
    {
        return Ok(None);
    }
    for (k, offset) in rows.offsets.iter().enumerate() {
        let start = offset * rows.per_step;
        let cells = elements.get(start..start + rows.per_step).ok_or_else(|| {
            unsupported(format!("series '{}' is shorter than its shape", row.name))
        })?;
        let value = if scalar {
            cells[0].clone()
        } else {
            Value::Text(nest(cells, &shape).to_string())
        };
        let mut params = vec![Value::Integer(array_id), Value::Integer(rows.target[k])];
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
        insert.execute(params_from_iter(params))?;
    }
    Ok(Some(rows.offsets.len()))
}

/// One series row, keyed by column name. Holds a cell for every column any
/// type's series table has; the caller picks the ones its table carries.
fn series_cells(series: &Planned<'_>, array_id: i64) -> HashMap<&'static str, Value> {
    let Planned { row, grid, .. } = series;
    let count = grid.count.unwrap_or(0) as i64;
    let mut cells: HashMap<&'static str, Value> = HashMap::from([
        (layout::ARRAY_ID, Value::Integer(array_id)),
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
/// values table without its series and arrays tables beside it.
pub fn sqlite_partitions(path: &Path, prefix: &str) -> Result<Vec<SqlitePartition>> {
    check_prefix(prefix)?;
    let conn = open_readonly(path)?;
    let bases = partition_bases(&conn, prefix, &path.display().to_string())?;
    if bases.is_empty() {
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
    Ok(bases
        .into_iter()
        .map(|base| SqlitePartition {
            db: path.to_path_buf(),
            base,
        })
        .collect())
}

/// The shared table name of every partition under `prefix`, sorted. `what`
/// names the database in an error.
fn partition_bases(conn: &Connection, prefix: &str, what: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let lower: BTreeSet<String> = tables.iter().map(|t| t.to_lowercase()).collect();

    let mut out = Vec::new();
    for table in &tables {
        let Some(base) = table.strip_suffix(VALUES_SUFFIX) else {
            continue;
        };
        // SQLite names are case-insensitive, so `Run_` and `run_` are one prefix.
        let Some(rest) = base
            .split_at_checked(prefix.len())
            .and_then(|(head, rest)| head.eq_ignore_ascii_case(prefix).then_some(rest))
        else {
            continue;
        };
        let is_ours = rest
            .split_once('_')
            .is_some_and(|(head, _)| TimeSeriesType::parse(head).is_some());
        if !is_ours {
            continue;
        }
        for suffix in [SERIES_SUFFIX, ARRAYS_SUFFIX] {
            if !lower.contains(&format!("{base}{suffix}").to_lowercase()) {
                return Err(unsupported(format!(
                    "{what} holds {table} but no {base}{suffix} beside it; a partition's tables \
                     come from one export"
                )));
            }
        }
        out.push(base.to_string());
    }
    out.sort();
    Ok(out)
}

/// Stream one partition's series into `sink`, returning how many were handed
/// over. The values and series tables are each read through the arrays table,
/// in array-key order, and merge-joined, so peak memory is one array, never a
/// partition.
pub fn read_sqlite_partition_with(
    partition: &SqlitePartition,
    options: &ImportOptions,
    sink: SeriesSink<'_>,
) -> Result<usize> {
    let conn = open_readonly(&partition.db)?;
    let values_table = partition.values_table();
    let series_table = partition.series_table();
    let arrays_table = partition.arrays_table();
    // Discovery admits any name after `<prefix><type>_`, so quote what it found.
    let (q_values, q_series, q_arrays) = (
        quote_ident(&values_table),
        quote_ident(&series_table),
        quote_ident(&arrays_table),
    );

    // The partition's type and element type are constant across its series
    // table; they decide which columns to expect and how to decode `value`.
    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT {}, {} FROM {q_series}",
        schema::TIME_SERIES_TYPE,
        schema::ELEMENT_TYPE
    ))?;
    let kinds: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let (ts_type, element_type) = match kinds.as_slice() {
        [] => {
            let values: i64 =
                conn.query_row(&format!("SELECT count(*) FROM {q_values}"), [], |r| {
                    r.get(0)
                })?;
            if values == 0 {
                return Ok(0);
            }
            return Err(unsupported(format!(
                "{series_table} is empty but {values_table} is not; a partition's tables \
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
    check_columns(&conn, &arrays_table, ts_type, layout::ROLE_ARRAYS)?;
    let dtype = ValueKind::of(element_type, &[]).leaf_dtype();

    // An inner join drops a row whose `array_id` names nothing, so look for
    // one first: either dangling side is an error, not a shorter import.
    for (q_table, half) in [
        (&q_values, layout::ROLE_VALUES),
        (&q_series, layout::ROLE_SERIES),
    ] {
        let orphan: Option<i64> = conn
            .query_row(
                &format!(
                    "SELECT {id} FROM {q_table} WHERE {id} NOT IN (SELECT id FROM {q_arrays}) \
                     LIMIT 1",
                    id = layout::ARRAY_ID
                ),
                [],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        if let Some(id) = orphan {
            return Err(dangling(id, half));
        }
    }

    // ---- values ----
    let mut select = vec!["a.data_hash", "a.time_axis", "v.timestamp"];
    let has_issue = values_columns.contains(layout::ISSUE_TIME);
    let lane = [layout::PERCENTILE, layout::SCENARIO]
        .into_iter()
        .find(|c| values_columns.contains(*c));
    if has_issue {
        select.push(layout::ISSUE_TIME);
    }
    select.extend(lane);
    select.push(layout::VALUE);
    // CROSS JOIN pins the order: the arrays table walked by its key (its
    // UNIQUE index), each array's rows fetched through the values index.
    let mut values_stmt = conn.prepare(&format!(
        "SELECT {} FROM {q_arrays} AS a CROSS JOIN {q_values} AS v ON v.{} = a.id \
         ORDER BY a.data_hash, a.time_axis, v.timestamp",
        select.join(", "),
        layout::ARRAY_ID,
    ))?;
    let mut values_rows = values_stmt.query([])?;
    let mut held: Option<(ArrayKey, ValueRow)> = None;
    let next_values = || -> Result<Option<ValuesGroup>> {
        let first = match held.take() {
            Some(first) => first,
            None => match values_rows.next()? {
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
        while let Some(next) = values_rows.next()? {
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
    let mut select: Vec<String> = ["a.data_hash", "a.time_axis", "s.id", "s.owner_id"]
        .map(String::from)
        .to_vec();
    select.extend(text_fields.iter().map(|c| {
        if series_columns.contains(*c) {
            format!("s.{c}")
        } else {
            "''".to_string()
        }
    }));
    let mut series_stmt = conn.prepare(&format!(
        "SELECT {} FROM {q_series} AS s JOIN {q_arrays} AS a ON a.id = s.{} \
         ORDER BY a.data_hash, a.time_axis, s.id",
        select.join(", "),
        layout::ARRAY_ID,
    ))?;
    let mut series_rows = series_stmt.query([])?;
    let mut held_row: Option<SeriesRow> = None;
    let next_rows = || -> Result<Option<Vec<SeriesRow>>> {
        let first = match held_row.take() {
            Some(first) => first,
            None => match series_rows.next()? {
                Some(row) => series_row(row)?,
                None => return Ok(None),
            },
        };
        let mut group = vec![first];
        while let Some(next) = series_rows.next()? {
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
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(infrastore_core::TimeSeriesError::from)
}

/// `name` as a quoted SQL identifier.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn table_columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    stmt.query_map([table], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()
        .map_err(infrastore_core::TimeSeriesError::from)
}

/// The table's columns, after checking it carries every one the layout
/// requires of it. Extra columns are allowed — a user may have added some.
fn check_columns(
    conn: &Connection,
    table: &str,
    ts_type: TimeSeriesType,
    role: &str,
) -> Result<BTreeSet<String>> {
    let columns = table_columns(conn, table)?;
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
        data_hash: row.get(0)?,
        time_axis: row.get(1)?,
    };
    let mut i = 2;
    let mut next = || {
        i += 1;
        i - 1
    };
    let timestamp = instant(row.get(next())?)?;
    let issue = has_issue
        .then(|| {
            row.get(next())
                .map_err(infrastore_core::TimeSeriesError::from)
                .and_then(instant)
        })
        .transpose()?;
    let lane = match lane {
        Some(layout::PERCENTILE) => Some(LaneValue::Percentile(row.get(next())?)),
        Some(_) => Some(LaneValue::Scenario(row.get(next())?)),
        None => None,
    };
    let cell: Value = row.get(next())?;
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
    let text =
        |i: usize| -> Result<String> { row.get(i).map_err(infrastore_core::TimeSeriesError::from) };
    Ok(SeriesRow {
        key: ArrayKey {
            data_hash: text(0)?,
            time_axis: text(1)?,
        },
        id: row.get(2)?,
        owner_id: Some(row.get(3)?),
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

/// How much one read of [`export_store`] asks the store for, by the catalog's
/// own account of each series.
///
/// A read is transiently several times this: the backend fetches a span of
/// columns, splits it, and hands each series its own copy.
// ponytail: fixed rather than a parameter; make it one if a host with less
// memory than a few of these needs to export.
const READ_BUDGET_BYTES: usize = 16 << 20;

/// How many reads go by before the store is asked to give back what they
/// cached (see [`Store::release_read_caches`]). Not every read, because a
/// packed dataset straddling two reads is then inflated twice.
const READS_PER_RELEASE: usize = 4;

/// An upper bound on a series' stored bytes from its catalog row: no dtype is
/// wider than eight bytes.
fn stored_bytes(row: &TimeSeriesMetadata) -> usize {
    let per_step: usize = row.element_shape.iter().product();
    row.length
        .unwrap_or(1)
        .max(1)
        .saturating_mul(per_step.max(1))
        .saturating_mul(8)
}

/// Read the series at `positions` of `metas` a budget's worth at a time,
/// handing each to `each` and dropping it before the next read.
///
/// **In catalog-id order, not the order given.** Ids are issued in the order
/// series were added, which is the order their arrays were packed into the
/// file, so consecutive ids sit in the same few datasets -- and a dataset is
/// inflated whole to read any column of it. Reading in any other order (the
/// writer asks in array-key order, which is a hash's) would inflate every
/// dataset for every read.
fn read_in_chunks(
    store: &Store,
    metas: &[TimeSeriesMetadata],
    ids: &[TimeSeriesId],
    time_range: Option<TimeRange>,
    positions: &[usize],
    budget: usize,
    each: &mut dyn FnMut(usize, &TimeSeriesData) -> Result<()>,
) -> Result<()> {
    let mut positions = positions.to_vec();
    positions.sort_by_key(|&i| ids[i]);
    let mut rest = positions.as_slice();
    let mut reads = 0usize;
    while !rest.is_empty() {
        // Always at least one, however large it is.
        let mut bytes = 0usize;
        let take = rest
            .iter()
            .take_while(|&&i| {
                bytes = bytes.saturating_add(stored_bytes(&metas[i]));
                bytes <= budget
            })
            .count()
            .max(1);
        let (chunk, tail) = rest.split_at(take);
        rest = tail;
        let chunk_ids: Vec<TimeSeriesId> = chunk.iter().map(|&i| ids[i]).collect();
        let datas = match time_range {
            Some(range) => store.read_by_ids_range(&chunk_ids, range)?,
            None => store.read_by_ids(&chunk_ids, ReadWindow::full())?,
        };
        if datas.len() != chunk.len() {
            return Err(TimeSeriesError::IntegrityError(format!(
                "a read of {} series returned {}",
                chunk.len(),
                datas.len()
            )));
        }
        for (&position, data) in chunk.iter().zip(&datas) {
            each(position, data)?;
        }
        reads += 1;
        if reads.is_multiple_of(READS_PER_RELEASE) {
            store.release_read_caches();
        }
    }
    // What this pass cached is no use to whatever the caller does next.
    store.release_read_caches();
    Ok(())
}

/// Export the series `filter` selects from `store` into the SQLite database at
/// `path` -- [`write_sqlite`] streamed out of a store, the call a binding and
/// the CLI make.
///
/// `time_range` clips each series the way [`Store::read_by_ids_range`] does;
/// `None` exports each whole.
///
/// **Memory is bounded by a read budget, not by the selection.** The tables are
/// planned from what each series' key and grid are, which for a whole
/// `SingleTimeSeries` of plain values the catalog row already says; anything
/// else (an irregular axis, a forecast, a composite kind, a clipped series) is
/// read a budget's worth at a time to learn them and dropped again. The values
/// are then read once per **distinct array**, again a budget at a time, so a
/// profile a thousand components share is read and written once. The store's
/// read caches are released as it goes, since they would otherwise grow to
/// most of the file.
///
/// Within a partition the arrays are therefore written in the order they are
/// stored rather than in key order; the tables' index is what orders them.
///
/// A `DeterministicSingleTimeSeries` is **omitted unless `include_derived`**
/// (see [`retain_exportable`]), and a filter naming the type without it is
/// refused. Including them has a cost beyond the repeated values: the import
/// refuses the type, and a database's only selector is its prefix, so one such
/// partition makes the whole prefix unreadable back.
pub fn export_store(
    store: &Store,
    filter: ListFilter,
    time_range: Option<TimeRange>,
    path: &Path,
    prefix: &str,
    include_derived: bool,
) -> Result<Vec<WrittenTables>> {
    // Before any read, so a bad prefix does not cost the selection.
    check_prefix(prefix)?;
    let filter_type = filter.time_series_type;
    let mut metas = store.list_metadata(filter)?;
    retain_exportable(filter_type, &mut metas, include_derived)?;
    let ids: Vec<TimeSeriesId> = metas
        .iter()
        .map(|m| {
            m.id.ok_or_else(|| unsupported(format!("row {:?} carries no catalog id", m.name)))
        })
        .collect::<Result<_>>()?;

    let mut planned: Vec<Option<Planned<'_>>> = metas
        .iter()
        .map(|row| {
            time_range
                .is_none()
                .then(|| Planned::from_catalog(row))
                .flatten()
        })
        .collect();
    // What the catalog planned is checked against the values when they are read.
    let from_catalog: Vec<bool> = planned.iter().map(Option::is_some).collect();
    let unread: Vec<usize> = (0..metas.len()).filter(|&i| !from_catalog[i]).collect();
    let budget = READ_BUDGET_BYTES;
    read_in_chunks(
        store,
        &metas,
        &ids,
        time_range,
        &unread,
        budget,
        &mut |i, data| {
            planned[i] = Some(Planned::of(&metas[i], data)?);
            Ok(())
        },
    )?;
    let planned: Vec<Planned<'_>> = planned
        .into_iter()
        .map(|p| p.expect("every position was planned from the catalog or read"))
        .collect();

    write_planned(path, &planned, prefix, &mut |positions, each| {
        read_in_chunks(
            store,
            &metas,
            &ids,
            time_range,
            positions,
            budget,
            &mut |i, data| {
                if from_catalog[i] && array_key(&metas[i], data)? != planned[i].key {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "series '{}' (owner {}) reads back as a different array than its catalog \
                     row describes",
                        metas[i].name, metas[i].owner_id
                    )));
                }
                each(i, data)
            },
        )
    })
}

/// Add every series in the tables an export wrote to `path` under `prefix`,
/// returning the ids `store` assigned, in the order read.
///
/// **One transaction across the whole database**, so a failure in any partition
/// leaves the store as it was. (The CLI's `add --sqlite` commits per partition
/// instead, because it can report the ones that landed; a returned id list
/// cannot.) Ids are always assigned fresh -- the ones the tables recorded are
/// not reused.
pub fn import_store(
    store: &mut Store,
    path: &Path,
    prefix: &str,
    options: &ImportOptions,
) -> Result<Vec<TimeSeriesId>> {
    let partitions = sqlite_partitions(path, prefix)?;
    store.begin_transaction()?;
    let mut ids = Vec::new();
    let read = partitions.iter().try_for_each(|partition| {
        read_sqlite_partition_with(partition, options, &mut |one| {
            ids.push(store.add(AddRequest {
                owner_id: one.owner_id,
                owner_type: one.owner_type,
                owner_category: one.owner_category,
                data: one.data,
                features: one.features,
            })?);
            Ok(())
        })
        .map(|_| ())
    });
    match read {
        Ok(()) => store.commit_transaction(),
        Err(e) => Err(e),
    }
    .map(|()| ids)
    .inspect_err(|_| {
        // A failed commit leaves the transaction open as much as a failed read
        // does, and the first error is the one worth reporting.
        let _ = store.rollback_transaction();
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use infrastore_core::{Features, OwnerCategory, SingleTimeSeries};

    #[test]
    fn a_chunked_read_visits_every_position_in_id_order_whatever_the_budget() {
        let mut store = Store::create(None, true).unwrap();
        let start = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        for owner in 0..5 {
            // Distinct values, so a read handing back the wrong series shows.
            let values = vec![f64::from(owner); 100];
            let series = SingleTimeSeries::new(
                start,
                Duration::hours(1),
                TypedArray::from_f64(vec![100], &values),
                "load",
            );
            store
                .add_time_series(
                    i64::from(owner),
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(series),
                    Features::new(),
                )
                .unwrap();
        }
        let metas = store.list_metadata(ListFilter::new()).unwrap();
        let ids: Vec<TimeSeriesId> = metas.iter().map(|m| m.id.unwrap()).collect();
        let positions = [4, 0, 3, 1, 2];
        // Each series is 800 bytes by the catalog's account: budgets that fit
        // none, one, two and all of them.
        for budget in [0, 800, 1700, usize::MAX] {
            let mut seen = Vec::new();
            read_in_chunks(
                &store,
                &metas,
                &ids,
                None,
                &positions,
                budget,
                &mut |i, data| {
                    let TimeSeriesData::SingleTimeSeries(series) = data else {
                        panic!("only SingleTimeSeries were stored");
                    };
                    assert_eq!(
                        series.data.to_vec::<f64>().unwrap()[0],
                        metas[i].owner_id as f64
                    );
                    seen.push(i);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(seen, [0, 1, 2, 3, 4], "budget {budget}");
        }
    }
}
