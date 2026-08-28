//! Tests for the catalog's association ids: the `id` column of the three
//! association tables, which a consumer stores in its own object model as a
//! durable reference to one row (a generator's cost function naming the series
//! that varies it).
//!
//! What makes the id worth testing is the guarantee, not the value: an id is
//! never reissued once its row is gone. A bare `INTEGER PRIMARY KEY` recycles —
//! delete the highest row, add another, and the new row takes the old one's id,
//! so a persisted reference silently resolves to a different and entirely valid
//! row. Nothing else in the store would notice, which is why these assertions
//! are here rather than left to the surfaces that read the id back.
//!
//! These reach into the sidecar catalog with `rusqlite` directly. The id is not
//! yet on the public API — that arrives with the write and read surfaces — so
//! the DDL is currently the whole contract, and it is exactly what needs
//! pinning.

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, AddedTimeSeries, FeatureValue, Features, KeyIdentity, ListFilter, OwnerCategory,
    ParentChildAssociation, Period, SingleTimeSeries, SupplementalAttributeAssociation,
    SupplementalAttributeFilter, TimeSeriesData, TimeSeriesError, TimeSeriesType, TransformPolicy,
    TypedArray, create_store, open_store,
};

/// One hourly `SingleTimeSeries` named `name`, three points long.
fn series(name: &str) -> SingleTimeSeries {
    let initial_timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let data = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);
    SingleTimeSeries::new(initial_timestamp, Duration::hours(1), data, name)
}

fn key(name: &str) -> KeyIdentity {
    KeyIdentity {
        owner_id: 1,
        owner_category: OwnerCategory::Component,
        time_series_type: TimeSeriesType::SingleTimeSeries,
        name: name.into(),
        resolution: Some(Period::fixed(Duration::hours(1))),
        interval: None,
        features: Features::new(),
    }
}

/// Add and drop `n` throwaway rows, so the next id `store` assigns clears `n`.
///
/// Ids are assigned and never chosen, so a document whose ids have to sit above
/// an importing store's high-water mark is arranged by advancing the exporter's
/// counter rather than by naming ids on the way in.
fn advance_ids(store: &mut infrastore_core::Store, n: usize) {
    for i in 0..n {
        let name = format!("__spacer{i}");
        store
            .add(AddRequest::new(
                999,
                "Spacer",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(&name)),
            ))
            .unwrap();
        let mut k = key(&name);
        k.owner_id = 999;
        store.remove_time_series(&k).unwrap();
    }
}

/// The `.sqlite` sidecar beside an HDF5 store.
fn sidecar(store_path: &std::path::Path) -> std::path::PathBuf {
    let mut p = store_path.as_os_str().to_owned();
    p.push(".sqlite");
    std::path::PathBuf::from(p)
}

/// Every association table declares `AUTOINCREMENT`, and SQLite is tracking the
/// high-water mark for each.
///
/// The `sqlite_sequence` half is what proves the keyword took effect rather
/// than merely being present in a string: SQLite creates that table only for
/// tables declared `AUTOINCREMENT`, and gives a table a row there only once it
/// has been inserted into.
#[test]
fn every_association_table_declares_autoincrement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series("load")),
                Features::new(),
            )
            .unwrap();
        store
            .add_supplemental_attribute_association(
                infrastore_core::SupplementalAttributeAssociation {
                    component_id: 1,
                    component_type: "Generator".into(),
                    attribute_id: 7,
                    attribute_type: "GeographicInfo".into(),
                    id: None,
                },
            )
            .unwrap();
        store
            .add_parent_child_association(infrastore_core::ParentChildAssociation {
                parent_id: 1,
                parent_type: "Generator".into(),
                child_id: 2,
                child_type: "Bus".into(),
                id: None,
            })
            .unwrap();
        store.flush().unwrap();
    }

    let conn = rusqlite::Connection::open(sidecar(&path)).unwrap();
    for table in [
        "time_series_associations",
        "supplemental_attribute_associations",
        "parent_child_associations",
    ] {
        let ddl: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("{table} is missing from the catalog: {e}"));
        assert!(
            ddl.contains("AUTOINCREMENT"),
            "{table} must declare AUTOINCREMENT so its ids are never reissued; got:\n{ddl}",
        );

        let seq: i64 = conn
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| {
                panic!("{table} has no sqlite_sequence row, so AUTOINCREMENT is not in effect: {e}")
            });
        assert!(
            seq >= 1,
            "{table} recorded a nonsensical high-water mark {seq}"
        );
    }
}

/// Deleting the highest-numbered row and adding another does not hand the new
/// row the old one's id.
///
/// This is the whole point of the change, and the one behavior a bare
/// `INTEGER PRIMARY KEY` gets wrong: it assigns `max(rowid) + 1`, so the id of
/// a deleted top row is immediately reused.
#[test]
fn an_id_is_not_reused_after_its_row_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");

    let ids_by_name = |conn: &rusqlite::Connection| -> Vec<(String, i64)> {
        let mut stmt = conn
            .prepare("SELECT name, id FROM time_series_associations ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap();
        rows.map(Result::unwrap).collect()
    };

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        for name in ["first", "second", "third"] {
            store
                .add_time_series(
                    1,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(series(name)),
                    Features::new(),
                )
                .unwrap();
        }
        store.flush().unwrap();
    }

    let before = {
        let conn = rusqlite::Connection::open(sidecar(&path)).unwrap();
        ids_by_name(&conn)
    };
    assert_eq!(before.len(), 3);
    let highest = before.last().unwrap().1;

    // Remove the row holding the highest id, then add a fresh series.
    {
        let mut store = open_store(path.as_path(), false).unwrap();
        let doomed = &before.last().unwrap().0;
        store.remove_time_series(&key(doomed)).unwrap();
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series("fourth")),
                Features::new(),
            )
            .unwrap();
        store.flush().unwrap();
    }

    let after = {
        let conn = rusqlite::Connection::open(sidecar(&path)).unwrap();
        ids_by_name(&conn)
    };
    let fresh = after
        .iter()
        .find(|(name, _)| name == "fourth")
        .expect("the newly added series is missing")
        .1;
    assert!(
        fresh > highest,
        "id {fresh} was reissued from the deleted row's id {highest}; a stored reference to \
         the deleted series would now resolve to this one",
    );

    // The surviving rows keep the ids they were given.
    for (name, id) in &before[..2] {
        let still = after
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name} disappeared"))
            .1;
        assert_eq!(still, *id, "{name} changed id across an unrelated removal");
    }
}

/// Each table counts independently: an id is only meaningful together with the
/// table it came from. The assertions below are that all three *do* start at 1
/// — equal values across tables are the ordinary case and say nothing, which is
/// the whole reason an id has to be carried with the table that issued it.
#[test]
fn the_three_tables_have_independent_id_streams() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        // Two time series, one attachment, one edge: if the streams were shared
        // the attachment and the edge could not both be id 1.
        for name in ["load", "wind"] {
            store
                .add_time_series(
                    1,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(series(name)),
                    Features::new(),
                )
                .unwrap();
        }
        store
            .add_supplemental_attribute_association(
                infrastore_core::SupplementalAttributeAssociation {
                    component_id: 1,
                    component_type: "Generator".into(),
                    attribute_id: 7,
                    attribute_type: "GeographicInfo".into(),
                    id: None,
                },
            )
            .unwrap();
        store
            .add_parent_child_association(infrastore_core::ParentChildAssociation {
                parent_id: 1,
                parent_type: "Generator".into(),
                child_id: 2,
                child_type: "Bus".into(),
                id: None,
            })
            .unwrap();
        store.flush().unwrap();
    }

    let conn = rusqlite::Connection::open(sidecar(&path)).unwrap();
    let first_id = |table: &str| -> i64 {
        conn.query_row(&format!("SELECT MIN(id) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    };
    assert_eq!(first_id("time_series_associations"), 1);
    assert_eq!(first_id("supplemental_attribute_associations"), 1);
    assert_eq!(first_id("parent_child_associations"), 1);
}

// ---------------------------------------------------------------------------
// Assignment, and the row-level paths that must not carry an id over
// ---------------------------------------------------------------------------

/// A stored row always reports its id; a request that did not ask for one gets
/// whatever the catalog assigned.
#[test]
fn a_stored_row_reports_the_id_the_catalog_gave_it() {
    let mut store = create_store(None, true).unwrap();
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("load")),
        ))
        .unwrap();

    let meta = store.get_metadata(&key("load")).unwrap();
    assert_eq!(
        meta.id,
        Some(1),
        "the first row of a fresh catalog is id 1, and a read must report it",
    );
}

/// An add never files a row under an id the caller picked. `AddRequest` names
/// none, and the association rows' `id` field — which a listing populates — is
/// ignored on the way back in, so a row read from one store and re-added to
/// another is filed under a fresh id rather than carrying the old one over.
#[test]
fn an_add_always_lets_the_catalog_assign() {
    let mut store = create_store(None, true).unwrap();
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("first")),
        ))
        .unwrap();
    assert_eq!(store.get_metadata(&key("first")).unwrap().id, Some(1));

    // An attachment read back from another store still carries that store's id.
    let mut elsewhere = attach(1, 100);
    elsewhere.id = Some(500);
    assert_eq!(
        store
            .add_supplemental_attribute_association(elsewhere)
            .unwrap(),
        1,
        "the id on the way in is ignored; this catalog assigns its own",
    );

    let mut edge = ParentChildAssociation {
        parent_id: 1,
        parent_type: "Generator".into(),
        child_id: 2,
        child_type: "Bus".into(),
        id: Some(500),
    };
    assert_eq!(store.add_parent_child_association(edge.clone()).unwrap(), 1);
    edge.child_id = 3;
    assert_eq!(store.add_parent_child_association(edge).unwrap(), 2);
}

/// An import's explicit id is honored, and the catalog's counter ratchets past
/// it — so a later assigned id cannot land on top of one the document placed.
#[test]
fn an_imported_id_is_honored_and_ratchets_the_counter() {
    let mut source = create_store(None, true).unwrap();
    advance_ids(&mut source, 500);
    source
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("imported")),
        ))
        .unwrap();
    let json = source
        .export_time_series_associations_openapi(&ListFilter::default())
        .unwrap();

    let mut target = create_store(None, true).unwrap();
    target
        .add(AddRequest::new(
            9,
            "Anchor",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("anchor")),
        ))
        .unwrap();
    target
        .import_time_series_associations_openapi(&json)
        .unwrap();
    target
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("assigned")),
        ))
        .unwrap();

    assert_eq!(target.get_metadata(&key("imported")).unwrap().id, Some(501));
    assert_eq!(
        target.get_metadata(&key("assigned")).unwrap().id,
        Some(502),
        "an assigned id must start past the imported one, not collide with it",
    );
}

/// The two collisions that both arrive as a SQLite constraint violation stay
/// distinguishable. Reporting one as the other sends a caller looking in
/// entirely the wrong place: a duplicate series is a re-add to fix, an id
/// collision means the import's ids do not fit this store.
#[test]
fn an_id_collision_and_an_identity_collision_are_different_errors() {
    // A document exported from a store whose ids start at 1.
    let mut source = create_store(None, true).unwrap();
    source
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("load")),
        ))
        .unwrap();
    let json = source
        .export_time_series_associations_openapi(&ListFilter::default())
        .unwrap();

    // The target already holds the array, under a row that took id 1 — so the
    // document's own id 1 is at the high-water mark and cannot be re-filed.
    let mut target = create_store(None, true).unwrap();
    target
        .add(AddRequest::new(
            9,
            "Anchor",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("anchor")),
        ))
        .unwrap();
    let err = target
        .import_time_series_associations_openapi(&json)
        .unwrap_err();
    match err {
        TimeSeriesError::DuplicateAssociationId(id) => assert_eq!(id, 1),
        other => panic!("expected DuplicateAssociationId, got {other:?}"),
    }

    // Same series added twice: still the identity collision it always was.
    let err = source
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("load")),
        ))
        .unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::DuplicateTimeSeries),
        "an identity collision must stay DuplicateTimeSeries, got {err:?}",
    );
}

/// A copy is a new row and gets a new id.
///
/// Regression guard. `copy_time_series` reads the source's metadata, edits the
/// owner in place, and re-inserts it — so the source's id rode along, making
/// every copy an explicit-id insert of an id that was by definition already
/// taken.
#[test]
fn a_copy_gets_its_own_id() {
    let mut store = create_store(None, true).unwrap();
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("load")),
        ))
        .unwrap();
    let source_id = store.get_metadata(&key("load")).unwrap().id.unwrap();

    store
        .copy_time_series(&key("load"), 2, "Generator", None)
        .unwrap();

    let mut copied = key("load");
    copied.owner_id = 2;
    let copy_id = store.get_metadata(&copied).unwrap().id.unwrap();
    assert_ne!(
        copy_id, source_id,
        "a copy must be filed under its own id, not the source's",
    );
    assert_eq!(
        store.get_metadata(&key("load")).unwrap().id,
        Some(source_id),
        "copying must not disturb the source's id",
    );
}

/// A derived `DeterministicSingleTimeSeries` is a new row and gets a new id.
///
/// Regression guard, and the subtler of the two: the transform builds the
/// derived row with `..src`, which fills in every field it does not name — the
/// source's id included, invisibly, with no compiler error to catch it.
#[test]
fn a_derived_view_gets_its_own_id() {
    let mut store = create_store(None, true).unwrap();
    let long = {
        let initial_timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let values: Vec<f64> = (0..24).map(|i| i as f64).collect();
        let data = TypedArray::from_f64(vec![24], &values);
        SingleTimeSeries::new(initial_timestamp, Duration::hours(1), data, "load")
    };
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(long),
        ))
        .unwrap();
    let source_id = store.get_metadata(&key("load")).unwrap().id.unwrap();

    let outcome = store
        .transform_single_time_series(
            Duration::hours(6),
            Duration::hours(6),
            None,
            None,
            TransformPolicy::default(),
        )
        .unwrap();
    assert_eq!(outcome.transformed, 1);

    let derived: Vec<_> = store
        .list_time_series(ListFilter::default())
        .unwrap()
        .into_iter()
        .filter(|m| m.time_series_type == TimeSeriesType::DeterministicSingleTimeSeries)
        .collect();
    assert_eq!(derived.len(), 1);
    assert_ne!(
        derived[0].id,
        Some(source_id),
        "a derived view must be filed under its own id, not its source's",
    );
}

/// `id` is a reserved feature name, so it cannot shadow the metadata field.
#[test]
fn id_cannot_be_used_as_a_feature_name() {
    let mut store = create_store(None, true).unwrap();
    let mut features = Features::new();
    features.insert("id".to_string(), FeatureValue::Int(3));
    let err = store
        .add(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series("load")),
            )
            .with_features(features),
        )
        .unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::InvalidParameter(_)),
        "a reserved feature name must be refused, got {err:?}",
    );
}

// ---------------------------------------------------------------------------
// The write API reports what it wrote
// ---------------------------------------------------------------------------

/// A write hands back the id it was filed under, and it is the same id a read
/// reports. The bulk form does the same, in input order.
#[test]
fn a_write_reports_the_id_it_used() {
    let mut store = create_store(None, true).unwrap();
    let added = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("load")),
        ))
        .unwrap();
    assert_eq!(
        store.get_metadata(added.identity()).unwrap().id,
        Some(added.id)
    );

    let bulk = store
        .add_time_series_bulk(
            ["a", "b", "c"]
                .into_iter()
                .map(|n| {
                    AddRequest::new(
                        2,
                        "Generator",
                        OwnerCategory::Component,
                        TimeSeriesData::SingleTimeSeries(series(n)),
                    )
                })
                .collect(),
        )
        .unwrap();
    let ids: Vec<i64> = bulk.iter().map(|a| a.id).collect();
    assert_eq!(
        ids,
        vec![added.id + 1, added.id + 2, added.id + 3],
        "bulk ids must be assigned in input order",
    );
    for a in &bulk {
        assert_eq!(store.get_metadata(a.identity()).unwrap().id, Some(a.id));
    }
}

/// An imported id cannot re-file a deleted id either. "Never reissued" would
/// be a promise about assigned ids only if the primary key were the sole guard:
/// it refuses an id a *live* row holds and nothing else, so a document carrying
/// a retired id would make a stale reference resolve to a different series. An
/// imported id must therefore sit above the counter, which is also what the
/// `DuplicateAssociationId` message has said all along.
#[test]
fn an_imported_id_cannot_reissue_a_deleted_one() {
    // Source rows at 1 and 2; the document names both.
    let mut source = create_store(None, true).unwrap();
    for name in ["first", "second"] {
        source
            .add(AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(name)),
            ))
            .unwrap();
    }
    let json = source
        .export_time_series_associations_openapi(&ListFilter::default())
        .unwrap();

    // A target that issued id 1 and then deleted that row. The id is retired,
    // not free: the primary key would accept it, and the high-water mark is
    // what refuses it.
    let mut target = create_store(None, true).unwrap();
    let anchor = target
        .add(AddRequest::new(
            9,
            "Anchor",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("anchor")),
        ))
        .unwrap();
    let mut anchor_key = key("anchor");
    anchor_key.owner_id = 9;
    target.remove_time_series(&anchor_key).unwrap();
    assert_eq!(anchor.id, 1);

    // The array is gone with the row, so put it back under another owner; the
    // rows being imported are the ones that do not exist yet.
    target
        .add(AddRequest::new(
            8,
            "Anchor",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("anchor")),
        ))
        .unwrap();

    let err = target
        .import_time_series_associations_openapi(&json)
        .unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::DuplicateAssociationId(id) if id == anchor.id),
        "a retired id must be refused as taken, got {err:?}",
    );
    assert!(!target.association_exists(anchor.id).unwrap());
}

// ---------------------------------------------------------------------------
// The two association catalogs
// ---------------------------------------------------------------------------

fn attach(component_id: i64, attribute_id: i64) -> SupplementalAttributeAssociation {
    SupplementalAttributeAssociation {
        component_id,
        component_type: "Generator".into(),
        attribute_id,
        attribute_type: "GeographicInfo".into(),
        id: None,
    }
}

/// Attaching reports the id, a read reports the same one, and an explicit id is
/// honored over this table's own stream.
#[test]
fn attaching_reports_its_id() {
    let mut store = create_store(None, true).unwrap();
    let first = store
        .add_supplemental_attribute_association(attach(1, 100))
        .unwrap();
    assert_eq!(first, 1);

    let ids = store
        .add_supplemental_attribute_associations(vec![attach(2, 100), attach(3, 100)])
        .unwrap();
    assert_eq!(ids, vec![2, 3], "bulk ids come back in input order");

    // A row that already carries an id — a listing from another store, say — is
    // filed under a fresh one anyway. This catalog's wire form has no id, so
    // there is never a document reference to preserve.
    let mut carried = attach(4, 100);
    carried.id = Some(90);
    assert_eq!(
        store
            .add_supplemental_attribute_association(carried)
            .unwrap(),
        4,
        "the id on the way in is ignored",
    );

    let rows = store
        .list_supplemental_attribute_associations(&SupplementalAttributeFilter::default())
        .unwrap();
    let seen: Vec<Option<i64>> = rows.iter().map(|r| r.id).collect();
    assert_eq!(seen, vec![Some(1), Some(2), Some(3), Some(4)]);
}

/// Two associations describing the same attachment are equal and hash alike
/// whether or not either has been through the catalog.
///
/// Identity here is the endpoint pair — the unique index says so — so folding
/// the id into equality would make a read-back row unequal to the value that
/// produced it, and would break `Hash`'s contract with `Eq` in every set these
/// land in.
#[test]
fn an_association_id_is_outside_equality_and_hashing() {
    use std::collections::HashSet;
    use std::hash::{BuildHasher, RandomState};

    let fresh = attach(1, 100);
    let mut stored = attach(1, 100);
    stored.id = Some(42);

    assert_eq!(fresh, stored);
    let hasher = RandomState::new();
    assert_eq!(
        hasher.hash_one(&fresh),
        hasher.hash_one(&stored),
        "equal values must hash equally",
    );

    let mut set = HashSet::new();
    set.insert(fresh.clone());
    assert!(
        set.contains(&stored),
        "a row read back from the catalog must be found by the value that wrote it",
    );

    // The same holds for the directed-edge table.
    let edge = ParentChildAssociation {
        parent_id: 1,
        parent_type: "Generator".into(),
        child_id: 2,
        child_type: "Bus".into(),
        id: None,
    };
    let mut stored_edge = edge.clone();
    stored_edge.id = Some(7);
    assert_eq!(edge, stored_edge);
    assert_eq!(hasher.hash_one(&edge), hasher.hash_one(&stored_edge));
}

/// The exported attribute-association JSON carries no id, so it still parses
/// through an importer that denies unknown fields.
///
/// The struct gained an `id` and the export used to be its serde derive, which
/// would have put the field on the wire and had the import reject it — an
/// export its own importer refuses.
#[test]
fn the_attribute_association_wire_form_carries_no_id() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![attach(1, 100), attach(2, 101)])
        .unwrap();

    let json = store
        .export_supplemental_attribute_associations_openapi()
        .unwrap();
    assert!(
        !json.contains("\"id\""),
        "the wire form must not carry the catalog id; got {json}",
    );

    let mut fresh = create_store(None, true).unwrap();
    let n = fresh
        .import_supplemental_attribute_associations_openapi(&json)
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        fresh
            .list_supplemental_attribute_associations(&SupplementalAttributeFilter::default())
            .unwrap(),
        store
            .list_supplemental_attribute_associations(&SupplementalAttributeFilter::default())
            .unwrap(),
        "the rows must round trip; ids are assigned fresh and are outside equality",
    );
}

// ---------------------------------------------------------------------------
// Reading by id — the direction that makes a stored reference useful
// ---------------------------------------------------------------------------

/// An id resolves to its row, and an id nothing was filed under resolves to
/// `None` rather than an error: a consumer validating references it persisted
/// earlier is asking a question, and a stale reference is an answer.
#[test]
fn an_id_resolves_to_its_row_or_to_nothing() {
    let mut store = create_store(None, true).unwrap();
    let added = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("load")),
        ))
        .unwrap();

    let meta = store.get_metadata_by_id(added.id).unwrap().unwrap();
    assert_eq!(meta.name, "load");
    assert_eq!(meta.id, Some(added.id));
    assert!(store.association_exists(added.id).unwrap());

    assert!(store.get_metadata_by_id(9_999).unwrap().is_none());
    assert!(!store.association_exists(9_999).unwrap());
}

/// A removed row's id stops resolving, and — because ids are never reissued —
/// never starts resolving to something else.
///
/// This is the guarantee a consumer's stored reference rests on: it can go
/// stale, but it cannot quietly come back meaning a different series.
#[test]
fn a_removed_rows_id_stops_resolving_and_is_not_reused() {
    let mut store = create_store(None, true).unwrap();
    let added = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("load")),
        ))
        .unwrap();
    store.remove_time_series(&key("load")).unwrap();
    assert!(!store.association_exists(added.id).unwrap());

    let replacement = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("load")),
        ))
        .unwrap();
    assert_ne!(replacement.id, added.id);
    assert!(
        !store.association_exists(added.id).unwrap(),
        "the old reference must stay dangling, not resolve to the replacement",
    );
}

/// A bulk read by id returns the series in the order the ids were given,
/// repeats included, and refuses a set containing an id that names no row.
#[test]
fn a_bulk_read_by_id_follows_the_order_it_was_given() {
    let mut store = create_store(None, true).unwrap();
    let mut ids = Vec::new();
    for (name, base) in [("a", 1.0), ("b", 10.0), ("c", 100.0)] {
        let data = TypedArray::from_f64(vec![3], &[base, base + 1.0, base + 2.0]);
        let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let s = SingleTimeSeries::new(initial, Duration::hours(1), data, name);
        ids.push(
            store
                .add(AddRequest::new(
                    1,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(s),
                ))
                .unwrap()
                .id,
        );
    }

    // Reversed, with a repeat: neither the catalog's order nor uniqueness is
    // assumed.
    let asked = vec![ids[2], ids[0], ids[2], ids[1]];
    let got = store.read_by_ids(&asked).unwrap();
    let firsts: Vec<f64> = got
        .iter()
        .map(|d| d.as_single().unwrap().data.to_f64_vec().unwrap()[0])
        .collect();
    assert_eq!(firsts, vec![100.0, 1.0, 100.0, 10.0]);

    let err = store.read_by_ids(&[ids[0], 9_999]).unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::NotFound),
        "an id naming no row must fail the read, got {err:?}",
    );

    assert!(store.read_by_ids(&[]).unwrap().is_empty());
}

/// The ids survive the trip to disk and back — the path IS3.jl's system
/// serialization actually takes, where the catalog is copied rather than
/// rewritten.
#[test]
fn ids_survive_a_persist_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let expected: Vec<(String, i64)> = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let mut out = Vec::new();
        for name in ["first", "second", "third"] {
            let added = store
                .add(AddRequest::new(
                    1,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(series(name)),
                ))
                .unwrap();
            out.push((name.to_string(), added.id));
        }
        store.flush().unwrap();
        out
    };

    let store = open_store(path.as_path(), true).unwrap();
    for (name, id) in &expected {
        let meta = store
            .get_metadata_by_id(*id)
            .unwrap()
            .unwrap_or_else(|| panic!("id {id} did not survive the reopen"));
        assert_eq!(&meta.name, name);
    }
}

// ---------------------------------------------------------------------------
// Deriving one view directly
// ---------------------------------------------------------------------------

/// A 24-step hourly series, long enough to carry a 6-hour forecast window.
fn long_series(name: &str) -> SingleTimeSeries {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let values: Vec<f64> = (0..24).map(|i| i as f64).collect();
    SingleTimeSeries::new(
        initial,
        Duration::hours(1),
        TypedArray::from_f64(vec![24], &values),
        name,
    )
}

fn add_long(store: &mut infrastore_core::Store, name: &str) -> AddedTimeSeries {
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(long_series(name)),
        ))
        .unwrap()
}

/// The sweep reports the id of every view it wrote, so a caller can reference
/// one without listing the store to find it again.
#[test]
fn the_sweep_reports_the_views_it_wrote() {
    let mut store = create_store(None, true).unwrap();
    for name in ["load", "wind"] {
        add_long(&mut store, name);
    }

    let outcome = store
        .transform_single_time_series(
            Duration::hours(6),
            Duration::hours(6),
            None,
            None,
            TransformPolicy::default(),
        )
        .unwrap();
    assert_eq!(outcome.transformed, 2);
    assert_eq!(outcome.written.len(), 2);
    for added in &outcome.written {
        let meta = store.get_metadata_by_id(added.id).unwrap().unwrap();
        assert_eq!(
            meta.time_series_type,
            TimeSeriesType::DeterministicSingleTimeSeries,
        );
        assert_eq!(meta.id, Some(added.id));
    }

    // A dry run writes nothing, and says so.
    let mut fresh = create_store(None, true).unwrap();
    add_long(&mut fresh, "load");
    let rehearsal = fresh
        .transform_single_time_series(
            Duration::hours(6),
            Duration::hours(6),
            None,
            None,
            TransformPolicy {
                dry_run: true,
                ..TransformPolicy::default()
            },
        )
        .unwrap();
    assert_eq!(
        rehearsal.transformed, 1,
        "a rehearsal still reports the count"
    );
    assert!(rehearsal.written.is_empty(), "a rehearsal writes nothing");

    // Re-running the committed sweep is idempotent, and writes nothing the
    // second time.
    let again = store
        .transform_single_time_series(
            Duration::hours(6),
            Duration::hours(6),
            None,
            None,
            TransformPolicy::default(),
        )
        .unwrap();
    assert_eq!(again.transformed, 0);
    assert!(again.written.is_empty());
}

// ---------------------------------------------------------------------------
// The OpenAPI document round trip
// ---------------------------------------------------------------------------

/// Exported rows re-import into a store holding the arrays, ids intact.
///
/// This is the point of putting the id on the wire: an import that assigned
/// fresh ids would leave every reference the document carries pointing at the
/// wrong series.
#[test]
fn a_document_round_trips_with_its_ids() {
    let mut source = create_store(None, true).unwrap();
    // Ids above whatever the target will have issued for its own rows.
    advance_ids(&mut source, 100);
    let mut expected = Vec::new();
    for (owner, name) in [(1, "load"), (2, "wind"), (3, "solar")] {
        let added = source
            .add(AddRequest::new(
                owner,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(name)),
            ))
            .unwrap();
        expected.push((name.to_string(), added.id));
    }
    let json = source
        .export_time_series_associations_openapi(&ListFilter::default())
        .unwrap();
    assert!(
        json.contains("\"association_id\":101"),
        "the wire form must carry the id, under the schema's spelling",
    );

    // A store that already holds the array the document's rows name, under an
    // identity of its own. Arrays are content-addressed, so "the artifact
    // brought the values" is exactly this: the bytes are present, and the rows
    // being imported are the ones that do not exist yet.
    let mut target = create_store(None, true).unwrap();
    target
        .add(AddRequest::new(
            9,
            "Anchor",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("anchor")),
        ))
        .unwrap();

    assert_eq!(
        target
            .import_time_series_associations_openapi(&json)
            .unwrap(),
        3,
    );
    for (name, id) in &expected {
        let meta = target
            .get_metadata_by_id(*id)
            .unwrap()
            .unwrap_or_else(|| panic!("id {id} did not survive the import"));
        assert_eq!(&meta.name, name);
    }
}
/// A read by reference scales past one `IN (...)` list. Each id is a bound
/// variable, and the predicate is bound more than once per statement, so a
/// model-sized set once tripped SQLite's variable limit where the keyed read
/// of the same series did not.
#[test]
fn a_read_by_ids_spans_many_query_chunks() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let mut bulk = store.bulk_add();
    for i in 0..1_200 {
        let data = TypedArray::from_f64(vec![2], &[i as f64, 0.0]);
        bulk.push(AddRequest::new(
            i,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                Duration::hours(1),
                data,
                "load",
            )),
        ));
    }
    let added = bulk.commit().unwrap();
    // Reversed, so the answer's order is visibly the caller's, not the catalog's.
    let asked: Vec<i64> = added.iter().rev().map(|a| a.id).collect();
    let got = store.read_by_ids(&asked).unwrap();
    assert_eq!(got.len(), 1_200);
    let firsts: Vec<f64> = got
        .iter()
        .map(|d| d.as_single().unwrap().data.to_f64_vec().unwrap()[0])
        .collect();
    let expected: Vec<f64> = (0..1_200).rev().map(|i| i as f64).collect();
    assert_eq!(firsts, expected);
}

/// A row that went through the document comes back *identical* — including the
/// native array shape a forecast stores (which the schema's per-step
/// `element_shape` strips) and the time reference. Neither can be rebuilt from
/// the schema's own fields: the forecast layouts are the caller's conventions,
/// and the reference is not on the schema at all.
#[test]
fn an_imported_row_is_identical_to_the_exported_one() {
    let mut source = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
    let values: Vec<f64> = (0..24 * 365).map(|i| i as f64).collect();
    let forecast = infrastore_core::Deterministic::new(
        initial,
        Duration::hours(1),
        Duration::days(1),
        Duration::hours(1),
        365,
        TypedArray::from_f64(vec![24, 365], &values),
        "forecast",
    )
    .unwrap()
    .with_time_reference(infrastore_core::TimeReference::Zone(
        "America/Denver".into(),
    ));
    advance_ids(&mut source, 100);
    let added = source
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::Deterministic(forecast),
        ))
        .unwrap();
    let original = source.get_metadata_by_id(added.id).unwrap().unwrap();
    assert_eq!(
        original.element_shape,
        vec![365],
        "the native shape is what the catalog holds"
    );
    let json = source
        .export_time_series_associations_openapi(&ListFilter::default())
        .unwrap();
    assert!(json.contains("\"array_shape\":[24,365]"), "{json}");
    assert!(
        json.contains("\"time_reference\":\"America/Denver\""),
        "{json}"
    );

    let mut target = create_store(None, true).unwrap();
    // The array, under another owner, so only the row is missing.
    target
        .add(AddRequest::new(
            9,
            "Anchor",
            OwnerCategory::Component,
            TimeSeriesData::Deterministic(
                infrastore_core::Deterministic::new(
                    initial,
                    Duration::hours(1),
                    Duration::days(1),
                    Duration::hours(1),
                    365,
                    TypedArray::from_f64(vec![24, 365], &values),
                    "anchor",
                )
                .unwrap(),
            ),
        ))
        .unwrap();
    target
        .import_time_series_associations_openapi(&json)
        .unwrap();
    let imported = target.get_metadata_by_id(added.id).unwrap().unwrap();
    assert_eq!(imported, original);
}

/// An import is held to the same all-or-none id rule as a bulk add. Rows are
/// not inserted in document order (views go last), so a mixed document's
/// outcome would depend on an order the document's author never saw.
#[test]
fn an_import_refuses_a_document_that_mixes_supplied_and_missing_ids() {
    let mut source = create_store(None, true).unwrap();
    for (owner, name) in [(1, "load"), (2, "wind")] {
        source
            .add(AddRequest::new(
                owner,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(name)),
            ))
            .unwrap();
    }
    let json = source
        .export_time_series_associations_openapi(&ListFilter::default())
        .unwrap();
    let mut rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    rows[1].as_object_mut().unwrap().remove("association_id");
    let mixed = serde_json::to_string(&rows).unwrap();

    let mut target = create_store(None, true).unwrap();
    target
        .add(AddRequest::new(
            9,
            "Anchor",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("anchor")),
        ))
        .unwrap();
    let err = target
        .import_time_series_associations_openapi(&mixed)
        .unwrap_err();
    match err {
        TimeSeriesError::InvalidParameter(msg) => {
            assert!(msg.contains("1 of 2 rows"), "{msg}");
        }
        other => panic!("expected InvalidParameter, got {other:?}"),
    }
    assert_eq!(
        target
            .list_time_series(ListFilter::default())
            .unwrap()
            .len(),
        1,
        "nothing from the document may land",
    );
}

/// A `DeterministicSingleTimeSeries` row is a view of a `SingleTimeSeries`;
/// importing the view without its source would create a state the sweep never
/// produces. The source may arrive in the same document or already be stored;
/// what it may not be is absent.
#[test]
fn an_import_refuses_a_view_without_its_source() {
    let mut source = create_store(None, true).unwrap();
    // High ids, so the document fits a target that has issued ids of its own.
    advance_ids(&mut source, 100);
    source
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(long_series("load")),
        ))
        .unwrap();
    source
        .transform_single_time_series(
            Duration::hours(6),
            Duration::hours(6),
            None,
            None,
            TransformPolicy::default(),
        )
        .unwrap();
    let json = source
        .export_time_series_associations_openapi(&ListFilter::default())
        .unwrap();
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(rows.len(), 2);
    let only_view: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|r| r["time_series_type"] == "DeterministicSingleTimeSeries")
        .collect();
    assert_eq!(only_view.len(), 1);
    let view_only_json = serde_json::to_string(&only_view).unwrap();

    // The array is present (another owner holds the same values), so only
    // the source check stands between the view and the catalog.
    let mut target = create_store(None, true).unwrap();
    target
        .add(AddRequest::new(
            9,
            "Anchor",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(long_series("anchor")),
        ))
        .unwrap();
    let err = target
        .import_time_series_associations_openapi(&view_only_json)
        .unwrap_err();
    match err {
        TimeSeriesError::InvalidParameter(msg) => {
            assert!(
                msg.contains("neither in this document nor already stored"),
                "{msg}"
            );
        }
        other => panic!("expected InvalidParameter, got {other:?}"),
    }
    assert_eq!(
        target
            .list_time_series(ListFilter::default())
            .unwrap()
            .len(),
        1
    );

    // With the source in the same document — in either order — it lands.
    let mut reversed = rows.clone();
    reversed.reverse();
    assert_eq!(
        target
            .import_time_series_associations_openapi(&serde_json::to_string(&reversed).unwrap())
            .unwrap(),
        2
    );
}

/// A row must describe the array it names, not merely name one that exists.
///
/// Holding the array proves nothing about this row's columns: a document is
/// free to point at a real array and declare a geometry it was never hashed
/// from. The row would then report a length or element shape the bytes do not
/// have — metadata and data disagreeing on a static read, and a forecast read
/// failing later with nothing pointing back at the import.
#[test]
fn an_import_refuses_a_row_that_misdescribes_its_array() {
    let mut source = create_store(None, true).unwrap();
    // Above the target's anchor row, so the closing import turns on the shape
    // check alone rather than on an id the target has already issued.
    advance_ids(&mut source, 10);
    source
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("load")),
        ))
        .unwrap();
    let json = source
        .export_time_series_associations_openapi(&ListFilter::default())
        .unwrap();

    // Rewrite the document's declared shape, leaving the hash it names alone.
    let mut rows: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    rows[0]["array_shape"] = serde_json::json!([2]);
    rows[0]["length"] = serde_json::json!(2);
    let doctored = serde_json::to_string(&rows).unwrap();

    // A target holding the real array, so only the geometry check stands
    // between the row and the catalog.
    let mut target = create_store(None, true).unwrap();
    target
        .add(AddRequest::new(
            9,
            "Anchor",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("anchor")),
        ))
        .unwrap();
    let err = target
        .import_time_series_associations_openapi(&doctored)
        .unwrap_err();
    match err {
        TimeSeriesError::InvalidParameter(msg) => {
            assert!(msg.contains("declares shape [2]"), "{msg}");
            assert!(msg.contains("holds with shape [3]"), "{msg}");
        }
        other => panic!("expected InvalidParameter, got {other:?}"),
    }
    // Only the anchor; nothing was written.
    assert_eq!(
        target
            .list_time_series(ListFilter::default())
            .unwrap()
            .len(),
        1
    );

    // The undoctored document imports, so the check is the shape and not the
    // rewrite itself.
    assert_eq!(
        target
            .import_time_series_associations_openapi(&json)
            .unwrap(),
        1
    );
}

/// An import refuses a row naming an array the store does not hold, rather than
/// writing an association that reads back as nothing.
#[test]
fn an_import_refuses_a_row_whose_array_is_absent() {
    let mut source = create_store(None, true).unwrap();
    source
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("load")),
        ))
        .unwrap();
    let json = source
        .export_time_series_associations_openapi(&ListFilter::default())
        .unwrap();

    let mut empty = create_store(None, true).unwrap();
    let err = empty
        .import_time_series_associations_openapi(&json)
        .unwrap_err();
    match err {
        TimeSeriesError::InvalidParameter(msg) => {
            assert!(msg.contains("does not hold"), "{msg}");
            assert!(msg.contains("dangling"), "{msg}");
        }
        other => panic!("expected InvalidParameter, got {other:?}"),
    }
    // Nothing was written.
    assert_eq!(
        empty.list_time_series(ListFilter::default()).unwrap().len(),
        0
    );
}

/// A `NonSequentialTimeSeries` cannot be imported: its timestamp vector is
/// content-addressed in the catalog and deliberately absent from the wire form,
/// so no document holds enough to rebuild the row. Refused with a message that
/// says so, rather than written with the wrong time axis.
#[test]
fn an_import_refuses_an_irregular_row() {
    let mut store = create_store(None, true).unwrap();
    let timestamps = vec![
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2024, 1, 1, 5, 0, 0).unwrap(),
    ];
    let irregular = infrastore_core::NonSequentialTimeSeries::new(
        timestamps,
        TypedArray::from_f64(vec![2], &[1.0, 2.0]),
        "events",
    )
    .unwrap();
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(irregular),
        ))
        .unwrap();
    let json = store
        .export_time_series_associations_openapi(&ListFilter::default())
        .unwrap();

    let err = store
        .import_time_series_associations_openapi(&json)
        .unwrap_err();
    match err {
        TimeSeriesError::InvalidParameter(msg) => {
            assert!(msg.contains("timestamps_hash"), "{msg}");
        }
        other => panic!("expected InvalidParameter, got {other:?}"),
    }
}
