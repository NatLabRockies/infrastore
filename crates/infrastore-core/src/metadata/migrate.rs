//! In-place upgrades of an existing SQLite catalog.
//!
//! The DDL in [`super::schema`] is idempotent, which lets a purely additive
//! *table* or *index* land without ceremony: an older store picks it up on its
//! first writable open. Idempotent is not version-agnostic, though —
//! `CREATE TABLE IF NOT EXISTS` will not alter a table that already exists, so
//! a new *column*, a changed CHECK, or a backfill needs something that
//! deliberately rewrites what is there. That is this module.
//!
//! # The contract
//!
//! [`CATALOG_SCHEMA_REVISION`] is a monotonic integer stamped into the
//! `schema_version` table. **Any catalog change the idempotent DDL cannot make
//! to an existing table needs a `CATALOG_SCHEMA_REVISION` bump plus an
//! append-only entry in [`MIGRATIONS`].** Never edit a landed entry: stores in
//! the wild have already run it, so changing it changes nothing for them and
//! silently diverges the shape a fresh store gets from the shape an upgraded
//! one gets. Add a new entry instead.
//!
//! Revision `1` is *defined* as "whatever a pre-ladder build stamped". That is
//! exactly what every existing store already says (`MetadataStore::init` seeded
//! a literal `1` and nothing ever read it back), so no detection heuristic is
//! needed — but it is deliberately not a claim about one particular table
//! shape. Nothing stamped a revision while the catalog was still changing, so
//! `1` spans several shapes: the 0.17.0 one, and the 0.18.0 one that added
//! `AUTOINCREMENT` to the association id. A migration must therefore tolerate
//! any of them rather than assume the earliest, which is why revision 2 treats
//! a missing `sqlite_sequence` as ordinary rather than as corruption.
//!
//! In practice the reachable set is narrower still: [`crate::version`]'s
//! `MIN_UPGRADABLE_VERSION` rejects an artifact below the current
//! `DATA_FORMAT_VERSION` floor before the catalog is ever opened, so only
//! catalogs from that era climb the ladder. Do not encode that in a migration
//! either — the floor moves, and a migration outlives the release that added
//! it. The ladder starts at `1` rather than trying to resurrect the 0.12–0.16
//! formats, which changed the meaning of bytes on disk and are rejected
//! outright by the same version check.
//!
//! # Ordering
//!
//! [`apply`] runs the migrations **before** re-applying the DDL, and the order
//! is load-bearing in both directions:
//!
//! * A migration that rebuilds a table drops that table's indexes along with
//!   it. The DDL's `CREATE INDEX IF NOT EXISTS` statements then put them back.
//! * Running the DDL first would be a no-op on the un-migrated table anyway,
//!   and could fail outright resolving a column that a migration has not added
//!   yet — the same trap described in the `DDL` doc comment.
//!
//! Each migration commits in its own transaction, stamping the revision it
//! produced as it goes, so an interrupted ladder resumes from where it stopped
//! rather than re-running work it already did.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::schema;
use crate::error::{Result, TimeSeriesError};

/// Monotonic revision of the SQLite catalog shape. Bumped by any change the
/// idempotent DDL cannot make to an existing catalog: a new column, a changed
/// CHECK, a rebuilt table, a backfill. See the module docs.
pub const CATALOG_SCHEMA_REVISION: i64 = 2;

/// The revision an unstamped catalog is taken to be at — any pre-ladder build's
/// shape, not one particular release's. See the module docs for why this needs
/// no detection, and why it is not a claim about a single table shape.
const BASE_REVISION: i64 = 1;

/// One rung of the ladder.
struct Migration {
    /// The revision this migration *produces*. Entries are ordered by it and
    /// must be contiguous from [`BASE_REVISION`] + 1 to
    /// [`CATALOG_SCHEMA_REVISION`].
    revision: i64,
    /// What it does, for the `tracing` line it emits.
    description: &'static str,
    apply: fn(&Transaction<'_>) -> Result<()>,
}

/// Ordered, append-only. Never edit a landed entry; add a new one.
static MIGRATIONS: &[Migration] = &[Migration {
    revision: 2,
    description: "widen the time_series_associations time_series_type CHECK from \
                  BETWEEN 0 AND 5 to >= 0",
    apply: relax_time_series_type_check,
}];

/// The catalog shape revision 2 produces for `time_series_associations`.
///
/// A *frozen snapshot*, deliberately not built from [`schema::DDL`]: the live
/// DDL will keep moving, and a migration has to reproduce the shape it produced
/// on the day it landed or an upgraded store and a fresh store stop agreeing.
/// The comments that document each column live in the DDL; this is the bare
/// structure.
///
/// It differs from the revision-1 shape in exactly one place: the
/// `time_series_type` CHECK. See [`relax_time_series_type_check`].
///
/// `AUTOINCREMENT` is not decoration and must not be dropped from this
/// snapshot: it is what makes a deleted association's id permanent rather than
/// reissued, which is the guarantee a consumer storing an id in its own object
/// model relies on. A rebuild spelling a bare `INTEGER PRIMARY KEY` would
/// silently take that away from every store that migrated.
const REVISION_2_ASSOCIATIONS_TABLE: &str = "\
CREATE TABLE time_series_associations_new (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id          INTEGER NOT NULL,
    owner_type        TEXT    NOT NULL,
    owner_category    INTEGER NOT NULL CHECK(owner_category IN (0,1)),
    time_series_type  INTEGER NOT NULL CHECK(time_series_type >= 0),
    name              TEXT    NOT NULL,
    initial_timestamp TEXT,
    resolution        TEXT,
    length            INTEGER,
    horizon           TEXT,
    interval          TEXT,
    count             INTEGER,
    timestamps_hash   BLOB,
    units             TEXT,
    quantity_kind     TEXT,
    unit_system       TEXT,
    time_reference    TEXT,
    component_field   TEXT,
    percentiles_json  TEXT,
    element_type      TEXT    NOT NULL DEFAULT 'f64',
    element_shape     TEXT,
    application_data  TEXT,
    data_hash         BLOB    NOT NULL,
    features_hash     BLOB    NOT NULL
)";

/// Every column of `time_series_associations`, named explicitly for the
/// copy step. Never `SELECT *`: it would silently depend on column *order*
/// matching between the old table and the new one.
const REVISION_2_ASSOCIATIONS_COLUMNS: &str = "\
id, owner_id, owner_type, owner_category, time_series_type, name, \
initial_timestamp, resolution, length, horizon, interval, count, \
timestamps_hash, units, quantity_kind, unit_system, time_reference, \
component_field, percentiles_json, element_type, element_shape, \
application_data, data_hash, features_hash";

/// Revision 2: permanently relax the `time_series_type` CHECK.
///
/// The revision-1 shape wrote `CHECK(time_series_type BETWEEN 0 AND 5)`, which
/// made appending a seventh type a table rebuild. `TimeSeriesType::from_code`
/// is the real gate on that domain and runs on every write and every read, so
/// the numeric bound bought nothing SQLite had to enforce. It is replaced by a
/// non-negativity check, which still refuses a garbage value while leaving
/// every future type migration-free.
///
/// SQLite has no `ALTER TABLE … DROP CONSTRAINT`, so this is the standard
/// table rebuild. Indexes are *not* re-created here: dropping the table drops
/// them, and [`apply`] re-runs the DDL immediately afterwards, which puts back
/// exactly the current set.
///
/// The `time_series_readable` view has to go first, and this is not optional:
/// SQLite refuses to drop a table a view still names, failing the whole
/// migration with `error in view time_series_readable: no such table`. The DDL
/// pass that follows drops and re-creates the view unconditionally, so removing
/// it here loses nothing. **Any future migration that rebuilds this table needs
/// the same line.**
///
/// The `AUTOINCREMENT` high-water mark is carried across by hand. `DROP TABLE`
/// takes the old table's `sqlite_sequence` row with it, and the copy leaves the
/// new table's mark at `max(id)` — so a store whose highest-numbered row had
/// been deleted would come out of the migration ready to reissue that id, which
/// is the one thing `AUTOINCREMENT` exists to prevent. Read it before the drop,
/// restore it after the rename, and never lower it.
fn relax_time_series_type_check(tx: &Transaction<'_>) -> Result<()> {
    // `sqlite_sequence` exists only once some AUTOINCREMENT table does; a
    // catalog old enough to lack it has no mark to carry.
    let previous_seq: Option<i64> = if super::table_exists(tx, "sqlite_sequence")? {
        tx.query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'time_series_associations'",
            [],
            |row| row.get(0),
        )
        .optional()?
    } else {
        None
    };

    tx.execute_batch(REVISION_2_ASSOCIATIONS_TABLE)?;
    tx.execute_batch(&format!(
        "DROP VIEW IF EXISTS time_series_readable;
         INSERT INTO time_series_associations_new ({cols})
         SELECT {cols} FROM time_series_associations;
         DROP TABLE time_series_associations;
         ALTER TABLE time_series_associations_new RENAME TO time_series_associations;",
        cols = REVISION_2_ASSOCIATIONS_COLUMNS
    ))?;

    if let Some(seq) = previous_seq {
        // The copy seeds a row only when it moved at least one association, so
        // insert first for the empty case, then ratchet. `seq <` keeps this
        // from ever lowering a mark the copy set higher.
        tx.execute(
            "INSERT INTO sqlite_sequence (name, seq)
             SELECT 'time_series_associations', ?1
             WHERE NOT EXISTS (
                 SELECT 1 FROM sqlite_sequence WHERE name = 'time_series_associations'
             )",
            [seq],
        )?;
        tx.execute(
            "UPDATE sqlite_sequence SET seq = ?1
             WHERE name = 'time_series_associations' AND seq < ?1",
            [seq],
        )?;
    }
    Ok(())
}

/// The revision stamped in `schema_version`, or [`BASE_REVISION`] when the
/// table or its row is absent.
///
/// An absent table means a catalog written before `schema_version` existed at
/// all; an absent row means one written by a build that created the table but
/// never seeded it. Both are the 0.17.0-and-earlier shape, which is what
/// [`BASE_REVISION`] names.
pub(super) fn read_revision(conn: &Connection) -> Result<i64> {
    if !super::table_exists(conn, "schema_version")? {
        return Ok(BASE_REVISION);
    }
    Ok(conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?
        .unwrap_or(BASE_REVISION))
}

/// Stamp `revision` into `schema_version`, replacing whatever was there. The
/// table holds at most one row, so this clears it first.
///
/// Creates the table when it is missing, which is not belt-and-braces:
/// [`read_revision`] deliberately treats an absent `schema_version` as
/// [`BASE_REVISION`] so such a catalog climbs the ladder, and the DDL that
/// would create the table only runs *after* the ladder. Without this, the
/// stamp inside the first migration's transaction would fail on `no such
/// table` and take the whole writable open down with it. The statement is the
/// DDL's, verbatim.
fn write_revision(conn: &Connection, revision: i64) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")?;
    conn.execute("DELETE FROM schema_version", [])?;
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        params![revision],
    )?;
    Ok(())
}

/// Bring a **read-only** catalog's revision to account for.
///
/// Nothing can be changed on a read-only connection, so this only reports.
/// A stale catalog gets [`TimeSeriesError::CatalogMigrationRequired`], which
/// tells the caller the one thing that fixes it: open the store once for
/// writing. Before this existed, such a store failed with a raw SQLite
/// `no such column` from somewhere inside a later query.
///
/// A connection with no association table at all is left alone: there is
/// nothing to migrate and nothing to read, and reporting a migration for an
/// empty file would be a lie. The *too-new* check still runs first, though --
/// see below.
pub(super) fn check_read_only(conn: &Connection) -> Result<()> {
    // Read the stamp before looking at the table, so `CatalogTooNew` holds
    // unconditionally. No build produces a stamped catalog without the
    // association table -- `apply` creates both in one call, and the table is
    // the DDL's first statement -- so this is a guard against a damaged file
    // rather than a reachable state. But the refusal is documented as absolute,
    // and an absolute guarantee that quietly depends on a table being present
    // is the kind that fails the one time it matters.
    let found = read_revision(conn)?;
    if found > CATALOG_SCHEMA_REVISION {
        return Err(TimeSeriesError::CatalogTooNew {
            found,
            expected: CATALOG_SCHEMA_REVISION,
        });
    }
    if !super::table_exists(conn, "time_series_associations")? {
        return Ok(());
    }
    if found < CATALOG_SCHEMA_REVISION {
        return Err(TimeSeriesError::CatalogMigrationRequired {
            found,
            expected: CATALOG_SCHEMA_REVISION,
        });
    }
    Ok(())
}

/// Bring a **writable** catalog up to [`CATALOG_SCHEMA_REVISION`], creating it
/// if it is empty. Returns how many migrations actually ran — zero for a fresh
/// catalog and for one already at the current revision.
///
/// See the module docs for why the migrations run before the DDL.
pub(super) fn apply(conn: &Connection) -> Result<usize> {
    // Same order as `check_read_only`, and for the same reason: a catalog from
    // a newer build is refused before this one writes anything to it, table or
    // no table.
    let found = read_revision(conn)?;
    if found > CATALOG_SCHEMA_REVISION {
        return Err(TimeSeriesError::CatalogTooNew {
            found,
            expected: CATALOG_SCHEMA_REVISION,
        });
    }

    // A catalog with no association table has never been written to. Creating
    // it from the current DDL *is* the current revision, so there is no ladder
    // to climb: stamp it and stop.
    if !super::table_exists(conn, "time_series_associations")? {
        conn.execute_batch(schema::DDL)?;
        write_revision(conn, CATALOG_SCHEMA_REVISION)?;
        return Ok(0);
    }

    let mut ran = 0;
    if found < CATALOG_SCHEMA_REVISION {
        // Outside the transaction, and it has to be: SQLite silently ignores
        // this pragma inside one, so setting it after `BEGIN` would leave
        // foreign keys enforced during the rebuild without saying so. There
        // are no foreign keys in this schema today; the discipline is here for
        // the migration that adds one.
        conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let result = run_ladder(conn, found, &mut ran);
        // Restore the connection's normal setting whether or not the ladder
        // succeeded — `MetadataStore::init` turned it on and the rest of the
        // session expects it on.
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        result?;
    }

    // Additive catch-up: new tables, new indexes (including the ones a rebuild
    // above dropped with its table), and the `time_series_readable` view, which
    // the DDL drops and re-creates so a stale definition cannot survive.
    conn.execute_batch(schema::DDL)?;
    write_revision(conn, CATALOG_SCHEMA_REVISION)?;
    Ok(ran)
}

/// Run every migration above `found`, in order, each in its own transaction.
fn run_ladder(conn: &Connection, found: i64, ran: &mut usize) -> Result<()> {
    for migration in MIGRATIONS.iter().filter(|m| m.revision > found) {
        tracing::info!(
            revision = migration.revision,
            description = migration.description,
            "applying catalog migration"
        );
        // rusqlite's `Transaction` needs `&mut Connection`; this module only
        // ever holds `&Connection` (the caller's), so the transaction is driven
        // by hand. `unchecked_transaction` gives the same RAII rollback-on-drop
        // guarantee without the exclusive borrow.
        let tx = conn.unchecked_transaction()?;
        (migration.apply)(&tx)?;
        // Stamp inside the same transaction: a revision that outlived its
        // migration, or a migration that outlived its revision, is exactly the
        // inconsistency this ladder exists to prevent.
        write_revision(&tx, migration.revision)?;
        // A rebuild that stranded a reference would be far worse than a failed
        // migration, so verify before committing rather than after.
        let violations: i64 =
            tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if violations > 0 {
            return Err(TimeSeriesError::IntegrityError(format!(
                "catalog migration to revision {} left {violations} foreign-key \
                 violations behind; the catalog was not modified",
                migration.revision
            )));
        }
        tx.commit()?;
        *ran += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A revision-1 shape of the table the ladder rebuilds, for tests that need
    /// a catalog to migrate. Identical to [`REVISION_2_ASSOCIATIONS_TABLE`] but
    /// for the CHECK and the name.
    ///
    /// *A* shape, not *the* shape: revision 1 is whatever a pre-ladder build
    /// stamped, and those builds disagreed about the id column (see the module
    /// docs). This is the later spelling, with `AUTOINCREMENT`, because that is
    /// the one a mark can be carried across — the case the test below exists
    /// for. `tests/migration.rs` drives the earlier, bare `INTEGER PRIMARY KEY`
    /// spelling end to end; between them both are covered.
    const REVISION_1_ASSOCIATIONS_TABLE: &str = "\
CREATE TABLE time_series_associations (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id          INTEGER NOT NULL,
    owner_type        TEXT    NOT NULL,
    owner_category    INTEGER NOT NULL CHECK(owner_category IN (0,1)),
    time_series_type  INTEGER NOT NULL CHECK(time_series_type BETWEEN 0 AND 5),
    name              TEXT    NOT NULL,
    initial_timestamp TEXT,
    resolution        TEXT,
    length            INTEGER,
    horizon           TEXT,
    interval          TEXT,
    count             INTEGER,
    timestamps_hash   BLOB,
    units             TEXT,
    quantity_kind     TEXT,
    unit_system       TEXT,
    time_reference    TEXT,
    component_field   TEXT,
    percentiles_json  TEXT,
    element_type      TEXT    NOT NULL DEFAULT 'f64',
    element_shape     TEXT,
    application_data  TEXT,
    data_hash         BLOB    NOT NULL,
    features_hash     BLOB    NOT NULL
)";

    /// A catalog at revision 1 holding one association row — **including the
    /// `time_series_readable` view**, which a real store always has.
    ///
    /// The view is not decoration here. SQLite refuses to drop a table a view
    /// still names, so a fixture without one lets a table-rebuild migration
    /// pass in tests and fail against every real store. That is exactly what
    /// happened to revision 2 before this fixture grew the view.
    fn revision_1_catalog() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch(REVISION_1_ASSOCIATIONS_TABLE)
            .expect("revision-1 table");
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES (1);
             INSERT INTO time_series_associations
                 (owner_id, owner_type, owner_category, time_series_type, name,
                  element_type, data_hash, features_hash)
             VALUES (7, 'Generator', 0, 1, 'load', 'f64', X'00', X'01');
             CREATE INDEX idx_ts_type ON time_series_associations(time_series_type);
             CREATE VIEW time_series_readable AS
                 SELECT id, name, time_series_type FROM time_series_associations;",
        )
        .expect("seed row, index, and view");
        conn
    }

    /// A rebuild must not quietly hand back an id the store has already issued.
    ///
    /// Two halves, and both have to hold: the rebuilt table still declares
    /// `AUTOINCREMENT`, and the mark it counts from survives `DROP TABLE` —
    /// including the case that makes the guarantee visible, where the
    /// highest-numbered row was deleted before the migration ran and `max(id)`
    /// alone would rewind the counter.
    #[test]
    fn the_rebuild_keeps_autoincrement_and_its_high_water_mark() {
        let conn = revision_1_catalog();
        // Add a second row and delete it: the mark is now 2, the rows go up to
        // 1, and a bare `INTEGER PRIMARY KEY` would reissue 2 next.
        conn.execute_batch(
            "INSERT INTO time_series_associations
                 (owner_id, owner_type, owner_category, time_series_type, name,
                  element_type, data_hash, features_hash)
             VALUES (8, 'Generator', 0, 1, 'wind', 'f64', X'02', X'03');
             DELETE FROM time_series_associations WHERE owner_id = 8;",
        )
        .expect("seed and delete the high row");
        let mark_before: i64 = conn
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'time_series_associations'",
                [],
                |r| r.get(0),
            )
            .expect("a revision-1 catalog keeps a mark");
        assert_eq!(mark_before, 2);

        assert_eq!(apply(&conn).expect("apply"), 1);

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'time_series_associations'",
                [],
                |r| r.get(0),
            )
            .expect("table sql");
        assert!(sql.contains("AUTOINCREMENT"), "{sql}");
        let mark_after: i64 = conn
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'time_series_associations'",
                [],
                |r| r.get(0),
            )
            .expect("the mark survives the rebuild");
        assert_eq!(mark_after, mark_before);

        // The next insert proves it end to end: id 2 is retired for good.
        conn.execute_batch(
            "INSERT INTO time_series_associations
                 (owner_id, owner_type, owner_category, time_series_type, name,
                  element_type, data_hash, features_hash)
             VALUES (9, 'Generator', 0, 6, 'gas_price', 'f64', X'04', X'05');",
        )
        .expect("insert after the migration");
        let next: i64 = conn
            .query_row(
                "SELECT id FROM time_series_associations WHERE owner_id = 9",
                [],
                |r| r.get(0),
            )
            .expect("the new row");
        assert_eq!(next, 3);
    }

    #[test]
    fn the_rebuild_survives_the_view_and_indexes_that_name_the_table() {
        let conn = revision_1_catalog();
        assert_eq!(apply(&conn).expect("apply"), 1);
        // The DDL pass after the ladder puts both back, with current
        // definitions rather than the fixture's stand-ins.
        let objects: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE name IN ('time_series_readable', 'idx_ts_type')")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<std::result::Result<_, _>>()
            .expect("rows");
        assert_eq!(objects.len(), 2, "got {objects:?}");
        // And it is the *current* view, not the fixture's three-column
        // stand-in: only the real one spells the type names out.
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'time_series_readable'",
                [],
                |r| r.get(0),
            )
            .expect("view sql");
        assert!(sql.contains("Scenarios"), "{sql}");
    }

    #[test]
    fn migrations_are_ordered_and_contiguous() {
        let expected: Vec<i64> = (BASE_REVISION + 1..=CATALOG_SCHEMA_REVISION).collect();
        let found: Vec<i64> = MIGRATIONS.iter().map(|m| m.revision).collect();
        assert_eq!(
            found, expected,
            "MIGRATIONS must cover every revision above the base, in ascending order"
        );
    }

    #[test]
    fn a_fresh_catalog_stamps_the_current_revision_and_runs_no_migration() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        assert_eq!(apply(&conn).expect("apply"), 0);
        assert_eq!(
            read_revision(&conn).expect("revision"),
            CATALOG_SCHEMA_REVISION
        );
    }

    #[test]
    fn revision_1_upgrades_in_place_preserving_rows() {
        let conn = revision_1_catalog();
        // The old CHECK is in force before the migration.
        assert!(
            conn.execute(
                "INSERT INTO time_series_associations
                     (owner_id, owner_type, owner_category, time_series_type, name,
                      element_type, data_hash, features_hash)
                 VALUES (8, 'Generator', 0, 6, 'fuel', 'f64', X'02', X'01')",
                [],
            )
            .is_err(),
            "revision 1 must refuse a code-6 row"
        );

        assert_eq!(apply(&conn).expect("apply"), 1);
        assert_eq!(
            read_revision(&conn).expect("revision"),
            CATALOG_SCHEMA_REVISION
        );

        // The pre-existing row survived, unchanged.
        let (owner, name, code): (i64, String, i64) = conn
            .query_row(
                "SELECT owner_id, name, time_series_type FROM time_series_associations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("surviving row");
        assert_eq!((owner, name.as_str(), code), (7, "load", 1));

        // And a code the old bound refused is now insertable.
        conn.execute(
            "INSERT INTO time_series_associations
                 (owner_id, owner_type, owner_category, time_series_type, name,
                  element_type, data_hash, features_hash)
             VALUES (8, 'Generator', 0, 6, 'fuel', 'f64', X'02', X'01')",
            [],
        )
        .expect("revision 2 accepts a code above the old upper bound");
    }

    #[test]
    fn the_relaxed_check_still_refuses_a_negative_code() {
        let conn = revision_1_catalog();
        apply(&conn).expect("apply");
        assert!(
            conn.execute(
                "INSERT INTO time_series_associations
                     (owner_id, owner_type, owner_category, time_series_type, name,
                      element_type, data_hash, features_hash)
                 VALUES (9, 'Generator', 0, -1, 'bad', 'f64', X'03', X'01')",
                [],
            )
            .is_err(),
            "a negative type code is still garbage and must be refused"
        );
    }

    #[test]
    fn a_second_apply_runs_no_migration() {
        let conn = revision_1_catalog();
        assert_eq!(apply(&conn).expect("first apply"), 1);
        assert_eq!(apply(&conn).expect("second apply"), 0);
        assert_eq!(
            read_revision(&conn).expect("revision"),
            CATALOG_SCHEMA_REVISION
        );
    }

    #[test]
    fn a_newer_catalog_is_refused_by_both_entry_points() {
        let conn = revision_1_catalog();
        write_revision(&conn, CATALOG_SCHEMA_REVISION + 1).expect("stamp a future revision");
        assert!(matches!(
            apply(&conn),
            Err(TimeSeriesError::CatalogTooNew { .. })
        ));
        assert!(matches!(
            check_read_only(&conn),
            Err(TimeSeriesError::CatalogTooNew { .. })
        ));
    }

    #[test]
    fn a_stale_catalog_reports_migration_required_read_only() {
        let conn = revision_1_catalog();
        match check_read_only(&conn) {
            Err(TimeSeriesError::CatalogMigrationRequired { found, expected }) => {
                assert_eq!((found, expected), (1, CATALOG_SCHEMA_REVISION));
            }
            other => panic!("expected CatalogMigrationRequired, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_catalog_is_readable_read_only() {
        // Nothing to migrate and nothing to read: reporting a migration here
        // would be a lie about a file that simply has no tables yet.
        let conn = Connection::open_in_memory().expect("in-memory database");
        check_read_only(&conn).expect("an empty catalog reports nothing");
    }

    #[test]
    fn an_unstamped_catalog_reads_as_the_base_revision() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch(REVISION_1_ASSOCIATIONS_TABLE)
            .expect("revision-1 table");
        assert_eq!(read_revision(&conn).expect("revision"), BASE_REVISION);
    }

    /// And it must then *climb* — the case `read_revision`'s tolerance exists
    /// for. The ladder stamps each rung as it goes, and the DDL that creates
    /// `schema_version` runs only after it, so `write_revision` has to make the
    /// table itself or the first migration takes the whole open down with a raw
    /// `no such table: schema_version`.
    #[test]
    fn an_unstamped_catalog_climbs_the_ladder() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch(REVISION_1_ASSOCIATIONS_TABLE)
            .expect("revision-1 table");
        conn.execute_batch(
            "INSERT INTO time_series_associations
                 (owner_id, owner_type, owner_category, time_series_type, name,
                  element_type, data_hash, features_hash)
             VALUES (7, 'Generator', 0, 1, 'load', 'f64', X'00', X'01');",
        )
        .expect("seed row");

        assert_eq!(apply(&conn).expect("apply"), 1);
        assert_eq!(
            read_revision(&conn).expect("revision"),
            CATALOG_SCHEMA_REVISION
        );
        // The row survived, and the relaxed CHECK is in force.
        let owner: i64 = conn
            .query_row("SELECT owner_id FROM time_series_associations", [], |r| {
                r.get(0)
            })
            .expect("surviving row");
        assert_eq!(owner, 7);
        conn.execute(
            "INSERT INTO time_series_associations
                 (owner_id, owner_type, owner_category, time_series_type, name,
                  element_type, data_hash, features_hash)
             VALUES (8, 'Generator', 0, 6, 'fuel', 'f64', X'02', X'01')",
            [],
        )
        .expect("revision 2 accepts a code above the old upper bound");
    }
}
