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

    // Documented mode-1 semantics: the arrays are on disk but nothing names
    // them, so a reopen that creates a fresh catalog sees an empty store.
    let store = open_store(&scratch, false).unwrap();
    assert_eq!(
        keys_for(&store, 1),
        0,
        "catalog changes were never persisted, so nothing is addressable"
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

    // Stand in for a store written before the stamp existed. Half-unstamped is
    // the strictly harder case: the check must skip rather than compare against
    // a missing value.
    delete_generation_attr(&path);

    let store = open_store(&path, true).unwrap();
    assert_eq!(read_values(&store, 1)[0], 100.0);
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
