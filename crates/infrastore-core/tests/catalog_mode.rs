//! Catalog placement ([`CatalogMode`]) and the paired generation stamp.
//!
//! Two workloads share one artifact format. An *attached* catalog is the
//! `<store>.sqlite` file and every commit is durable; an *in-memory* catalog
//! lives in RAM and reaches disk only through `persist_to`, which suits a
//! consumer building a store in a scratch directory beside its own volatile
//! state. The stamp is what keeps the two halves of a save honest — see
//! `Store::persist_to`.

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    CatalogMode, Compression, Features, ListFilter, OwnerCategory, SingleTimeSeries,
    TimeSeriesData, TimeSeriesError, TypedArray, catalog_sqlite_path, create_store,
    create_store_with_catalog, open_store, open_store_with_catalog,
};

fn series(base: f64) -> SingleTimeSeries {
    let initial_timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let values: Vec<f64> = (0..24).map(|i| base + i as f64).collect();
    SingleTimeSeries::new(
        initial_timestamp,
        Duration::hours(1),
        TypedArray::from_f64(vec![24], &values),
        "load",
    )
}

fn add(store: &mut infrastore_core::Store, owner: i64, base: f64) {
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(base)),
            Features::new(),
        )
        .unwrap();
}

/// The one `SingleTimeSeries` values vector stored for `owner`.
fn read_values(store: &infrastore_core::Store, owner: i64) -> Vec<f64> {
    let keys = store.list_keys(ListFilter::new().owner_id(owner)).unwrap();
    let key = keys.first().expect("a key for this owner");
    let TimeSeriesData::SingleTimeSeries(s) = store.get_time_series(key.identity(), None).unwrap()
    else {
        panic!("expected a SingleTimeSeries");
    };
    s.data.to_f64_vec().unwrap()
}

fn keys_for(store: &infrastore_core::Store, owner: i64) -> usize {
    store
        .list_keys(ListFilter::new().owner_id(owner))
        .unwrap()
        .len()
}

fn scratch_store(path: &std::path::Path) -> infrastore_core::Store {
    create_store_with_catalog(
        Some(path),
        false,
        Compression::default(),
        CatalogMode::InMemory,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Catalog placement
// ---------------------------------------------------------------------------

#[test]
fn an_in_memory_catalog_writes_no_sqlite_file_until_persist() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");

    let mut store = scratch_store(&scratch);
    add(&mut store, 1, 100.0);
    store.flush().unwrap();

    assert!(scratch.exists(), "arrays still stream to the HDF5 file");
    assert!(
        !catalog_sqlite_path(&scratch).exists(),
        "an in-memory catalog must not create a sidecar; nothing is durable until persist_to"
    );

    let dest = dir.path().join("saved.h5");
    store.persist_to(&dest).unwrap();
    assert!(
        catalog_sqlite_path(&dest).exists(),
        "persist writes the pair"
    );
}

#[test]
fn an_in_memory_catalog_discards_changes_when_the_store_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");

    {
        let mut store = scratch_store(&scratch);
        add(&mut store, 1, 100.0);
        store.flush().unwrap();
    }

    // Mode-1 semantics: the arrays are on disk but nothing names them. Reopening
    // attached creates a fresh, unstamped catalog beside a stamped HDF5 file, and
    // the paired-stamp check now refuses that rather than presenting the arrays
    // as an empty store — an abandoned scratch half-artifact is exactly the case
    // where "opens fine, contains nothing" is the wrong answer.
    let err = open_store(&scratch, false)
        .err()
        .expect("a half-artifact must not open");
    assert!(
        matches!(err, TimeSeriesError::MismatchedArtifact { .. }),
        "expected MismatchedArtifact, got {err:?}"
    );
}

#[test]
fn an_attached_catalog_is_durable_without_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");

    {
        let mut store = create_store(Some(&path), false).unwrap();
        add(&mut store, 1, 100.0);
        store.flush().unwrap();
    }

    let store = open_store(&path, true).unwrap();
    assert_eq!(read_values(&store, 1).len(), 24);
}

#[test]
fn an_in_memory_backend_rejects_an_attached_catalog() {
    let err = create_store_with_catalog(None, true, Compression::default(), CatalogMode::Attached)
        .err()
        .expect("there is no file for an attached catalog to sit beside");
    assert!(
        matches!(err, TimeSeriesError::InvalidParameter(_)),
        "expected InvalidParameter, got {err:?}"
    );
}

#[test]
fn opening_with_an_in_memory_catalog_requires_the_catalog_file() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    {
        let mut store = scratch_store(&scratch);
        add(&mut store, 1, 100.0);
        store.flush().unwrap();
    }

    // Unlike an attached open, which creates an empty catalog when one is
    // missing, loading into memory has nothing to read.
    assert!(
        open_store_with_catalog(&scratch, false, CatalogMode::InMemory).is_err(),
        "no catalog file to load"
    );
}

// ---------------------------------------------------------------------------
// The mode-1 round trip
// ---------------------------------------------------------------------------

#[test]
fn a_scratch_store_persists_and_reopens_with_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    let dest = dir.path().join("system.h5");

    {
        let mut store = scratch_store(&scratch);
        add(&mut store, 1, 100.0);
        add(&mut store, 2, 200.0);
        store.persist_to(&dest).unwrap();
    }

    let store = open_store(&dest, true).unwrap();
    assert_eq!(read_values(&store, 1)[0], 100.0);
    assert_eq!(read_values(&store, 2)[0], 200.0);
}

#[test]
fn a_saved_store_loads_into_memory_and_saves_again() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.h5");
    let second = dir.path().join("second.h5");

    {
        let mut store = create_store(Some(&first), false).unwrap();
        add(&mut store, 1, 100.0);
        store.flush().unwrap();
    }

    // Load the pair into RAM, mutate, and save elsewhere.
    {
        let mut store = open_store_with_catalog(&first, false, CatalogMode::InMemory).unwrap();
        assert_eq!(store.catalog_mode(), CatalogMode::InMemory);
        add(&mut store, 2, 200.0);
        store.persist_to(&second).unwrap();
    }

    let saved = open_store(&second, true).unwrap();
    assert_eq!(read_values(&saved, 1)[0], 100.0);
    assert_eq!(read_values(&saved, 2)[0], 200.0);
}

#[test]
fn persisting_twice_to_one_destination_replaces_the_pair() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    let dest = dir.path().join("system.h5");

    let mut store = scratch_store(&scratch);
    add(&mut store, 1, 100.0);
    store.persist_to(&dest).unwrap();

    add(&mut store, 2, 200.0);
    store.persist_to(&dest).unwrap();

    // The second save must not leave the first save's catalog beside the second
    // save's arrays — that is exactly the pairing the stamp guards.
    let saved = open_store(&dest, true).unwrap();
    assert_eq!(read_values(&saved, 1)[0], 100.0);
    assert_eq!(read_values(&saved, 2)[0], 200.0);
}

#[test]
fn persist_leaves_no_temporary_files_behind() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    let dest = dir.path().join("system.h5");

    let mut store = scratch_store(&scratch);
    add(&mut store, 1, 100.0);
    store.persist_to(&dest).unwrap();

    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".persist"))
        .collect();
    assert!(leftovers.is_empty(), "stale staging files: {leftovers:?}");
}

#[test]
fn persist_is_rejected_while_a_transaction_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    let dest = dir.path().join("system.h5");

    let mut store = scratch_store(&scratch);
    store.begin_transaction().unwrap();
    add(&mut store, 1, 100.0);

    let err = store
        .persist_to(&dest)
        .expect_err("uncommitted rows must not be saved");
    assert!(
        matches!(err, TimeSeriesError::InvalidParameter(_)),
        "expected InvalidParameter, got {err:?}"
    );
}

/// An attached catalog *is* the file a save would write, and the store holds it
/// open. Staging a replacement and renaming it over that open connection orphans
/// the connection and leaves its `-wal` to be recovered against a database it
/// does not belong to — which reads back as a malformed image. `flush` has
/// already made the pair durable, so there is nothing left to do.
#[test]
fn persisting_an_attached_catalog_onto_its_own_path_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");

    let mut store = create_store(Some(&path), false).unwrap();
    add(&mut store, 1, 100.0);
    store.persist_to(&path).unwrap();

    // The catalog the store still writes through has to be the one on disk.
    add(&mut store, 2, 200.0);
    drop(store);

    let reopened = open_store(&path, true).expect("the store survived saving onto itself");
    assert_eq!(read_values(&reopened, 1)[0], 100.0);
    assert_eq!(
        read_values(&reopened, 2)[0],
        200.0,
        "writes after a self-save must reach the catalog on disk"
    );
}

/// The in-memory counterpart is the scratch-directory workflow, not a no-op:
/// the arrays are already at `path` and the save is what creates the sidecar.
#[test]
fn persisting_an_in_memory_catalog_onto_its_own_path_writes_the_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scratch.h5");

    let mut store = scratch_store(&path);
    add(&mut store, 1, 100.0);
    assert!(!catalog_sqlite_path(&path).exists());
    store.persist_to(&path).unwrap();
    drop(store);

    assert!(
        catalog_sqlite_path(&path).exists(),
        "the save wrote a catalog"
    );
    let reopened = open_store(&path, true).unwrap();
    assert_eq!(read_values(&reopened, 1)[0], 100.0);
}

/// A `-wal` beside the destination belongs to the catalog being replaced — a
/// writer that crashed there leaves one. SQLite would recover it over the
/// database renamed into its place, resurrecting the replaced catalog's pages,
/// so the save silently would not take.
#[test]
fn persist_clears_a_stale_wal_beside_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    let dest = dir.path().join("system.h5");

    // Build the destination, capturing its live `-wal` before the clean close
    // checkpoints it away. Restoring that copy is what a crashed writer leaves.
    let wal = sqlite_sidecar(&catalog_sqlite_path(&dest), "-wal");
    let hoarded = dir.path().join("hoarded-wal");
    {
        let mut victim = create_store(Some(&dest), false).unwrap();
        add(&mut victim, 99, 900.0);
        std::fs::copy(&wal, &hoarded).expect("an attached catalog journals through a -wal");
    }
    std::fs::copy(&hoarded, &wal).unwrap();

    let mut store = scratch_store(&scratch);
    add(&mut store, 1, 100.0);
    store.persist_to(&dest).unwrap();
    drop(store);

    assert!(
        !wal.exists(),
        "the swap must not leave the replaced catalog's -wal"
    );
    let saved = open_store(&dest, true).expect("the saved pair opens");
    assert_eq!(read_values(&saved, 1)[0], 100.0);
    assert_eq!(
        keys_for(&saved, 99),
        0,
        "the replaced catalog must not come back"
    );
}

fn sqlite_sidecar(sqlite: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = sqlite.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

// ---------------------------------------------------------------------------
// Operations that predate `CatalogMode`, run in the mode that came second
// ---------------------------------------------------------------------------

/// Compaction rewrites the array half and sweeps the catalog. With the catalog
/// in RAM only the first of those lands, so what has to hold is that the
/// rewritten file still carries the stamp the in-memory catalog is holding —
/// otherwise the checkpoint that follows would pair two halves that disagree.
#[test]
fn compaction_still_pairs_with_an_in_memory_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");

    {
        let mut store = scratch_store(&scratch);
        add(&mut store, 1, 100.0);
        add(&mut store, 2, 200.0);
        store
            .clear_time_series(Some((2, OwnerCategory::Component)))
            .unwrap();

        let report = store.compact().unwrap();
        assert!(
            report.slots_reclaimed + report.datasets_dropped > 0,
            "the removed series left something for the rewrite to reclaim: {report:?}"
        );
        assert!(
            !catalog_sqlite_path(&scratch).exists(),
            "compaction must not land a catalog the caller never asked to write"
        );

        store.persist_catalog().unwrap();
    }

    assert!(
        generation_attr(&scratch).is_some(),
        "the rewrite carried the stamp across"
    );
    let store = open_store(&scratch, true)
        .expect("the rewritten arrays pair with the catalog checkpointed after them");
    assert_eq!(read_values(&store, 1)[0], 100.0);
    assert_eq!(keys_for(&store, 2), 0);
    assert!(store.verify_integrity().unwrap().ok());
}

/// A rollback has to undo both halves, and only one of them is a database. The
/// array side is undone by removing what the transaction wrote — which happens
/// against the backend, so the catalog living in RAM changes nothing about it.
/// The failure this guards against is a rolled-back array surviving as an
/// orphan that the eventual checkpoint then never names.
#[test]
fn a_rollback_under_an_in_memory_catalog_takes_its_arrays_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");

    {
        let mut store = scratch_store(&scratch);
        add(&mut store, 1, 100.0);

        store.begin_transaction().unwrap();
        add(&mut store, 2, 200.0);
        store.rollback_transaction().unwrap();
        assert_eq!(keys_for(&store, 2), 0);

        store.persist_catalog().unwrap();
    }

    let mut store = open_store(&scratch, false).unwrap();
    assert_eq!(read_values(&store, 1)[0], 100.0);
    assert_eq!(keys_for(&store, 2), 0);
    assert!(
        store.verify_integrity().unwrap().ok(),
        "the rolled-back array is gone, not left behind hashing wrong"
    );
    let report = store.compact().unwrap();
    assert_eq!(
        report.datasets_dropped, 0,
        "a rollback leaves no unreferenced dataset for a later compaction to find: {report:?}"
    );
}

/// The buffered form writes nothing until `commit`, so dropping it must leave
/// the array file exactly as it found it — no half-written arrays for a catalog
/// checkpoint to miss.
#[test]
fn a_dropped_bulk_add_writes_no_arrays() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");

    let mut store = scratch_store(&scratch);
    add(&mut store, 1, 100.0);
    store.flush().unwrap();
    let before = std::fs::metadata(&scratch).unwrap().len();

    {
        let mut bulk = store.bulk_add();
        bulk.add(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(200.0)),
            Features::new(),
        );
        assert_eq!(bulk.len(), 1);
    }

    store.flush().unwrap();
    assert_eq!(keys_for(&store, 2), 0);
    assert_eq!(
        std::fs::metadata(&scratch).unwrap().len(),
        before,
        "a discarded batch must not have reached the file"
    );
}

/// The two halves of `persist_to` are not symmetric, and the difference is
/// visible in the result. A store whose arrays are already a file gets that
/// file *copied*, dead space and all; a store whose arrays are in memory gets
/// them *materialized*, which writes only what the catalog still references.
/// Neither is wrong — the copy preserves the live layout — but a caller sizing
/// a save should know which one they are getting.
#[test]
fn saving_copies_an_array_file_but_materializes_an_in_memory_one() {
    let dir = tempfile::tempdir().unwrap();

    // Arrays on disk: the freed slot travels into the save.
    let src = dir.path().join("src.h5");
    let from_disk = dir.path().join("from-disk.h5");
    {
        let mut store = create_store(Some(&src), false).unwrap();
        add(&mut store, 1, 100.0);
        add(&mut store, 2, 200.0);
        store
            .clear_time_series(Some((2, OwnerCategory::Component)))
            .unwrap();
        store.persist_to(&from_disk).unwrap();
    }
    let mut saved = open_store(&from_disk, false).unwrap();
    let copied = saved.compact().unwrap();
    assert!(
        copied.slots_reclaimed > 0,
        "the copy inherited the source's freed slot: {copied:?}"
    );

    // Arrays in memory: only the live set is written out.
    let from_memory = dir.path().join("from-memory.h5");
    {
        let mut store = create_store(None, true).unwrap();
        add(&mut store, 1, 100.0);
        add(&mut store, 2, 200.0);
        store
            .clear_time_series(Some((2, OwnerCategory::Component)))
            .unwrap();
        store.persist_to(&from_memory).unwrap();
    }
    let mut saved = open_store(&from_memory, false).unwrap();
    let materialized = saved.compact().unwrap();
    assert_eq!(
        materialized.slots_reclaimed, 0,
        "materializing writes the live set, so the save starts compact: {materialized:?}"
    );
}

/// `persist_catalog` refuses a read-only store; `persist_to` deliberately does
/// not. Saving elsewhere writes nothing the caller opened read-only, and it is
/// the "load someone's artifact, save my own copy" flow. This pins the
/// asymmetry so neither guard drifts by accident.
#[test]
fn a_read_only_store_can_still_save_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.h5");
    let dest = dir.path().join("dest.h5");
    {
        let mut store = create_store(Some(&src), false).unwrap();
        add(&mut store, 1, 100.0);
    }
    let (before_len, before_stamp) = (
        std::fs::metadata(&src).unwrap().len(),
        generation_attr(&src),
    );

    let mut store = open_store(&src, true).unwrap();
    store.persist_to(&dest).unwrap();
    drop(store);

    assert_eq!(
        std::fs::metadata(&src).unwrap().len(),
        before_len,
        "the source was read-only and must be untouched"
    );
    assert_eq!(generation_attr(&src), before_stamp);
    assert_ne!(
        generation_attr(&dest),
        before_stamp,
        "a save mints its own stamp, so the copy is not mistaken for the source"
    );
    assert_eq!(read_values(&open_store(&dest, true).unwrap(), 1)[0], 100.0);
}

/// Read-only and in-memory are independent notions of "does not write", and a
/// store can hold both. Loading a catalog into RAM read-only must leave the
/// database it was read from byte-for-byte alone, and mutations must be refused
/// rather than merely stranded in RAM.
///
/// It is not quite "touches nothing on disk": SQLite needs the `-shm` index to
/// read a WAL-mode database and creates the sidecar pair even for a read-only
/// connection, then cannot check them back in when it closes. They are left
/// empty — no rows, nothing to recover — and deleting them here would break a
/// concurrent reader holding the same index, so they stay. Where they cannot be
/// created at all, the read falls back to `immutable=1`; see
/// `a_store_on_read_only_media_still_opens`.
#[test]
fn a_read_only_in_memory_catalog_writes_nothing_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        add(&mut store, 1, 100.0);
    }
    let sqlite = catalog_sqlite_path(&path);
    let before = std::fs::read(&sqlite).unwrap();

    let mut store = open_store_with_catalog(&path, true, CatalogMode::InMemory).unwrap();
    assert_eq!(store.catalog_mode(), CatalogMode::InMemory);
    assert_eq!(read_values(&store, 1)[0], 100.0);

    let denied = store.add_time_series(
        2,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::SingleTimeSeries(series(200.0)),
        Features::new(),
    );
    assert!(
        matches!(denied, Err(TimeSeriesError::ReadOnlyStore)),
        "expected ReadOnlyStore, got {denied:?}"
    );
    assert!(matches!(
        store.persist_catalog(),
        Err(TimeSeriesError::ReadOnlyStore)
    ));
    drop(store);

    assert_eq!(
        std::fs::read(&sqlite).unwrap(),
        before,
        "reading a catalog into memory must not rewrite the database it came from"
    );
    let wal = sqlite_sidecar(&sqlite, "-wal");
    if wal.exists() {
        assert_eq!(
            std::fs::metadata(&wal).unwrap().len(),
            0,
            "a read-only load journaled something"
        );
    }
}

/// A store nobody can write to must still be readable — both ways of reading
/// one.
///
/// A WAL-mode catalog needs its `-shm` index, and SQLite creates that sidecar
/// even for a read-only connection. On media that refuses the write — a
/// read-only mount, a shared archive, a directory this process does not own —
/// creating it fails and, before the `immutable=1` fallback, took the whole open
/// with it: a complete, uncorrupted artifact that could not be opened at all.
#[cfg(unix)]
#[test]
fn a_store_on_read_only_media_still_opens() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        add(&mut store, 1, 100.0);
    }
    // The clean close checkpointed and removed the sidecars, which is what
    // makes the fallback safe to take: no committed rows live outside the
    // database file.
    assert!(!sqlite_sidecar(&catalog_sqlite_path(&path), "-wal").exists());

    let perms = std::fs::metadata(dir.path()).unwrap().permissions();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root ignores the mode bits, so there is nothing to test there.
    let enforced = std::fs::File::create(dir.path().join("probe")).is_err();

    let result = std::panic::catch_unwind(|| {
        if !enforced {
            return;
        }
        let attached = open_store(&path, true).expect("a read-only attached open");
        assert_eq!(read_values(&attached, 1)[0], 100.0);
        drop(attached);

        let loaded = open_store_with_catalog(&path, true, CatalogMode::InMemory)
            .expect("a read-only load into memory");
        assert_eq!(read_values(&loaded, 1)[0], 100.0);
    });

    // Restore before the temp dir is dropped, or its own cleanup cannot run.
    std::fs::set_permissions(dir.path(), perms).unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

// ---------------------------------------------------------------------------
// The generation stamp
// ---------------------------------------------------------------------------

/// Swap `donor`'s catalog in beside `victim`'s arrays, simulating a save
/// interrupted between its two renames.
fn transplant_catalog(donor: &std::path::Path, victim: &std::path::Path) {
    std::fs::copy(catalog_sqlite_path(donor), catalog_sqlite_path(victim)).unwrap();
}

#[test]
fn a_catalog_from_a_different_save_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.h5"), dir.path().join("b.h5"));

    for (path, base) in [(&a, 100.0), (&b, 200.0)] {
        let mut store = create_store(Some(path), false).unwrap();
        add(&mut store, 1, base);
        store.flush().unwrap();
    }

    transplant_catalog(&b, &a);

    let err = open_store(&a, true)
        .err()
        .expect("mismatched halves must not open");
    assert!(
        matches!(err, TimeSeriesError::MismatchedArtifact { .. }),
        "expected MismatchedArtifact, got {err:?}"
    );
}

#[test]
fn each_save_mints_a_fresh_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    let (first, second) = (dir.path().join("first.h5"), dir.path().join("second.h5"));

    let mut store = scratch_store(&scratch);
    add(&mut store, 1, 100.0);
    store.persist_to(&first).unwrap();
    store.persist_to(&second).unwrap();

    assert_ne!(
        generation_attr(&first),
        generation_attr(&second),
        "two saves reusing one stamp would make an interrupted re-save undetectable"
    );
    // Each save is internally consistent even though they differ from each other.
    open_store(&first, true).unwrap();
    open_store(&second, true).unwrap();
}

#[test]
fn an_unstamped_artifact_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        add(&mut store, 1, 100.0);
        store.flush().unwrap();
    }

    // Stand in for a store written before the stamp existed: *neither* half
    // carries one. That is the only legitimate unstamped state, because every
    // path that writes a stamp writes both halves together.
    delete_generation_attr(&path);
    delete_catalog_generation(&path);

    let store = open_store(&path, true).unwrap();
    assert_eq!(read_values(&store, 1)[0], 100.0);
}

/// One stamped half and one unstamped is a mismatch, not a legacy artifact.
///
/// This is the hole the pre-stamp migration window left open: a `persist_to`
/// interrupted between its two renames, onto a destination whose previous pair
/// predated stamping, leaves a freshly stamped HDF5 file beside an unstamped
/// catalog. Skipping the check there would silently pair new arrays with an old
/// catalog — the exact failure the stamp exists to prevent.
#[test]
fn one_stamped_half_is_a_mismatch() {
    let dir = tempfile::tempdir().unwrap();

    for strip_h5 in [true, false] {
        let path = dir.path().join(format!("store{strip_h5}.h5"));
        {
            let mut store = create_store(Some(&path), false).unwrap();
            add(&mut store, 1, 100.0);
            store.flush().unwrap();
        }
        if strip_h5 {
            delete_generation_attr(&path);
        } else {
            delete_catalog_generation(&path);
        }

        let err = open_store(&path, true)
            .err()
            .expect("a half-stamped pair must not open");
        assert!(
            matches!(err, TimeSeriesError::MismatchedArtifact { .. }),
            "stripping the {} stamp: expected MismatchedArtifact, got {err:?}",
            if strip_h5 { "HDF5" } else { "catalog" }
        );
    }
}

/// The one hole the stamp does not close, pinned so that closing it stays a
/// decision rather than an accident.
///
/// An artifact predating stamping carries no stamp on either half. Delete its
/// catalog and the attached open creates a fresh, also-unstamped one: the halves
/// agree, so the store opens — as an empty store, with every array still on disk
/// and now unreachable. `verify_integrity` is array-side only and reports it
/// clean. A stamped artifact in the same state is caught, because the surviving
/// HDF5 stamp has no partner.
#[test]
fn an_unstamped_artifact_that_lost_its_catalog_opens_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        add(&mut store, 1, 100.0);
    }
    delete_generation_attr(&path);
    std::fs::remove_file(catalog_sqlite_path(&path)).unwrap();

    let store = open_store(&path, false).expect("both halves unstamped, so the pair agrees");
    assert!(
        store.list_keys(ListFilter::default()).unwrap().is_empty(),
        "the arrays are still in the file, but nothing names them any more"
    );
    assert!(
        store.verify_integrity().unwrap().ok(),
        "and it verifies clean: the check covers stored arrays, not the catalog that lost them"
    );
}

#[test]
fn compaction_preserves_the_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");

    let before = {
        let mut store = create_store(Some(&path), false).unwrap();
        add(&mut store, 1, 100.0);
        add(&mut store, 2, 200.0);
        store
            .clear_time_series(Some((2, OwnerCategory::Component)))
            .unwrap();
        store.compact().unwrap();
        store.flush().unwrap();
        generation_attr(&path)
    };

    // Compaction rewrites only the HDF5 half. Minting a new stamp there would
    // unpair it from the untouched catalog.
    assert!(before.is_some(), "the rewritten file kept a stamp");
    open_store(&path, true).expect("the pair still matches after compaction");
}

// ---------------------------------------------------------------------------
// White-box helpers
// ---------------------------------------------------------------------------

fn generation_attr(path: &std::path::Path) -> Option<String> {
    let f = hdf5_metno::File::open(path).unwrap();
    f.attr("catalog_generation")
        .ok()?
        .read_scalar::<hdf5_metno::types::VarLenUnicode>()
        .ok()
        .map(|v| v.to_string())
}

fn delete_generation_attr(path: &std::path::Path) {
    let f = hdf5_metno::File::open_rw(path).unwrap();
    f.delete_attr("catalog_generation").unwrap();
}

/// Strip the catalog half's stamp, the way an artifact predating stamping has
/// no `catalog_identity` row to begin with.
fn delete_catalog_generation(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(catalog_sqlite_path(path)).unwrap();
    conn.execute("DELETE FROM catalog_identity", []).unwrap();
}
