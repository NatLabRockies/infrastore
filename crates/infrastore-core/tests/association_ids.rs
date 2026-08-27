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
    AddRequest, FeatureValue, Features, KeyIdentity, ListFilter, OwnerCategory, Period,
    SingleTimeSeries, TimeSeriesData, TimeSeriesError, TimeSeriesType, TransformPolicy, TypedArray,
    create_store, open_store,
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
                },
            )
            .unwrap();
        store
            .add_parent_child_association(infrastore_core::ParentChildAssociation {
                parent_id: 1,
                parent_type: "Generator".into(),
                child_id: 2,
                child_type: "Bus".into(),
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
                },
            )
            .unwrap();
        store
            .add_parent_child_association(infrastore_core::ParentChildAssociation {
                parent_id: 1,
                parent_type: "Generator".into(),
                child_id: 2,
                child_type: "Bus".into(),
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
