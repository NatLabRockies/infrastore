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
    AddRequest, FeatureValue, Features, KeyIdentity, ListFilter, OwnerCategory,
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
/// table it came from, and the three sequences are never meant to coincide.
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
// Explicit ids, and the row-level paths that must not carry one over
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

/// An explicit id is honored, and the catalog's counter ratchets past it — so a
/// later assigned id cannot land on top of one the caller already placed.
#[test]
fn an_explicit_id_is_honored_and_ratchets_the_counter() {
    let mut store = create_store(None, true).unwrap();
    store
        .add(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series("imported")),
            )
            .with_id(500),
        )
        .unwrap();
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series("assigned")),
        ))
        .unwrap();

    assert_eq!(store.get_metadata(&key("imported")).unwrap().id, Some(500));
    assert_eq!(
        store.get_metadata(&key("assigned")).unwrap().id,
        Some(501),
        "an assigned id must start past the explicit one, not collide with it",
    );
}

/// The two collisions that both arrive as a SQLite constraint violation stay
/// distinguishable. Reporting one as the other sends a caller looking in
/// entirely the wrong place: a duplicate series is a re-add to fix, an id
/// collision means the import's ids do not fit this store.
#[test]
fn an_id_collision_and_an_identity_collision_are_different_errors() {
    let mut store = create_store(None, true).unwrap();
    store
        .add(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series("load")),
            )
            .with_id(7),
        )
        .unwrap();

    // Same id, different series.
    let err = store
        .add(
            AddRequest::new(
                2,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series("other")),
            )
            .with_id(7),
        )
        .unwrap_err();
    match err {
        TimeSeriesError::DuplicateAssociationId(id) => assert_eq!(id, 7),
        other => panic!("expected DuplicateAssociationId, got {other:?}"),
    }

    // Same series, no id: still the identity collision it always was.
    let err = store
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

/// A batch either supplies every id or none. Half of each has no coherent
/// meaning — whether an explicit id collides with an assigned one would depend
/// on the order the items happened to be in.
#[test]
fn a_batch_may_not_mix_supplied_and_assigned_ids() {
    let mut store = create_store(None, true).unwrap();
    let err = store
        .add_time_series_bulk(vec![
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series("supplied")),
            )
            .with_id(10),
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series("assigned")),
            ),
        ])
        .unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::InvalidParameter(_)),
        "a mixed batch must be refused, got {err:?}",
    );
    // All-or-none both pass.
    store
        .add_time_series_bulk(vec![
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series("one")),
            )
            .with_id(10),
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series("two")),
            )
            .with_id(11),
        ])
        .unwrap();
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

    let mut explicit = attach(4, 100);
    explicit.id = Some(90);
    assert_eq!(
        store
            .add_supplemental_attribute_association(explicit)
            .unwrap(),
        90,
    );

    let rows = store
        .list_supplemental_attribute_associations(&SupplementalAttributeFilter::default())
        .unwrap();
    let seen: Vec<Option<i64>> = rows.iter().map(|r| r.id).collect();
    assert_eq!(seen, vec![Some(1), Some(2), Some(3), Some(90)]);
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
    let got = store.bulk_read_by_ids(&asked).unwrap();
    let firsts: Vec<f64> = got
        .iter()
        .map(|d| d.as_single().unwrap().data.to_f64_vec().unwrap()[0])
        .collect();
    assert_eq!(firsts, vec![100.0, 1.0, 100.0, 10.0]);

    let err = store.bulk_read_by_ids(&[ids[0], 9_999]).unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::NotFound),
        "an id naming no row must fail the read, got {err:?}",
    );

    assert!(store.bulk_read_by_ids(&[]).unwrap().is_empty());
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
