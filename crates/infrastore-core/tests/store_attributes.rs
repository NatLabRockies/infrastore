//! Tests for store attributes: free-form key/value provenance on the artifact
//! as a whole, as opposed to a supplemental attribute (which belongs to a
//! component) or `application_data` (which belongs to one series).
//!
//! Attributes are independent of time series, so most of these run against an
//! otherwise empty store. The ones that are not about the four calls themselves
//! are about survival: an attribute is only useful if it is still there after
//! the artifact has been saved, compacted, and copied.

use std::collections::BTreeMap;

use infrastore_core::{
    CatalogMode, Compression, Store, TimeSeriesError, create_store, create_store_with_catalog,
    open_store,
};

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// Stamp a store with the provenance every survival test below looks for.
fn stamp(store: &mut Store) {
    store
        .set_store_attribute("creator", "sienna-build")
        .expect("setting an attribute should succeed");
    store
        .set_store_attribute("source_system", "WECC 2032 ADS")
        .expect("setting an attribute should succeed");
}

#[test]
fn set_get_list_remove_round_trip() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    assert_eq!(
        store.list_store_attributes().expect("list should succeed"),
        BTreeMap::new(),
        "a fresh store carries no attributes"
    );

    stamp(&mut store);
    assert_eq!(
        store
            .get_store_attribute("creator")
            .expect("get should succeed"),
        Some("sienna-build".to_string())
    );
    assert_eq!(
        store.list_store_attributes().expect("list should succeed"),
        map(&[
            ("creator", "sienna-build"),
            ("source_system", "WECC 2032 ADS"),
        ])
    );

    assert!(
        store
            .remove_store_attribute("creator")
            .expect("remove should succeed"),
        "removing a key that is there reports true"
    );
    assert_eq!(
        store
            .get_store_attribute("creator")
            .expect("get should succeed"),
        None
    );
    assert_eq!(
        store.list_store_attributes().expect("list should succeed"),
        map(&[("source_system", "WECC 2032 ADS")])
    );
}

#[test]
fn set_replaces_rather_than_appends() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    store
        .set_store_attribute("schema_version", "3")
        .expect("setting an attribute should succeed");
    store
        .set_store_attribute("schema_version", "4")
        .expect("setting an attribute should succeed");
    assert_eq!(
        store.list_store_attributes().expect("list should succeed"),
        map(&[("schema_version", "4")]),
        "an artifact records one value per key, not a history of them"
    );
}

#[test]
fn absent_key_is_a_question_not_an_error() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    assert_eq!(
        store
            .get_store_attribute("nothing")
            .expect("get of an absent key should succeed"),
        None
    );
    assert!(
        !store
            .remove_store_attribute("nothing")
            .expect("remove of an absent key should succeed"),
        "removing an absent key is Ok(false), not an error"
    );
}

#[test]
fn empty_key_is_refused() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    assert!(matches!(
        store.set_store_attribute("", "value"),
        Err(TimeSeriesError::InvalidParameter(_))
    ));
    assert!(matches!(
        store.remove_store_attribute(""),
        Err(TimeSeriesError::InvalidParameter(_))
    ));
}

#[test]
fn reserved_prefix_is_refused_in_both_directions() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    let err = store
        .set_store_attribute("infrastore.generation", "1")
        .expect_err("the reserved prefix should be refused");
    assert!(matches!(err, TimeSeriesError::InvalidParameter(_)));
    assert!(
        err.to_string().contains("infrastore."),
        "the error should name the reserved prefix, got: {err}"
    );
    // Refused on removal too, so "reserved" cannot be worked around by deleting
    // a key the store might later introduce.
    assert!(matches!(
        store.remove_store_attribute("infrastore.generation"),
        Err(TimeSeriesError::InvalidParameter(_))
    ));
    // Only the prefix is reserved; a key merely mentioning the word is fine.
    store
        .set_store_attribute("infrastore_notes", "written by hand")
        .expect("a key outside the reserved prefix should be accepted");
}

#[test]
fn writes_are_refused_on_a_read_only_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(&path), false).expect("store should be created");
        stamp(&mut store);
        store.flush().expect("flush should succeed");
    }

    let mut store = open_store(&path, true).expect("read-only open should succeed");
    assert!(matches!(
        store.set_store_attribute("creator", "someone else"),
        Err(TimeSeriesError::ReadOnlyStore)
    ));
    assert!(matches!(
        store.remove_store_attribute("creator"),
        Err(TimeSeriesError::ReadOnlyStore)
    ));
    // Reads still work, which is the whole point of a read-only open.
    assert_eq!(
        store
            .get_store_attribute("creator")
            .expect("get should succeed"),
        Some("sienna-build".to_string())
    );
}

/// A key is checked before the store's writability, so a bad key reports what
/// is wrong with the key rather than that the store is read-only — the answer
/// that stays true once the caller opens it for writing.
#[test]
fn a_bad_key_is_refused_before_a_read_only_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.h5");
    create_store(Some(&path), false)
        .expect("store should be created")
        .flush()
        .expect("flush should succeed");

    let mut store = open_store(&path, true).expect("read-only open should succeed");
    for key in ["", "infrastore.version"] {
        assert!(matches!(
            store.set_store_attribute(key, "v"),
            Err(TimeSeriesError::InvalidParameter(_))
        ));
        assert!(matches!(
            store.remove_store_attribute(key),
            Err(TimeSeriesError::InvalidParameter(_))
        ));
    }
}

#[test]
fn attributes_take_part_in_the_ambient_transaction() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    store
        .set_store_attribute("creator", "before")
        .expect("setting an attribute should succeed");

    store.begin_transaction().expect("transaction should open");
    store
        .set_store_attribute("creator", "during")
        .expect("setting an attribute should succeed");
    store
        .set_store_attribute("extra", "during")
        .expect("setting an attribute should succeed");
    store
        .rollback_transaction()
        .expect("rollback should succeed");

    assert_eq!(
        store.list_store_attributes().expect("list should succeed"),
        map(&[("creator", "before")]),
        "a rolled-back transaction takes its attribute writes with it"
    );
}

#[test]
fn a_store_holding_only_attributes_is_not_empty() {
    let mut store = create_store(None, true).expect("in-memory store should initialize");
    assert!(store.is_empty().expect("is_empty should succeed"));
    store
        .set_store_attribute("creator", "sienna-build")
        .expect("setting an attribute should succeed");
    assert!(
        !store.is_empty().expect("is_empty should succeed"),
        "attributes are the consumer's own text and are recoverable from \
         nowhere else, so a store carrying them has content"
    );
}

#[test]
fn attributes_survive_persist_to() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("saved.h5");
    {
        let mut store = create_store(None, true).expect("in-memory store should initialize");
        stamp(&mut store);
        store.persist_to(&path).expect("persist_to should succeed");
    }
    let store = open_store(&path, true).expect("the saved artifact should open");
    assert_eq!(
        store.list_store_attributes().expect("list should succeed"),
        map(&[
            ("creator", "sienna-build"),
            ("source_system", "WECC 2032 ADS"),
        ])
    );
}

#[test]
fn attributes_survive_persist_catalog_from_an_in_memory_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store_with_catalog(
            Some(&path),
            false,
            Compression::default(),
            CatalogMode::InMemory,
        )
        .expect("store should be created");
        stamp(&mut store);
        store
            .persist_catalog()
            .expect("persist_catalog should succeed");
    }
    let store = open_store(&path, true).expect("the artifact should open");
    assert_eq!(
        store
            .get_store_attribute("creator")
            .expect("get should succeed"),
        Some("sienna-build".to_string())
    );
}

#[test]
fn attributes_survive_compact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.h5");
    let mut store = create_store(Some(&path), false).expect("store should be created");
    stamp(&mut store);
    store.compact().expect("compact should succeed");
    assert_eq!(
        store.list_store_attributes().expect("list should succeed"),
        map(&[
            ("creator", "sienna-build"),
            ("source_system", "WECC 2032 ADS"),
        ]),
        "compact rewrites only the array half; the catalog keeps its rows"
    );
}

#[test]
fn attributes_survive_open_copy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.h5");
    let dest = dir.path().join("dest.h5");
    {
        let mut store = create_store(Some(&src), false).expect("store should be created");
        stamp(&mut store);
        store.flush().expect("flush should succeed");
    }
    let copy = Store::open_copy(&src, &dest, CatalogMode::Attached).expect("open_copy");
    assert_eq!(
        copy.get_store_attribute("source_system")
            .expect("get should succeed"),
        Some("WECC 2032 ADS".to_string())
    );
}

#[test]
fn open_without_catalog_mints_an_empty_attribute_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(&path), false).expect("store should be created");
        stamp(&mut store);
        store.flush().expect("flush should succeed");
    }
    std::fs::remove_file(infrastore_core::catalog_sqlite_path(&path))
        .expect("the catalog half should be removable");

    let store = Store::open_without_catalog(&path, CatalogMode::Attached)
        .expect("the array half alone should open");
    assert_eq!(
        store.list_store_attributes().expect("list should succeed"),
        BTreeMap::new(),
        "attributes live only in the catalog, so a minted one has none"
    );
}
