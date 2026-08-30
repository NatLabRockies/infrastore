//! The guards that keep an already-saved artifact from being destroyed.
//!
//! A store's two halves are written once and then, in the workflow this library
//! is built for, never touched in place again: a consumer builds in a scratch
//! directory and `persist_to`s the result. What threatens that saved pair is not
//! mainly crashes — every path that writes it stages and renames — but ordinary
//! calls that quietly do the wrong thing to a path that already holds a save.
//! Each test here pins one of those.

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    CatalogMode, Compression, Features, ListFilter, OwnerCategory, SingleTimeSeries, Store,
    TimeSeriesData, TimeSeriesError, TypedArray, catalog_sqlite_path, create_store,
    create_store_replacing, create_store_with_catalog, open_store, open_store_copy,
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

fn add(store: &mut Store, owner: i64, base: f64) {
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

fn read_values(store: &Store, owner: i64) -> Vec<f64> {
    let keys = store
        .list_metadata(ListFilter::new().owner_id(owner))
        .unwrap();
    let key = keys.first().expect("a key for this owner");
    let TimeSeriesData::SingleTimeSeries(s) = store
        .read_by_id(key.id.unwrap(), infrastore_core::ReadWindow::full())
        .unwrap()
    else {
        panic!("expected a SingleTimeSeries");
    };
    s.data.to_f64_vec().unwrap()
}

/// Every owner with a key in `store`, sorted.
fn owners(store: &Store) -> Vec<i64> {
    let mut ids: Vec<i64> = store
        .list_metadata(ListFilter::new())
        .unwrap()
        .iter()
        .map(|m| m.owner_id)
        .collect();
    ids.sort_unstable();
    ids
}

/// The staging files a save leaves in `dir` — none, once it has returned.
fn staging_strays(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".persist-") || n.contains(".repack-"))
        .collect()
}

/// A saved store at `path` holding one series for owner 1.
fn saved_store(path: &std::path::Path) {
    let mut store = create_store(Some(path), false).unwrap();
    add(&mut store, 1, 100.0);
    store.flush().unwrap();
}

// ---------------------------------------------------------------------------
// Creating over an existing artifact
// ---------------------------------------------------------------------------

/// The failure this guard exists for, in full.
///
/// Creating truncates the HDF5 file but only *opens* the catalog, then stamps
/// both halves with one fresh generation. Without the guard, pointing a build
/// script at a path that already holds a save left an empty array file paired
/// with the old catalog's rows — a store that opens cleanly, reports every
/// series still present, and has nothing behind any of them. No crash required.
#[test]
fn creating_over_a_saved_store_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("system.h5");
    saved_store(&path);
    let before = std::fs::metadata(&path).unwrap().len();

    let err = create_store(Some(&path), false)
        .err()
        .expect("creating over a saved store must be refused");
    assert!(
        matches!(err, TimeSeriesError::StoreExists { .. }),
        "expected StoreExists, got {err:?}"
    );

    // Refused means untouched, not partially applied.
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        before,
        "the refused create still truncated the file"
    );
    let store = open_store(&path, true).unwrap();
    assert_eq!(read_values(&store, 1)[0], 100.0);
    assert!(store.verify_integrity().unwrap().ok());
}

/// Either half alone is enough to poison a fresh store, so either half alone is
/// enough to refuse: an orphaned catalog would pair its rows with new, empty
/// arrays, and an orphaned HDF5 file would pair its arrays with a new, empty
/// catalog.
#[test]
fn creating_over_a_lone_half_is_refused() {
    let dir = tempfile::tempdir().unwrap();

    let only_catalog = dir.path().join("catalog_only.h5");
    saved_store(&only_catalog);
    std::fs::remove_file(&only_catalog).unwrap();
    assert!(matches!(
        create_store(Some(&only_catalog), false).err(),
        Some(TimeSeriesError::StoreExists { .. })
    ));

    let only_arrays = dir.path().join("arrays_only.h5");
    saved_store(&only_arrays);
    std::fs::remove_file(catalog_sqlite_path(&only_arrays)).unwrap();
    assert!(matches!(
        create_store(Some(&only_arrays), false).err(),
        Some(TimeSeriesError::StoreExists { .. })
    ));
}

#[test]
fn create_replacing_discards_both_halves() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("system.h5");
    saved_store(&path);

    {
        let mut store =
            create_store_replacing(&path, Compression::default(), CatalogMode::Attached).unwrap();
        add(&mut store, 2, 200.0);
        store.flush().unwrap();
    }

    // The old catalog went with the old arrays. Had it survived, owner 1 would
    // still be listed here with nothing behind it.
    let store = open_store(&path, true).unwrap();
    assert!(
        store
            .list_metadata(ListFilter::new().owner_id(1))
            .unwrap()
            .is_empty(),
        "the replaced store's catalog rows survived"
    );
    assert_eq!(read_values(&store, 2)[0], 200.0);
    assert!(store.verify_integrity().unwrap().ok());
}

// ---------------------------------------------------------------------------
// Working on a copy
// ---------------------------------------------------------------------------

#[test]
fn open_copy_leaves_the_original_alone() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("system.h5");
    let dest = dir.path().join("scratch.h5");
    saved_store(&src);
    let src_bytes = std::fs::read(&src).unwrap();

    {
        let mut copy = open_store_copy(&src, &dest, CatalogMode::Attached).unwrap();
        assert_eq!(read_values(&copy, 1)[0], 100.0, "the copy carries the data");
        add(&mut copy, 2, 200.0);
        copy.flush().unwrap();
    }

    assert_eq!(
        std::fs::read(&src).unwrap(),
        src_bytes,
        "the source file changed"
    );
    let original = open_store(&src, true).unwrap();
    assert!(
        original
            .list_metadata(ListFilter::new().owner_id(2))
            .unwrap()
            .is_empty(),
        "a mutation of the copy reached the original"
    );

    // The round trip a consumer actually runs: change the copy, save back over
    // the original. The original is only replaced by the final atomic rename —
    // and nothing may still hold it open, since Windows refuses to rename over
    // a file with a live handle.
    drop(original);
    let mut copy = open_store(&dest, false).unwrap();
    copy.persist_to(&src).unwrap();
    let reloaded = open_store(&src, true).unwrap();
    assert_eq!(read_values(&reloaded, 2)[0], 200.0);
    assert!(reloaded.verify_integrity().unwrap().ok());
}

/// Committed rows sitting in a crashed source's `-wal` must reach the copy.
///
/// SQLite in WAL mode holds committed transactions in the sidecar until a
/// checkpoint, so a writer that died leaves rows the main database does not have
/// yet. Copying `<src>.sqlite` with `fs::copy` dropped them — and dropped them
/// silently, because the copy then opened cleanly and simply listed fewer
/// series. The catalog half goes through `VACUUM INTO` instead, which reads
/// through committed WAL content.
#[test]
fn open_copy_carries_rows_still_in_the_sources_wal() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("system.h5");
    let sqlite = catalog_sqlite_path(&src);
    let wal = {
        let mut name = sqlite.as_os_str().to_os_string();
        name.push("-wal");
        std::path::PathBuf::from(name)
    };

    // Rebuild what a crashed writer leaves: owner 1 checkpointed into the main
    // database, owner 2 committed but living only in the `-wal`.
    //
    // Assembled from two snapshots rather than one, because only the catalog can
    // be copied out from under a live store. HDF5 holds a byte-range lock on an
    // open file, so copying the `.h5` mid-write fails on Windows — and it is not
    // needed: the arrays are the same either way, and letting them close cleanly
    // gives the crashed catalog real arrays to point at.
    let hoard_sqlite = dir.path().join("h.sqlite");
    let hoard_wal = dir.path().join("h.wal");
    {
        let mut store = create_store(Some(&src), false).unwrap();
        add(&mut store, 1, 100.0);
        store.flush().unwrap(); // checkpoints owner 1 into the main database
    }
    // Snapshotted closed, so the copy is safe on every platform.
    std::fs::copy(&sqlite, &hoard_sqlite).unwrap();
    {
        let mut store = open_store(&src, false).unwrap();
        add(&mut store, 2, 200.0); // committed, but still only in the `-wal`
        std::fs::copy(&wal, &hoard_wal).expect("an attached catalog journals through a -wal");
    }
    // That clean close checkpointed owner 2 into the main database and removed
    // the `-wal`. Restoring the pre-owner-2 catalog beside the `-wal` written on
    // top of it is exactly the pair a killed writer leaves behind.
    std::fs::copy(&hoard_sqlite, &sqlite).unwrap();
    std::fs::copy(&hoard_wal, &wal).unwrap();

    let dest = dir.path().join("copy.h5");
    let copy = open_store_copy(&src, &dest, CatalogMode::Attached).unwrap();
    let mut owners: Vec<i64> = copy
        .list_metadata(ListFilter::new())
        .unwrap()
        .iter()
        .map(|m| m.owner_id)
        .collect();
    owners.sort_unstable();
    assert_eq!(
        owners,
        [1, 2],
        "a row committed to the source's -wal did not reach the copy"
    );
    // And durably: `VACUUM INTO` wrote a self-contained database, so the copy
    // still has both rows once its own connection is gone.
    drop(copy);
    assert_eq!(
        open_store(&dest, true)
            .unwrap()
            .list_metadata(ListFilter::new())
            .unwrap()
            .len(),
        2
    );

    // The source is still exactly what it was — copying it is not a recovery
    // that mutates it.
    assert!(wal.exists(), "open_copy checkpointed the source's -wal");
}

/// A source with no catalog is the half-artifact an in-memory-catalog store
/// leaves when its process dies before landing one. `open_copy` copies no
/// catalog for it — deliberately, so the copy stays honest about what the source
/// is — and the copy then fails to open rather than presenting the arrays as an
/// empty store.
#[test]
fn open_copy_of_a_half_artifact_refuses_rather_than_reading_it_empty() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    {
        let mut store = create_store_with_catalog(
            Some(&scratch),
            false,
            Compression::default(),
            CatalogMode::InMemory,
        )
        .unwrap();
        add(&mut store, 1, 100.0);
        store.flush().unwrap();
    }
    assert!(!catalog_sqlite_path(&scratch).exists());

    let attached = dir.path().join("attached-copy.h5");
    let err = open_store_copy(&scratch, &attached, CatalogMode::Attached)
        .err()
        .expect("a copy of a half-artifact must not open");
    assert!(
        matches!(err, TimeSeriesError::MismatchedArtifact { .. }),
        "expected MismatchedArtifact, got {err:?}"
    );
    // And it takes the copied arrays with it. A destination left behind is the
    // state `StoreExists` refuses next time, stranding the caller on a path they
    // never successfully wrote — which is as true of a failed open as of a
    // failed copy.
    assert!(
        !attached.exists(),
        "the failed copy left a destination behind"
    );
    assert!(!catalog_sqlite_path(&attached).exists());
    // So the same call can simply be tried again.
    let err = open_store_copy(&scratch, &attached, CatalogMode::Attached)
        .err()
        .expect("still a half-artifact");
    assert!(matches!(err, TimeSeriesError::MismatchedArtifact { .. }));

    let loaded = dir.path().join("in-memory-copy.h5");
    assert!(
        open_store_copy(&scratch, &loaded, CatalogMode::InMemory).is_err(),
        "loading the copy's catalog into memory needs a catalog to load"
    );
}

/// The mode flows through to the copy: `open_copy` hands back a store whose
/// catalog is in RAM, so changes stay there until a checkpoint lands them beside
/// the copied arrays. The source is untouched throughout.
#[test]
fn open_copy_can_hand_back_an_in_memory_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("system.h5");
    let dest = dir.path().join("scratch.h5");
    saved_store(&src);

    let mut copy = open_store_copy(&src, &dest, CatalogMode::InMemory).unwrap();
    assert_eq!(copy.catalog_mode(), CatalogMode::InMemory);
    add(&mut copy, 2, 200.0);
    copy.flush().unwrap();
    assert!(
        owners(&open_store(&dest, true).unwrap()) == vec![1],
        "the copied catalog on disk is still the source's until a checkpoint"
    );

    copy.persist_catalog().unwrap();
    drop(copy);

    let landed = open_store(&dest, true).unwrap();
    assert_eq!(read_values(&landed, 1)[0], 100.0);
    assert_eq!(read_values(&landed, 2)[0], 200.0);
    assert_eq!(
        owners(&open_store(&src, true).unwrap()),
        vec![1],
        "nothing reached the source"
    );
}

#[test]
fn open_copy_refuses_a_destination_that_already_holds_a_store() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("system.h5");
    let dest = dir.path().join("other.h5");
    saved_store(&src);
    saved_store(&dest);

    let err = open_store_copy(&src, &dest, CatalogMode::Attached)
        .err()
        .expect("copying onto a live store must be refused");
    assert!(
        matches!(err, TimeSeriesError::StoreExists { .. }),
        "expected StoreExists, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Landing an in-memory catalog
// ---------------------------------------------------------------------------

#[test]
fn persist_catalog_pairs_an_in_memory_catalog_with_the_arrays_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scratch.h5");

    {
        let mut store = create_store_with_catalog(
            Some(&path),
            false,
            Compression::default(),
            CatalogMode::InMemory,
        )
        .unwrap();
        add(&mut store, 1, 100.0);
        assert!(
            !catalog_sqlite_path(&path).exists(),
            "an in-memory catalog writes nothing until asked"
        );
        store.persist_catalog().unwrap();
    }

    // The catalog landed beside the arrays already in place — no copy of the
    // HDF5 half, and stamped to match it, so the pair opens.
    let store = open_store(&path, true).unwrap();
    assert_eq!(read_values(&store, 1)[0], 100.0);
    assert!(store.verify_integrity().unwrap().ok());
}

#[test]
fn persist_catalog_is_a_checkpoint_not_a_mode_switch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scratch.h5");

    let mut store = create_store_with_catalog(
        Some(&path),
        false,
        Compression::default(),
        CatalogMode::InMemory,
    )
    .unwrap();
    add(&mut store, 1, 100.0);
    store.persist_catalog().unwrap();
    // Written after the checkpoint: still RAM-only until the next one.
    add(&mut store, 2, 200.0);
    store.flush().unwrap();

    {
        let reopened = open_store(&path, true).unwrap();
        assert_eq!(read_values(&reopened, 1)[0], 100.0);
        assert!(
            reopened
                .list_metadata(ListFilter::new().owner_id(2))
                .unwrap()
                .is_empty(),
            "post-checkpoint changes must not be on disk yet"
        );
    }

    store.persist_catalog().unwrap();
    let reopened = open_store(&path, true).unwrap();
    assert_eq!(read_values(&reopened, 2)[0], 200.0);
}

/// The three states in which landing a catalog is refused, and the one in which
/// it degrades to a flush.
///
/// Each is a documented contract with a distinct reason, and each is a way a
/// caller can otherwise believe a checkpoint happened when it did not.
#[test]
fn persist_catalog_refuses_what_it_cannot_pair() {
    let dir = tempfile::tempdir().unwrap();

    // No HDF5 file means no half to pair a catalog with. `persist_to` is the
    // call that materializes an in-memory store; this one has nothing to sit
    // beside.
    let mut in_memory = create_store(None, true).unwrap();
    add(&mut in_memory, 1, 100.0);
    let err = in_memory
        .persist_catalog()
        .expect_err("an in-memory store has no artifact to pair with");
    assert!(
        matches!(err, TimeSeriesError::InvalidParameter(_)),
        "expected InvalidParameter, got {err:?}"
    );

    // An open transaction holds uncommitted rows a rollback would take back;
    // writing them out would publish a state the caller has not committed to.
    let scratch = dir.path().join("scratch.h5");
    let mut store = create_store_with_catalog(
        Some(&scratch),
        false,
        Compression::default(),
        CatalogMode::InMemory,
    )
    .unwrap();
    add(&mut store, 1, 100.0);
    store.begin_transaction().unwrap();
    let err = store
        .persist_catalog()
        .expect_err("an open transaction must block the checkpoint");
    assert!(
        matches!(err, TimeSeriesError::InvalidParameter(_)),
        "expected InvalidParameter, got {err:?}"
    );
    store.rollback_transaction().unwrap();
    store.persist_catalog().unwrap();
    drop(store);

    // A read-only store may not write either half.
    let mut ro = open_store(&scratch, true).unwrap();
    assert!(matches!(
        ro.persist_catalog().err(),
        Some(TimeSeriesError::ReadOnlyStore)
    ));
    drop(ro);

    // For an attached catalog the file already *is* the catalog, so this is a
    // flush rather than an error — the same call works whichever mode a caller
    // happens to hold.
    let attached = dir.path().join("attached.h5");
    let mut store = create_store(Some(&attached), false).unwrap();
    add(&mut store, 7, 700.0);
    store.persist_catalog().unwrap();
    drop(store);
    assert_eq!(
        read_values(&open_store(&attached, true).unwrap(), 7)[0],
        700.0
    );
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/// A save stages through a name unique to itself, so a leftover from an earlier
/// interrupted save cannot be picked up as this one's staging area — the way a
/// fixed `<target>.persist` could be, with nothing locking a `persist_to`
/// destination to serialize two savers.
#[test]
fn persist_stages_through_a_unique_temp() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    let dest = dir.path().join("system.h5");

    let mut store = create_store(Some(&scratch), false).unwrap();
    add(&mut store, 1, 100.0);

    // What an interrupted save under the old fixed-name scheme left behind.
    let legacy_temp = dir.path().join("system.h5.persist");
    std::fs::write(&legacy_temp, b"leftover from a crashed save").unwrap();

    store.persist_to(&dest).unwrap();

    assert_eq!(read_values(&open_store(&dest, true).unwrap(), 1)[0], 100.0);
    assert!(
        legacy_temp.exists(),
        "an unrelated leftover must be left alone, not adopted as staging"
    );
    let strays: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".persist-"))
        .collect();
    assert!(
        strays.is_empty(),
        "a completed save left its staged temps behind: {strays:?}"
    );
}

// ---------------------------------------------------------------------------
// When a save fails
// ---------------------------------------------------------------------------
//
// Every path here stages, and every one of them can fail partway. The staging
// is what makes the failures survivable, but the recovery code — reopening the
// backend the save dropped, clearing the temps, leaving the caller's store
// live — only runs when something goes wrong, which is precisely when it is
// least tested. Each test below drives one of those paths.
//
// The injection is a destination that cannot be written: a directory that does
// not exist, or one standing where a file has to land. Both fail the same way
// on Unix and Windows, which a permission bit does not.

/// A save that cannot even stage must leave the store exactly as usable as it
/// was. `persist_to` drops the live HDF5 handle before copying the file, so the
/// real subject here is the reopen on the failure path: without it the store is
/// stranded on a placeholder backend and every later call reads an empty store.
#[test]
fn a_failed_save_leaves_a_disk_backed_store_live() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("store.h5");

    let mut store = create_store(Some(&src), false).unwrap();
    add(&mut store, 1, 100.0);

    let doomed = dir.path().join("no-such-dir").join("system.h5");
    assert!(
        store.persist_to(&doomed).is_err(),
        "staging into a directory that does not exist cannot succeed"
    );

    // Still the same store: readable, writable, and saveable.
    assert_eq!(read_values(&store, 1)[0], 100.0);
    add(&mut store, 2, 200.0);
    let good = dir.path().join("system.h5");
    store.persist_to(&good).unwrap();
    drop(store);

    let saved = open_store(&good, true).unwrap();
    assert_eq!(owners(&saved), vec![1, 2]);
    assert!(
        staging_strays(dir.path()).is_empty(),
        "the failed save left staging files: {:?}",
        staging_strays(dir.path())
    );
}

/// The same for the other half of `persist_to`: a store whose arrays live in
/// memory materializes them into the staged file instead of copying one.
#[test]
fn a_failed_save_leaves_an_in_memory_store_live() {
    let dir = tempfile::tempdir().unwrap();

    let mut store = create_store(None, true).unwrap();
    add(&mut store, 1, 100.0);

    let doomed = dir.path().join("no-such-dir").join("system.h5");
    assert!(store.persist_to(&doomed).is_err());

    assert_eq!(read_values(&store, 1)[0], 100.0);
    add(&mut store, 2, 200.0);
    let good = dir.path().join("system.h5");
    store.persist_to(&good).unwrap();
    drop(store);

    assert_eq!(owners(&open_store(&good, true).unwrap()), vec![1, 2]);
    assert!(staging_strays(dir.path()).is_empty());
}

/// A checkpoint that cannot land must not consume the catalog it was writing.
/// It stays in RAM, still complete, still writable, and the next attempt lands
/// everything — including what was added after the failure.
#[test]
fn a_failed_catalog_checkpoint_keeps_the_catalog_in_ram() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    let sqlite = catalog_sqlite_path(&scratch);

    let mut store = create_store_with_catalog(
        Some(&scratch),
        false,
        Compression::default(),
        CatalogMode::InMemory,
    )
    .unwrap();
    add(&mut store, 1, 100.0);

    // A directory where the sidecar belongs: staging succeeds, the rename does
    // not.
    std::fs::create_dir(&sqlite).unwrap();
    assert!(store.persist_catalog().is_err());
    assert!(
        staging_strays(dir.path()).is_empty(),
        "the failed checkpoint left staging files: {:?}",
        staging_strays(dir.path())
    );

    std::fs::remove_dir(&sqlite).unwrap();
    add(&mut store, 2, 200.0);
    store.persist_catalog().unwrap();
    drop(store);

    assert_eq!(owners(&open_store(&scratch, true).unwrap()), vec![1, 2]);
}

/// The documented limit of `persist_to`, pinned rather than papered over.
///
/// The two renames cannot be atomic together, so a save that fails after the
/// first one has already replaced the destination's array half. The docs say
/// callers must not assume the destination survived a failed save; this proves
/// it, and proves the other half of the bargain — the stamp turns the resulting
/// half-save into a loud `MismatchedArtifact` instead of a store that quietly
/// contradicts itself.
#[test]
fn a_failed_save_can_still_have_replaced_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    let dest = dir.path().join("system.h5");
    let dest_sqlite = catalog_sqlite_path(&dest);

    let mut store = create_store(Some(&scratch), false).unwrap();
    add(&mut store, 1, 100.0);

    // Let the arrays land and block the catalog behind them.
    std::fs::create_dir(&dest_sqlite).unwrap();
    assert!(store.persist_to(&dest).is_err());
    drop(store);

    assert!(
        dest.exists(),
        "the array half was renamed into place before the failure"
    );
    assert!(
        staging_strays(dir.path()).is_empty(),
        "the failed save left staging files: {:?}",
        staging_strays(dir.path())
    );

    std::fs::remove_dir(&dest_sqlite).unwrap();
    let err = open_store(&dest, false)
        .err()
        .expect("a half-saved destination must not open");
    assert!(
        matches!(err, TimeSeriesError::MismatchedArtifact { .. }),
        "expected MismatchedArtifact, got {err:?}"
    );
}

/// Staging names are unique per call, so nothing sweeps the temps an
/// interrupted save or compaction leaves behind. They accumulate — a cost worth
/// knowing about — but they must be inert: never adopted as this call's staging
/// area, never mistaken for a store, never removed on someone else's behalf.
#[test]
fn stale_staging_files_are_inert() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let dest = dir.path().join("system.h5");

    let litter = [
        dir.path().join("store.h5.repack-0123456789abcdef"),
        dir.path().join("system.h5.persist-0123456789abcdef"),
        dir.path().join("system.h5.sqlite.persist-0123456789abcdef"),
    ];
    for f in &litter {
        std::fs::write(f, b"leftover from an interrupted save").unwrap();
    }

    let mut store = create_store(Some(&path), false).unwrap();
    add(&mut store, 1, 100.0);
    store.compact().unwrap();
    store.persist_to(&dest).unwrap();
    drop(store);

    assert_eq!(owners(&open_store(&dest, true).unwrap()), vec![1]);
    for f in &litter {
        assert_eq!(
            std::fs::read(f).unwrap(),
            b"leftover from an interrupted save",
            "{} was adopted or rewritten",
            f.display()
        );
    }
}

/// The scratch-directory workflow's recovery path. A run that dies before
/// landing its catalog leaves the array half behind; the next run must refuse to
/// create over it rather than pairing a fresh empty catalog with somebody
/// else's arrays, and the explicit destructive form must be the way through.
#[test]
fn an_abandoned_scratch_file_blocks_recreation_until_it_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch.h5");
    {
        let mut store = create_store_with_catalog(
            Some(&scratch),
            false,
            Compression::default(),
            CatalogMode::InMemory,
        )
        .unwrap();
        add(&mut store, 1, 100.0);
        store.flush().unwrap();
    }

    let err = create_store_with_catalog(
        Some(&scratch),
        false,
        Compression::default(),
        CatalogMode::InMemory,
    )
    .err()
    .expect("the leftover array half must block a fresh create");
    assert!(
        matches!(err, TimeSeriesError::StoreExists { .. }),
        "expected StoreExists, got {err:?}"
    );

    let mut fresh =
        create_store_replacing(&scratch, Compression::default(), CatalogMode::InMemory).unwrap();
    add(&mut fresh, 7, 700.0);
    fresh.persist_catalog().unwrap();
    drop(fresh);

    let store = open_store(&scratch, true).unwrap();
    assert_eq!(
        owners(&store),
        vec![7],
        "the abandoned run's arrays did not come back"
    );
    assert!(store.verify_integrity().unwrap().ok());
}
