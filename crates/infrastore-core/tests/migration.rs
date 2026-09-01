//! In-place catalog migration: the ladder in `infrastore_core::metadata::migrate`
//! seen from outside, through a real two-half artifact on disk.
//!
//! These tests build a current store and then *backdate* it to the released
//! 0.17.0 shape — the `data_format_version` attribute on the HDF5 half, and on
//! the catalog side both the `schema_version` stamp and the `BETWEEN 0 AND 5`
//! CHECK the rebuild exists to remove. That is a truer fixture than a store
//! merely wearing an old label: re-opening it is what the ladder actually has
//! to survive.

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    Features, NonSequentialTimeSeries, OwnerCategory, SingleTimeSeries, TimeSeriesData,
    TimeSeriesError, TimeSeriesType, TypedArray, create_store, open_store,
};

/// The revision-1 shape of `time_series_associations`, verbatim as 0.17.0
/// wrote it. Only the `time_series_type` CHECK differs from the current one.
const REVISION_1_TABLE: &str = "\
CREATE TABLE time_series_associations_old (
    id                INTEGER PRIMARY KEY,
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

const ALL_COLUMNS: &str = "\
id, owner_id, owner_type, owner_category, time_series_type, name, \
initial_timestamp, resolution, length, horizon, interval, count, \
timestamps_hash, units, quantity_kind, unit_system, time_reference, \
component_field, percentiles_json, element_type, element_shape, \
application_data, data_hash, features_hash";

fn sqlite_path_of(h5: &std::path::Path) -> std::path::PathBuf {
    let mut p = h5.as_os_str().to_owned();
    p.push(".sqlite");
    std::path::PathBuf::from(p)
}

fn set_format_attr(path: &std::path::Path, value: &str) {
    use std::str::FromStr;
    let f = hdf5_metno::File::open_rw(path).unwrap();
    f.attr("data_format_version")
        .expect("attr present")
        .write_scalar(&hdf5_metno::types::VarLenUnicode::from_str(value).unwrap())
        .unwrap();
}

fn single(base: f64) -> SingleTimeSeries {
    let initial_timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let values: Vec<f64> = (0..24).map(|i| base + i as f64).collect();
    SingleTimeSeries::new(
        initial_timestamp,
        Duration::hours(1),
        TypedArray::from_f64(vec![24], &values),
        "load",
    )
}

fn irregular() -> NonSequentialTimeSeries {
    let timestamps: Vec<_> = [0i64, 5, 11, 40]
        .iter()
        .map(|h| Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap() + Duration::hours(*h))
        .collect();
    NonSequentialTimeSeries::new(
        timestamps,
        TypedArray::from_f64(vec![4], &[1.0, 2.0, 3.0, 4.0]),
        "events",
    )
    .unwrap()
}

/// Build a store holding one series of each static type, then backdate both
/// halves to the released 0.17.0 shape. Returns the tempdir (which must be kept
/// alive) and the store path.
fn stale_store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(single(100.0)).with_units("MW"),
                Features::new(),
            )
            .unwrap();
        store
            .add_time_series(
                2,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::NonSequentialTimeSeries(irregular()),
                Features::new(),
            )
            .unwrap();
        store.flush().unwrap();
    }

    // This backdates *both* halves, which it could not do before 0.20.0: the
    // two version constants have separated, so `MIN_UPGRADABLE_VERSION` now
    // stamps the HDF5 half `Compat::Upgradable` rather than `Compat::Current`.
    // A writable open therefore has two things to do here -- climb the catalog
    // ladder and re-stamp the array half -- and the deferred re-stamp finally
    // has a reachable input instead of only the explicit-window coverage in
    // `storage::hdf5`'s own tests.
    set_format_attr(&path, infrastore_core::MIN_UPGRADABLE_VERSION);
    {
        let conn = rusqlite::Connection::open(sqlite_path_of(&path)).unwrap();
        // Rebuild the table with the old CHECK, preserving every row, and
        // restore the 0.17.0 revision stamp. The view has to go first: SQLite
        // refuses to drop a table a view still names. Indexes go with the
        // table; the DDL puts them back on the next writable open, which is
        // itself part of what these tests exercise.
        conn.execute_batch(REVISION_1_TABLE).unwrap();
        conn.execute_batch(&format!(
            "DROP VIEW IF EXISTS time_series_readable;
             INSERT INTO time_series_associations_old ({ALL_COLUMNS})
                 SELECT {ALL_COLUMNS} FROM time_series_associations;
             DROP TABLE time_series_associations;
             ALTER TABLE time_series_associations_old RENAME TO time_series_associations;
             DELETE FROM schema_version;
             INSERT INTO schema_version (version) VALUES (1);"
        ))
        .unwrap();
    }
    (dir, path)
}

/// The revision the catalog file at `path` stamps, read directly rather than
/// through a `Store` — the point being to observe the file without upgrading it.
fn revision_on_disk(path: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(sqlite_path_of(path)).unwrap();
    conn.query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
        r.get(0)
    })
    .unwrap()
}

fn format_version_on_disk(path: &std::path::Path) -> String {
    let f = hdf5_metno::File::open(path).unwrap();
    f.attr("data_format_version")
        .unwrap()
        .read_scalar::<hdf5_metno::types::VarLenUnicode>()
        .unwrap()
        .to_string()
}

fn generation_stamps(path: &std::path::Path) -> (Option<String>, Option<String>) {
    let h5 = {
        let f = hdf5_metno::File::open(path).unwrap();
        f.attr("catalog_generation")
            .ok()
            .and_then(|a| a.read_scalar::<hdf5_metno::types::VarLenUnicode>().ok())
            .map(|v| v.to_string())
    };
    let conn = rusqlite::Connection::open(sqlite_path_of(path)).unwrap();
    let sqlite = conn
        .query_row("SELECT generation FROM catalog_identity LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .ok();
    (h5, sqlite)
}

#[test]
fn fresh_store_stamps_the_current_revision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let store = create_store(Some(path.as_path()), false).unwrap();
    assert_eq!(store.catalog_schema_revision().unwrap(), 2);
}

#[test]
fn a_revision_1_catalog_upgrades_in_place_on_a_writable_open() {
    let (_dir, path) = stale_store();
    assert_eq!(revision_on_disk(&path), 1);
    let before = generation_stamps(&path);
    let format_before = format_version_on_disk(&path);

    let mut store = open_store(path.as_path(), false).unwrap();
    assert_eq!(store.catalog_schema_revision().unwrap(), 2);

    // Both pre-existing series survived, byte for byte.
    let rows = store
        .list_metadata(infrastore_core::ListFilter::new())
        .unwrap();
    assert_eq!(rows.len(), 2);
    let id_of = |owner_id: i64| {
        rows.iter()
            .find(|r| r.owner_id == owner_id)
            .unwrap()
            .id
            .unwrap()
    };
    let single_back = store
        .read_by_id(id_of(1), infrastore_core::ReadWindow::full())
        .unwrap();
    assert_eq!(
        single_back
            .as_single()
            .unwrap()
            .data
            .to_vec::<f64>()
            .unwrap(),
        single(100.0).data.to_vec::<f64>().unwrap()
    );
    let irregular_back = store
        .read_by_id(id_of(2), infrastore_core::ReadWindow::full())
        .unwrap();
    assert_eq!(
        irregular_back.as_non_sequential().unwrap().timestamps,
        irregular().timestamps
    );

    // A code-6 row is now insertable: the CHECK the rebuild removed was the
    // one thing standing between this store and the new type.
    store
        .add_time_series(
            3,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::PersistentTimeSeries(persistent()),
            Features::new(),
        )
        .unwrap();
    drop(store);

    // Both halves moved. The catalog climbed the ladder, and the array half was
    // re-stamped from the upgradable floor to the current version on the same
    // writable open -- which is the whole point of the 0.20.0 bump: a store
    // that now holds a code-6 row no longer claims a stamp a 0.19.0 reader
    // would accept.
    assert_eq!(format_before, infrastore_core::MIN_UPGRADABLE_VERSION);
    assert_eq!(
        format_version_on_disk(&path),
        infrastore_core::DATA_FORMAT_VERSION
    );
    assert_eq!(revision_on_disk(&path), 2);
    // Migration is not a save: the paired generation stamps are untouched.
    assert_eq!(generation_stamps(&path), before);
}

fn persistent() -> infrastore_core::PersistentTimeSeries {
    let timestamps: Vec<_> = (0..3)
        .map(|m| Utc.with_ymd_and_hms(2024, 1 + m, 1, 0, 0, 0).unwrap())
        .collect();
    infrastore_core::PersistentTimeSeries::new(
        timestamps,
        TypedArray::from_f64(vec![3], &[3.5, 4.25, 5.0]),
        "gas_price",
    )
    .unwrap()
}

#[test]
fn a_read_only_open_of_a_stale_catalog_reports_migration_required() {
    let (_dir, path) = stale_store();
    match open_store(path.as_path(), true) {
        Err(TimeSeriesError::CatalogMigrationRequired { found, expected }) => {
            assert_eq!((found, expected), (1, 2));
            // The message has to name the remedy, not just the numbers: this is
            // the error a read-only consumer (the gRPC server) now surfaces.
            let rendered =
                TimeSeriesError::CatalogMigrationRequired { found, expected }.to_string();
            assert!(
                rendered.contains("writing"),
                "the message must say the store upgrades on a writable open: {rendered}"
            );
        }
        Err(other) => panic!("expected CatalogMigrationRequired, got {other:?}"),
        Ok(_) => panic!("expected a stale catalog to be refused read-only"),
    }
    // Refused, and left exactly as it was.
    assert_eq!(revision_on_disk(&path), 1);
    assert_eq!(
        format_version_on_disk(&path),
        infrastore_core::MIN_UPGRADABLE_VERSION
    );
}

/// A stale catalog must still be *copyable*. `open_copy` opens the source
/// read-only only to `VACUUM INTO` the destination, and running the read-only
/// schema check on that connection made a stale store impossible to copy —
/// which is backwards, since taking a writable copy is precisely how a consumer
/// migrates one without touching the user's artifact. Both shipped consumers
/// (infrasys, IS3.jl) depend on this path.
#[test]
fn a_stale_catalog_can_still_be_copied_and_the_copy_migrates() {
    let (dir, path) = stale_store();
    let dest = dir.path().join("scratch.h5");

    let copy = infrastore_core::open_store_copy(
        path.as_path(),
        dest.as_path(),
        infrastore_core::CatalogMode::Attached,
    )
    .expect("a stale catalog must still be copyable");
    // The copy is writable, so it climbed the ladder on the way in, and carries
    // every row the source had.
    assert_eq!(copy.catalog_schema_revision().unwrap(), 2);
    assert_eq!(
        copy.list_metadata(infrastore_core::ListFilter::new())
            .unwrap()
            .len(),
        2
    );
    drop(copy);

    // And the original is untouched — still stale, still refused read-only.
    assert_eq!(revision_on_disk(&path), 1);
}

#[test]
fn a_migrated_store_reopens_without_migrating_again() {
    let (_dir, path) = stale_store();
    {
        let store = open_store(path.as_path(), false).unwrap();
        assert_eq!(store.catalog_schema_revision().unwrap(), 2);
    }
    // A second writable open finds nothing to do, and a read-only open — which
    // could not migrate even if there were — now succeeds.
    {
        let store = open_store(path.as_path(), false).unwrap();
        assert_eq!(store.catalog_schema_revision().unwrap(), 2);
    }
    let store = open_store(path.as_path(), true).unwrap();
    assert_eq!(store.catalog_schema_revision().unwrap(), 2);
    assert_eq!(
        store
            .list_metadata(infrastore_core::ListFilter::new())
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_catalog_from_a_newer_build_is_refused_rather_than_downgraded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        create_store(Some(path.as_path()), false)
            .unwrap()
            .flush()
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(sqlite_path_of(&path)).unwrap();
        conn.execute_batch(
            "DELETE FROM schema_version; INSERT INTO schema_version (version) VALUES (99);",
        )
        .unwrap();
    }
    for read_only in [false, true] {
        match open_store(path.as_path(), read_only) {
            Err(TimeSeriesError::CatalogTooNew { found, expected }) => {
                assert_eq!((found, expected), (99, 2));
            }
            Err(other) => panic!("expected CatalogTooNew (read_only={read_only}), got {other:?}"),
            Ok(_) => {
                panic!("expected a future-revision catalog to be refused (read_only={read_only})")
            }
        }
    }
}

#[test]
fn a_store_older_than_the_upgrade_floor_is_still_rejected_outright() {
    let (_dir, path) = stale_store();
    set_format_attr(&path, "0.16.0");
    match open_store(path.as_path(), false) {
        Err(TimeSeriesError::IncompatibleFormat { found, expected }) => {
            assert_eq!(found, "0.16.0");
            assert_eq!(expected, infrastore_core::DATA_FORMAT_VERSION);
        }
        Err(other) => panic!("expected IncompatibleFormat, got {other:?}"),
        Ok(_) => panic!("expected a pre-floor store to be rejected"),
    }
}

/// A mismatched pair is refused **before** either half is written to.
///
/// Opening the catalog runs the ladder and a successful one re-stamps the HDF5
/// half, so checking the generation stamps afterwards means mutating two of a
/// user's files in order to tell them the files never belonged together. The
/// stale catalog here would migrate happily if it were ever opened as a
/// catalog; the point of the test is that it is not.
#[test]
fn a_mismatched_pair_is_refused_before_anything_migrates() {
    let (_dir, path) = stale_store();
    assert_eq!(revision_on_disk(&path), 1);

    // Give the catalog half a generation stamp the array half does not share,
    // which is what an interrupted `persist_to` or a hand-swapped file looks
    // like from the outside.
    {
        let conn = rusqlite::Connection::open(sqlite_path_of(&path)).unwrap();
        conn.execute(
            "UPDATE catalog_identity SET generation = 'not-the-h5s-generation'",
            [],
        )
        .unwrap();
    }

    match open_store(path.as_path(), false) {
        Err(TimeSeriesError::MismatchedArtifact { .. }) => {}
        Err(other) => panic!("expected MismatchedArtifact, got {other:?}"),
        Ok(_) => panic!("a mismatched pair must not open"),
    }

    // The catalog is still at revision 1: the ladder never ran. Before the
    // preflight this assertion read 2, because the rejected artifact had
    // already been rebuilt.
    assert_eq!(revision_on_disk(&path), 1);
}

/// Relaxing the CHECK makes a type code the view does not name *reachable* for
/// the first time. The view has to render such a row rather than fail on it —
/// otherwise the next type to land breaks hand inspection of every store that
/// has one until the `CASE` arm catches up.
///
/// The code here is deliberately far above the next one a type will claim, so
/// that landing a type does not quietly turn this into a test of nothing.
#[test]
fn the_readable_view_renders_a_code_it_does_not_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(single(1.0)),
                Features::new(),
            )
            .unwrap();
        store.flush().unwrap();
    }
    let conn = rusqlite::Connection::open(sqlite_path_of(&path)).unwrap();
    conn.execute(
        "INSERT INTO time_series_associations
             (owner_id, owner_type, owner_category, time_series_type, name,
              element_type, data_hash, features_hash)
         VALUES (2, 'Generator', 0, 99, 'future', 'f64', X'02', X'01')",
        [],
    )
    .unwrap();
    let mut named: Vec<String> = conn
        .prepare("SELECT time_series_type FROM time_series_readable ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    named.sort();
    assert_eq!(
        named,
        vec![
            TimeSeriesType::SingleTimeSeries.as_str().to_string(),
            "unknown(99)".to_string(),
        ]
    );
}

/// The other half: a code the view *does* name renders as its type name. Added
/// with `PersistentTimeSeries`, the first type to reach the catalog through the
/// widened CHECK rather than through a format bump.
#[test]
fn the_readable_view_names_the_new_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::PersistentTimeSeries(persistent()),
                Features::new(),
            )
            .unwrap();
        store.flush().unwrap();
    }
    let conn = rusqlite::Connection::open(sqlite_path_of(&path)).unwrap();
    let rendered: String = conn
        .query_row(
            "SELECT time_series_type FROM time_series_readable",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rendered, TimeSeriesType::PersistentTimeSeries.as_str());
}
