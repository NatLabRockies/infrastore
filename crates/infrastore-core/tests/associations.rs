//! Tests for the two association catalogs: supplemental attributes attached to
//! components, and parent/child edges between components. Both replace logic
//! the consumers previously kept in SQLite databases of their own.
//!
//! Associations are independent of time series, so most of these exercise an
//! otherwise empty store.

use infrastore_core::{
    ParentChildAssociation, ParentChildFilter, SupplementalAttributeAssociation,
    SupplementalAttributeFilter, TimeSeriesError, create_store, open_store,
};

/// Generator `component_id` carrying `GeographicInfo` attribute `attribute_id`.
fn attach(component_id: i64, attribute_id: i64) -> SupplementalAttributeAssociation {
    typed_attach(component_id, "Generator", attribute_id, "GeographicInfo")
}

fn typed_attach(
    component_id: i64,
    component_type: &str,
    attribute_id: i64,
    attribute_type: &str,
) -> SupplementalAttributeAssociation {
    SupplementalAttributeAssociation {
        component_id,
        component_type: component_type.into(),
        attribute_id,
        attribute_type: attribute_type.into(),
        id: None,
    }
}

/// Generator `parent_id` connected to Bus `child_id`.
fn edge(parent_id: i64, child_id: i64) -> ParentChildAssociation {
    typed_edge(parent_id, "Generator", child_id, "Bus")
}

fn typed_edge(
    parent_id: i64,
    parent_type: &str,
    child_id: i64,
    child_type: &str,
) -> ParentChildAssociation {
    ParentChildAssociation {
        parent_id,
        parent_type: parent_type.into(),
        child_id,
        child_type: child_type.into(),
        id: None,
    }
}

fn all_attachments() -> SupplementalAttributeFilter {
    SupplementalAttributeFilter::default()
}

fn all_edges() -> ParentChildFilter {
    ParentChildFilter::default()
}

// ---- Supplemental attributes: round trip -----------------------------------

#[test]
fn attach_list_remove_round_trip() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_association(attach(1, 100))
        .unwrap();
    store
        .add_supplemental_attribute_association(attach(2, 100))
        .unwrap();

    assert_eq!(
        store
            .list_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        vec![attach(1, 100), attach(2, 100)]
    );

    let removed = store
        .remove_supplemental_attribute_associations(
            &SupplementalAttributeFilter::new().component_id(1),
        )
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(
        store
            .list_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        vec![attach(2, 100)]
    );
}

#[test]
fn removing_nothing_is_not_an_error() {
    let mut store = create_store(None, true).unwrap();
    assert_eq!(
        store
            .remove_supplemental_attribute_associations(
                &SupplementalAttributeFilter::new().component_id(999)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .remove_parent_child_associations(&ParentChildFilter::new().parent_id(999))
            .unwrap(),
        0
    );
}

#[test]
fn associations_are_independent_of_time_series() {
    // Neither catalog requires a time series to exist, and clearing the series
    // must not disturb either one.
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_association(attach(1, 100))
        .unwrap();
    store.add_parent_child_association(edge(1, 7)).unwrap();
    assert_eq!(store.clear_time_series(None).unwrap(), 0);
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        1
    );
    assert_eq!(
        store.count_parent_child_associations(&all_edges()).unwrap(),
        1
    );
}

#[test]
fn the_two_catalogs_do_not_interfere() {
    // Same integer ids in both tables: clearing one leaves the other intact.
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_association(attach(1, 100))
        .unwrap();
    store.add_parent_child_association(edge(1, 100)).unwrap();

    store
        .remove_supplemental_attribute_associations(&all_attachments())
        .unwrap();
    assert_eq!(
        store.count_parent_child_associations(&all_edges()).unwrap(),
        1
    );
}

// ---- Supplemental attributes: uniqueness -----------------------------------

#[test]
fn duplicate_attachment_is_rejected_regardless_of_type_names() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_association(attach(1, 100))
        .unwrap();

    // Same component and attribute ids, different type names: still the same
    // attachment, because types are denormalized labels, not identity.
    let err = store
        .add_supplemental_attribute_association(typed_attach(1, "Load", 100, "Outage"))
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateAssociation(_)));
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        1
    );
}

#[test]
fn one_attribute_may_be_attached_to_many_components() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![
            attach(1, 100),
            attach(2, 100),
            attach(3, 100),
        ])
        .unwrap();
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        3
    );
    assert_eq!(
        store
            .count_supplemental_attributes(&all_attachments())
            .unwrap(),
        1
    );
}

// ---- Supplemental attributes: filtering ------------------------------------

#[test]
fn attachment_filters_narrow_by_id_and_type() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![
            typed_attach(1, "Generator", 100, "GeographicInfo"),
            typed_attach(1, "Generator", 101, "Outage"),
            typed_attach(2, "Load", 100, "GeographicInfo"),
        ])
        .unwrap();

    assert_eq!(
        store
            .list_supplemental_attribute_associations(
                &SupplementalAttributeFilter::new().component_id(1)
            )
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .list_supplemental_attribute_associations(
                &SupplementalAttributeFilter::new().attribute_id(100)
            )
            .unwrap()
            .len(),
        2
    );
    // Multi-type IN list: the shape IS3 renders after expanding an abstract type.
    assert_eq!(
        store
            .list_supplemental_attribute_associations(
                &SupplementalAttributeFilter::new().attribute_types(["GeographicInfo", "Outage"])
            )
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        store
            .list_supplemental_attribute_associations(
                &SupplementalAttributeFilter::new().component_types(["Load"])
            )
            .unwrap(),
        vec![typed_attach(2, "Load", 100, "GeographicInfo")]
    );
}

#[test]
fn empty_type_list_matches_nothing() {
    // An empty allow-list is a deliberate "none of these", not "no filter" —
    // and SQLite cannot express it as `IN ()`.
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_association(attach(1, 100))
        .unwrap();
    let empty: Vec<String> = Vec::new();
    let filter = SupplementalAttributeFilter::new().attribute_types(empty);
    assert_eq!(
        store
            .list_supplemental_attribute_associations(&filter)
            .unwrap(),
        vec![]
    );
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&filter)
            .unwrap(),
        0
    );
    assert!(
        !store
            .has_supplemental_attribute_association(&filter)
            .unwrap()
    );
}

#[test]
fn has_attachment_covers_every_dispatch_form() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_association(attach(1, 100))
        .unwrap();

    // by pair, by component, by attribute, by attribute type
    let cases = [
        SupplementalAttributeFilter::new()
            .component_id(1)
            .attribute_id(100),
        SupplementalAttributeFilter::new().component_id(1),
        SupplementalAttributeFilter::new().attribute_id(100),
        SupplementalAttributeFilter::new().attribute_types(["GeographicInfo"]),
    ];
    for filter in &cases {
        assert!(
            store
                .has_supplemental_attribute_association(filter)
                .unwrap()
        );
    }
    assert!(
        !store
            .has_supplemental_attribute_association(
                &SupplementalAttributeFilter::new().component_id(7)
            )
            .unwrap()
    );
}

// ---- Supplemental attributes: ids, counts, summary -------------------------

#[test]
fn attachment_ids_and_counts_on_both_sides() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![
            typed_attach(1, "Generator", 100, "GeographicInfo"),
            typed_attach(1, "Generator", 101, "Outage"),
            typed_attach(2, "Load", 100, "GeographicInfo"),
        ])
        .unwrap();

    // Attributes attached to component 1.
    assert_eq!(
        store
            .list_supplemental_attribute_ids(&SupplementalAttributeFilter::new().component_id(1))
            .unwrap(),
        vec![100, 101]
    );
    // Components carrying attribute 100.
    assert_eq!(
        store
            .list_components_with_attributes(&SupplementalAttributeFilter::new().attribute_id(100))
            .unwrap(),
        vec![1, 2]
    );

    let all = all_attachments();
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&all)
            .unwrap(),
        3
    );
    assert_eq!(store.count_supplemental_attributes(&all).unwrap(), 2);
    assert_eq!(store.count_components_with_attributes(&all).unwrap(), 2);
}

#[test]
fn attachment_counts_by_type_and_summary_group_correctly() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![
            typed_attach(1, "Generator", 100, "GeographicInfo"),
            typed_attach(1, "Generator", 101, "Outage"),
            typed_attach(2, "Load", 102, "GeographicInfo"),
        ])
        .unwrap();

    assert_eq!(
        store.supplemental_attribute_counts_by_type().unwrap(),
        vec![("GeographicInfo".to_string(), 2), ("Outage".to_string(), 1)]
    );

    let rows: Vec<(String, String, i64)> = store
        .supplemental_attribute_summary()
        .unwrap()
        .into_iter()
        .map(|r| (r.attribute_type, r.component_type, r.count))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("GeographicInfo".to_string(), "Generator".to_string(), 1),
            ("GeographicInfo".to_string(), "Load".to_string(), 1),
            ("Outage".to_string(), "Generator".to_string(), 1),
        ]
    );
}

// ---- Supplemental attributes: component rewrite ----------------------------

#[test]
fn replace_component_id_moves_every_attachment() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![attach(1, 100), attach(1, 101)])
        .unwrap();

    assert_eq!(
        store
            .replace_supplemental_attribute_component_id(1, 5)
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .list_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        vec![attach(5, 100), attach(5, 101)]
    );
}

#[test]
fn replace_component_id_reports_a_collision() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![attach(1, 100), attach(2, 100)])
        .unwrap();

    let err = store
        .replace_supplemental_attribute_component_id(1, 2)
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateAssociation(_)));
    // The failed update rolled back; both originals survive.
    assert_eq!(
        store
            .list_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        vec![attach(1, 100), attach(2, 100)]
    );
}

// ---- Supplemental attributes: bulk round trip ------------------------------

#[test]
fn attachment_bulk_export_import_round_trips() {
    let mut source = create_store(None, true).unwrap();
    let records = vec![
        typed_attach(1, "Generator", 100, "GeographicInfo"),
        typed_attach(1, "Generator", 101, "Outage"),
        typed_attach(2, "Load", 100, "GeographicInfo"),
    ];
    assert_eq!(
        source
            .add_supplemental_attribute_associations(records.clone())
            .unwrap()
            .len(),
        records.len()
    );
    let exported = source
        .list_supplemental_attribute_associations(&all_attachments())
        .unwrap();
    assert_eq!(exported, records);

    let mut target = create_store(None, true).unwrap();
    target
        .add_supplemental_attribute_associations(exported.clone())
        .unwrap();
    assert_eq!(
        target
            .list_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        exported
    );
}

#[test]
fn attachment_bulk_add_is_all_or_nothing() {
    let mut store = create_store(None, true).unwrap();
    let err = store
        .add_supplemental_attribute_associations(vec![
            attach(1, 100),
            attach(2, 100),
            attach(1, 100),
        ])
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateAssociation(_)));
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        0
    );
}

// ---- Parent/child ----------------------------------------------------------

#[test]
fn parent_child_round_trip() {
    let mut store = create_store(None, true).unwrap();
    store.add_parent_child_association(edge(1, 7)).unwrap();
    store.add_parent_child_association(edge(2, 7)).unwrap();

    assert_eq!(
        store.list_parent_child_associations(&all_edges()).unwrap(),
        vec![edge(1, 7), edge(2, 7)]
    );
    assert_eq!(
        store
            .remove_parent_child_associations(&ParentChildFilter::new().parent_id(1))
            .unwrap(),
        1
    );
    assert_eq!(
        store.list_parent_child_associations(&all_edges()).unwrap(),
        vec![edge(2, 7)]
    );
}

#[test]
fn parent_child_pair_is_unique_but_direction_matters() {
    let mut store = create_store(None, true).unwrap();
    store.add_parent_child_association(edge(1, 7)).unwrap();

    // Same ordered pair under different type names is still the same edge.
    let err = store
        .add_parent_child_association(typed_edge(1, "Load", 7, "Area"))
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateAssociation(_)));

    // The reversed pair is a different edge: identity is ordered.
    store
        .add_parent_child_association(typed_edge(7, "Bus", 1, "Generator"))
        .unwrap();
    assert_eq!(
        store.count_parent_child_associations(&all_edges()).unwrap(),
        2
    );
}

#[test]
fn parents_and_children_are_listed_separately() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_parent_child_associations(vec![
            typed_edge(1, "Generator", 7, "Bus"),
            typed_edge(1, "Generator", 8, "Bus"),
            typed_edge(2, "Load", 7, "Bus"),
        ])
        .unwrap();

    assert_eq!(
        store
            .list_children(&ParentChildFilter::new().parent_id(1))
            .unwrap(),
        vec![7, 8]
    );
    assert_eq!(
        store
            .list_parents(&ParentChildFilter::new().child_id(7))
            .unwrap(),
        vec![1, 2]
    );
    assert_eq!(
        store
            .list_parent_child_associations(&ParentChildFilter::new().parent_types(["Load"]))
            .unwrap(),
        vec![typed_edge(2, "Load", 7, "Bus")]
    );
    assert!(
        store
            .has_parent_child_association(&ParentChildFilter::new().parent_id(1).child_id(8))
            .unwrap()
    );
}

#[test]
fn replace_parent_child_component_id_rewrites_both_ends() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_parent_child_associations(vec![
            typed_edge(1, "Generator", 7, "Bus"),
            typed_edge(9, "Area", 1, "Generator"),
        ])
        .unwrap();

    assert_eq!(store.replace_parent_child_component_id(1, 5).unwrap(), 2);
    assert_eq!(
        store.list_parent_child_associations(&all_edges()).unwrap(),
        vec![
            typed_edge(5, "Generator", 7, "Bus"),
            typed_edge(9, "Area", 5, "Generator")
        ]
    );
}

#[test]
fn replace_parent_child_component_id_counts_a_self_edge_once() {
    // A row with the id on both ends is rewritten by a single statement, so it
    // contributes one to the count rather than one per column.
    let mut store = create_store(None, true).unwrap();
    store
        .add_parent_child_association(typed_edge(1, "Generator", 1, "Generator"))
        .unwrap();
    assert_eq!(store.replace_parent_child_component_id(1, 5).unwrap(), 1);
    assert_eq!(
        store.list_parent_child_associations(&all_edges()).unwrap(),
        vec![typed_edge(5, "Generator", 5, "Generator")]
    );
}

#[test]
fn replace_parent_child_component_id_reports_a_collision() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_parent_child_associations(vec![edge(1, 7), edge(2, 7)])
        .unwrap();

    let err = store.replace_parent_child_component_id(1, 2).unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateAssociation(_)));
    assert_eq!(
        store.list_parent_child_associations(&all_edges()).unwrap(),
        vec![edge(1, 7), edge(2, 7)]
    );
}

#[test]
fn parent_child_bulk_add_is_all_or_nothing() {
    let mut store = create_store(None, true).unwrap();
    let err = store
        .add_parent_child_associations(vec![edge(1, 7), edge(2, 7), edge(1, 7)])
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateAssociation(_)));
    assert_eq!(
        store.count_parent_child_associations(&all_edges()).unwrap(),
        0
    );
}

// ---- Persistence -----------------------------------------------------------

#[test]
fn associations_survive_persist_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(None, true).unwrap();
        store
            .add_supplemental_attribute_associations(vec![attach(1, 100), attach(2, 101)])
            .unwrap();
        store
            .add_parent_child_associations(vec![edge(1, 7), edge(2, 8)])
            .unwrap();
        store.persist_to(path.as_path()).unwrap();
    }

    let store = open_store(path.as_path(), true).unwrap();
    assert_eq!(
        store
            .list_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        vec![attach(1, 100), attach(2, 101)]
    );
    assert_eq!(
        store.list_parent_child_associations(&all_edges()).unwrap(),
        vec![edge(1, 7), edge(2, 8)]
    );
}

#[test]
fn read_only_store_rejects_association_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        store
            .add_supplemental_attribute_association(attach(1, 100))
            .unwrap();
        store.add_parent_child_association(edge(1, 7)).unwrap();
        store.flush().unwrap();
    }

    let mut store = open_store(path.as_path(), true).unwrap();
    assert!(matches!(
        store
            .add_supplemental_attribute_association(attach(2, 100))
            .unwrap_err(),
        TimeSeriesError::ReadOnlyStore
    ));
    assert!(matches!(
        store
            .remove_supplemental_attribute_associations(&all_attachments())
            .unwrap_err(),
        TimeSeriesError::ReadOnlyStore
    ));
    assert!(matches!(
        store
            .replace_supplemental_attribute_component_id(1, 2)
            .unwrap_err(),
        TimeSeriesError::ReadOnlyStore
    ));
    assert!(matches!(
        store.add_parent_child_association(edge(2, 7)).unwrap_err(),
        TimeSeriesError::ReadOnlyStore
    ));
    assert!(matches!(
        store
            .remove_parent_child_associations(&all_edges())
            .unwrap_err(),
        TimeSeriesError::ReadOnlyStore
    ));
    assert!(matches!(
        store.replace_parent_child_component_id(1, 2).unwrap_err(),
        TimeSeriesError::ReadOnlyStore
    ));
    // Reads still work.
    assert_eq!(
        store
            .list_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        vec![attach(1, 100)]
    );
    assert_eq!(
        store.list_parent_child_associations(&all_edges()).unwrap(),
        vec![edge(1, 7)]
    );
}

// ---- Backward compatibility ------------------------------------------------

#[test]
fn read_only_open_of_a_pre_associations_store_reads_empty() {
    // The association tables were added without a DATA_FORMAT_VERSION bump, so a
    // store written by an older build stays readable. A writable open recreates
    // them through the idempotent DDL; a read-only open cannot, and must degrade
    // to empty answers rather than erroring.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    drop(create_store(Some(path.as_path()), false).unwrap());

    let sqlite_path = {
        let mut p = path.clone().into_os_string();
        p.push(".sqlite");
        std::path::PathBuf::from(p)
    };
    {
        let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
        conn.execute_batch(
            "DROP TABLE supplemental_attribute_associations;
             DROP TABLE parent_child_associations;",
        )
        .unwrap();
    }

    {
        let store = open_store(path.as_path(), true).unwrap();
        let attachments = all_attachments();
        assert_eq!(
            store
                .list_supplemental_attribute_associations(&attachments)
                .unwrap(),
            vec![]
        );
        assert_eq!(
            store.list_supplemental_attribute_ids(&attachments).unwrap(),
            Vec::<i64>::new()
        );
        assert_eq!(
            store.list_components_with_attributes(&attachments).unwrap(),
            Vec::<i64>::new()
        );
        assert!(
            !store
                .has_supplemental_attribute_association(&attachments)
                .unwrap()
        );
        assert_eq!(
            store
                .count_supplemental_attribute_associations(&attachments)
                .unwrap(),
            0
        );
        assert_eq!(
            store.count_supplemental_attributes(&attachments).unwrap(),
            0
        );
        assert_eq!(
            store
                .count_components_with_attributes(&attachments)
                .unwrap(),
            0
        );
        assert_eq!(
            store.supplemental_attribute_counts_by_type().unwrap(),
            vec![]
        );
        assert_eq!(store.supplemental_attribute_summary().unwrap(), vec![]);

        let edges = all_edges();
        assert_eq!(
            store.list_parent_child_associations(&edges).unwrap(),
            vec![]
        );
        assert_eq!(store.list_children(&edges).unwrap(), Vec::<i64>::new());
        assert_eq!(store.list_parents(&edges).unwrap(), Vec::<i64>::new());
        assert!(!store.has_parent_child_association(&edges).unwrap());
        assert_eq!(store.count_parent_child_associations(&edges).unwrap(), 0);
    }

    // Opening the same store for writing restores both tables, and they work.
    let mut store = open_store(path.as_path(), false).unwrap();
    store
        .add_supplemental_attribute_association(attach(1, 100))
        .unwrap();
    store.add_parent_child_association(edge(1, 7)).unwrap();
    assert_eq!(
        store
            .list_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        vec![attach(1, 100)]
    );
    assert_eq!(
        store.list_parent_child_associations(&all_edges()).unwrap(),
        vec![edge(1, 7)]
    );
}

// ---- serde -----------------------------------------------------------------

#[test]
fn association_serde_round_trips() {
    let attachment = attach(1, 100);
    let json = serde_json::to_string(&attachment).unwrap();
    assert_eq!(
        serde_json::from_str::<SupplementalAttributeAssociation>(&json).unwrap(),
        attachment
    );

    let e = edge(1, 7);
    let json = serde_json::to_string(&e).unwrap();
    assert_eq!(
        serde_json::from_str::<ParentChildAssociation>(&json).unwrap(),
        e
    );
}

// ---- Index rename ----------------------------------------------------------

#[test]
fn opening_for_writing_renames_the_legacy_time_series_indexes() {
    // The time-series uniqueness indexes were renamed uq_assoc ->
    // uq_ts_assoc (and _coalesced) once a bare "assoc" became ambiguous. The
    // DDL renames them in place on first writable open; leaving the old pair
    // behind would silently double the index maintenance on every insert.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    drop(create_store(Some(path.as_path()), false).unwrap());

    let sqlite_path = {
        let mut p = path.clone().into_os_string();
        p.push(".sqlite");
        std::path::PathBuf::from(p)
    };
    let index_names = |conn: &rusqlite::Connection| -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    };

    // Fabricate a store carrying the pre-rename index names.
    {
        let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
        conn.execute_batch(
            "DROP INDEX uq_ts_assoc;
             DROP INDEX uq_ts_assoc_coalesced;
             CREATE UNIQUE INDEX uq_assoc ON time_series_associations
                 (owner_id, owner_category, time_series_type, name, resolution, interval,
                  features_hash);
             CREATE UNIQUE INDEX uq_assoc_coalesced ON time_series_associations
                 (owner_id, owner_category, time_series_type, name,
                  COALESCE(resolution, ''), COALESCE(interval, ''), features_hash);",
        )
        .unwrap();
        let names = index_names(&conn);
        assert!(names.iter().any(|n| n == "uq_assoc"));
        assert!(!names.iter().any(|n| n == "uq_ts_assoc"));
    }

    drop(open_store(path.as_path(), false).unwrap());

    let conn = rusqlite::Connection::open(&sqlite_path).unwrap();
    let names = index_names(&conn);
    assert!(names.iter().any(|n| n == "uq_ts_assoc"));
    assert!(names.iter().any(|n| n == "uq_ts_assoc_coalesced"));
    assert!(
        !names
            .iter()
            .any(|n| n == "uq_assoc" || n == "uq_assoc_coalesced"),
        "legacy indexes survived the rename: {names:?}"
    );
}

// ===========================================================================
// Association edge cases
// ===========================================================================

#[test]
fn a_supplemental_self_pair_is_accepted() {
    // PIN: nothing rejects `component_id == attribute_id`. The catalog stores
    // two opaque ids and a pair identity; it has no notion that a component and
    // an attribute come from different id spaces, so a self-pair is a legal row.
    // A consumer that shares one id space across both would silently create it.
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_association(attach(5, 5))
        .unwrap();

    assert_eq!(
        store
            .list_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        vec![attach(5, 5)]
    );
    // It is visible from both directions of the same id.
    assert_eq!(
        store
            .list_supplemental_attribute_ids(&SupplementalAttributeFilter::new().component_id(5))
            .unwrap(),
        vec![5]
    );
    assert_eq!(
        store
            .list_components_with_attributes(&SupplementalAttributeFilter::new().attribute_id(5))
            .unwrap(),
        vec![5]
    );
    // And the pair identity still applies: adding it again collides.
    assert!(matches!(
        store.add_supplemental_attribute_association(attach(5, 5)),
        Err(TimeSeriesError::DuplicateAssociation(_))
    ));
}

#[test]
fn a_parent_child_self_edge_is_accepted() {
    // PIN: a component may be its own parent. The identity is the ordered pair,
    // so `(5, 5)` is a legal — if physically meaningless — edge, and it appears
    // as both a child of and a parent of 5.
    let mut store = create_store(None, true).unwrap();
    store.add_parent_child_association(edge(5, 5)).unwrap();

    assert_eq!(
        store
            .list_children(&ParentChildFilter::new().parent_id(5))
            .unwrap(),
        vec![5]
    );
    assert_eq!(
        store
            .list_parents(&ParentChildFilter::new().child_id(5))
            .unwrap(),
        vec![5]
    );
    assert!(matches!(
        store.add_parent_child_association(edge(5, 5)),
        Err(TimeSeriesError::DuplicateAssociation(_))
    ));
}

#[test]
fn replace_component_id_with_old_equal_to_new_is_a_self_move() {
    // PIN: `old == new` rewrites each matching row with the value it already
    // has. It must report the rows it touched (not zero) and must not trip the
    // collision check against itself.
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![attach(1, 10), attach(1, 11), attach(2, 10)])
        .unwrap();

    let moved = store
        .replace_supplemental_attribute_component_id(1, 1)
        .unwrap();
    assert_eq!(moved, 2, "both of component 1's attachments were rewritten");

    // Nothing changed.
    let mut rows = store
        .list_supplemental_attribute_associations(&all_attachments())
        .unwrap();
    rows.sort_by_key(|r| (r.component_id, r.attribute_id));
    assert_eq!(rows, vec![attach(1, 10), attach(1, 11), attach(2, 10)]);
}

#[test]
fn replace_parent_child_component_id_with_old_equal_to_new_is_a_self_move() {
    let mut store = create_store(None, true).unwrap();
    // Component 1 appears as a parent once and as a child once, so a self-move
    // must find it on both sides without colliding.
    store
        .add_parent_child_associations(vec![edge(1, 20), edge(30, 1)])
        .unwrap();

    let moved = store.replace_parent_child_component_id(1, 1).unwrap();
    assert_eq!(moved, 2, "both endpoints referencing 1 were rewritten");

    let mut rows = store.list_parent_child_associations(&all_edges()).unwrap();
    rows.sort_by_key(|r| (r.parent_id, r.child_id));
    assert_eq!(rows, vec![edge(1, 20), edge(30, 1)]);
}

#[test]
fn replace_component_id_for_an_unreferenced_id_is_zero() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![attach(1, 10), attach(2, 11)])
        .unwrap();

    assert_eq!(
        store
            .replace_supplemental_attribute_component_id(999, 1000)
            .unwrap(),
        0
    );
    // The attachments are untouched, and 1000 gained nothing.
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        2
    );
    assert!(
        store
            .list_supplemental_attribute_ids(&SupplementalAttributeFilter::new().component_id(1000))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn replace_parent_child_component_id_for_an_unreferenced_id_is_zero() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_parent_child_associations(vec![edge(1, 20), edge(2, 21)])
        .unwrap();

    assert_eq!(
        store.replace_parent_child_component_id(999, 1000).unwrap(),
        0
    );
    assert_eq!(
        store.count_parent_child_associations(&all_edges()).unwrap(),
        2
    );
}

#[test]
fn a_type_list_filter_with_over_a_thousand_entries_still_works() {
    // The type filters render as a literal SQL `IN (?, ?, …)` list, one bind
    // variable per entry. SQLite's default `SQLITE_MAX_VARIABLE_NUMBER` is the
    // ceiling that caps how many an expanded abstract type may contribute; a
    // caller expanding a wide hierarchy can plausibly reach four figures. Pin
    // that ~1,200 entries is under the limit and still selects correctly.
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![
            typed_attach(1, "Generator", 10, "GeographicInfo"),
            typed_attach(2, "Bus", 11, "Outage"),
        ])
        .unwrap();

    // 1,200 names, exactly one of which exists in the catalog.
    let mut component_types: Vec<String> = (0..1_199).map(|i| format!("Synthetic{i}")).collect();
    component_types.push("Generator".to_string());
    assert_eq!(component_types.len(), 1_200);

    let filter = SupplementalAttributeFilter::new().component_types(component_types.clone());
    let rows = store
        .list_supplemental_attribute_associations(&filter)
        .unwrap();
    assert_eq!(
        rows,
        vec![typed_attach(1, "Generator", 10, "GeographicInfo")]
    );
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&filter)
            .unwrap(),
        1
    );
    assert!(
        store
            .has_supplemental_attribute_association(&filter)
            .unwrap()
    );

    // A 1,200-entry list of names that match nothing selects nothing (rather
    // than erroring or degenerating into "match all").
    let none = SupplementalAttributeFilter::new()
        .component_types((0..1_200).map(|i| format!("Synthetic{i}")));
    assert!(
        store
            .list_supplemental_attribute_associations(&none)
            .unwrap()
            .is_empty()
    );

    // Both sides of the filter, and the parent/child catalog, take the same path.
    let both = SupplementalAttributeFilter::new()
        .component_types(component_types)
        .attribute_types(
            (0..1_199)
                .map(|i| format!("Synthetic{i}"))
                .chain(["GeographicInfo".to_string()]),
        );
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&both)
            .unwrap(),
        1
    );

    store
        .add_parent_child_associations(vec![edge(1, 20)])
        .unwrap();
    let wide_edges = ParentChildFilter::new().parent_types(
        (0..1_199)
            .map(|i| format!("Synthetic{i}"))
            .chain(["Generator".to_string()]),
    );
    assert_eq!(
        store.list_parent_child_associations(&wide_edges).unwrap(),
        vec![edge(1, 20)]
    );
}

/// Brute-force the counts a fan-in / fan-out graph should produce, straight from
/// the association list, so the SQL aggregates are checked against an
/// independent computation rather than a hand-copied number.
#[test]
fn counts_and_summary_match_brute_force_on_a_fan_in_and_fan_out_graph() {
    use std::collections::{BTreeMap, BTreeSet};

    // Fan-in: one attribute (9000, "SharedInfo") on 50 components 1..=50.
    // Fan-out: one component (7000) carrying 50 attributes 1..=50.
    // The two overlap nowhere, so every expected count is a clean sum.
    let mut rows = Vec::new();
    for c in 1..=50i64 {
        rows.push(typed_attach(c, "Generator", 9_000, "SharedInfo"));
    }
    for a in 1..=50i64 {
        rows.push(typed_attach(7_000, "Bus", a, "PerBusInfo"));
    }

    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(rows.clone())
        .unwrap();

    // ---- brute force ----
    let total = rows.len() as i64;
    let distinct_attributes = rows.iter().map(|r| r.attribute_id).collect::<BTreeSet<_>>();
    let distinct_components = rows.iter().map(|r| r.component_id).collect::<BTreeSet<_>>();
    let mut by_type: BTreeMap<&str, i64> = BTreeMap::new();
    let mut by_pair: BTreeMap<(&str, &str), i64> = BTreeMap::new();
    for r in &rows {
        *by_type.entry(r.attribute_type.as_str()).or_default() += 1;
        *by_pair
            .entry((r.component_type.as_str(), r.attribute_type.as_str()))
            .or_default() += 1;
    }

    // ---- totals ----
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        total
    );
    assert_eq!(
        store
            .count_supplemental_attributes(&all_attachments())
            .unwrap(),
        distinct_attributes.len() as i64,
        "distinct attributes"
    );
    assert_eq!(
        store
            .count_components_with_attributes(&all_attachments())
            .unwrap(),
        distinct_components.len() as i64,
        "distinct components"
    );

    // ---- counts by type ----
    let mut got: Vec<(String, i64)> = store.supplemental_attribute_counts_by_type().unwrap();
    got.sort();
    let mut want: Vec<(String, i64)> = by_type
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    want.sort();
    assert_eq!(got, want);

    // ---- grouped summary ----
    let mut got: Vec<(String, String, i64)> = store
        .supplemental_attribute_summary()
        .unwrap()
        .into_iter()
        .map(|r| (r.component_type, r.attribute_type, r.count))
        .collect();
    got.sort();
    let mut want: Vec<(String, String, i64)> = by_pair
        .into_iter()
        .map(|((c, a), n)| (c.to_string(), a.to_string(), n))
        .collect();
    want.sort();
    assert_eq!(got, want);

    // ---- the fan-in side, per-attribute ----
    let fan_in = SupplementalAttributeFilter::new().attribute_id(9_000);
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&fan_in)
            .unwrap(),
        50
    );
    assert_eq!(
        store.count_components_with_attributes(&fan_in).unwrap(),
        50,
        "50 components share the one attribute"
    );
    assert_eq!(store.count_supplemental_attributes(&fan_in).unwrap(), 1);
    let mut components = store.list_components_with_attributes(&fan_in).unwrap();
    components.sort();
    assert_eq!(components, (1..=50i64).collect::<Vec<_>>());

    // ---- the fan-out side, per-component ----
    let fan_out = SupplementalAttributeFilter::new().component_id(7_000);
    assert_eq!(
        store
            .count_supplemental_attribute_associations(&fan_out)
            .unwrap(),
        50
    );
    assert_eq!(store.count_supplemental_attributes(&fan_out).unwrap(), 50);
    assert_eq!(store.count_components_with_attributes(&fan_out).unwrap(), 1);
    let mut attributes = store.list_supplemental_attribute_ids(&fan_out).unwrap();
    attributes.sort();
    assert_eq!(attributes, (1..=50i64).collect::<Vec<_>>());

    // ---- and the aggregates survive a disk round trip ----
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assoc.h5");
    store.persist_to(path.as_path()).unwrap();
    let reopened = open_store(path.as_path(), true).unwrap();
    assert_eq!(
        reopened
            .count_supplemental_attribute_associations(&all_attachments())
            .unwrap(),
        total
    );
    assert_eq!(
        reopened.supplemental_attribute_summary().unwrap(),
        store.supplemental_attribute_summary().unwrap()
    );
}

#[test]
fn parent_child_counts_match_brute_force_on_a_fan_in_and_fan_out_graph() {
    use std::collections::BTreeSet;

    // Fan-out: one parent 7000 with 50 children. Fan-in: one child 9000 with 50
    // parents. Both directions must aggregate independently.
    let mut rows = Vec::new();
    for c in 1..=50i64 {
        rows.push(typed_edge(7_000, "Generator", c, "Bus"));
    }
    for p in 100..150i64 {
        rows.push(typed_edge(p, "Generator", 9_000, "Bus"));
    }

    let mut store = create_store(None, true).unwrap();
    store.add_parent_child_associations(rows.clone()).unwrap();

    assert_eq!(
        store.count_parent_child_associations(&all_edges()).unwrap(),
        rows.len() as i64
    );

    let mut children = store
        .list_children(&ParentChildFilter::new().parent_id(7_000))
        .unwrap();
    children.sort();
    assert_eq!(children, (1..=50i64).collect::<Vec<_>>());
    assert_eq!(
        store
            .count_parent_child_associations(&ParentChildFilter::new().parent_id(7_000))
            .unwrap(),
        50
    );

    let mut parents = store
        .list_parents(&ParentChildFilter::new().child_id(9_000))
        .unwrap();
    parents.sort();
    assert_eq!(parents, (100..150i64).collect::<Vec<_>>());
    assert_eq!(
        store
            .count_parent_child_associations(&ParentChildFilter::new().child_id(9_000))
            .unwrap(),
        50
    );

    // `list_children` / `list_parents` return distinct ids, so the unfiltered
    // forms are the id sets, not the row count.
    let expected_children: BTreeSet<i64> = rows.iter().map(|r| r.child_id).collect();
    let mut all_children = store.list_children(&all_edges()).unwrap();
    all_children.sort();
    assert_eq!(
        all_children,
        expected_children.into_iter().collect::<Vec<_>>()
    );
}

/// Reassigning a component brings its denormalized type label with it.
///
/// `component_type` / `parent_type` / `child_type` are carried for filtering,
/// and the reassignment used to rewrite only the id. The moved rows went on
/// describing the component they came from, so filtering by the destination's
/// real type missed them, filtering by the source's type returned them under the
/// destination's id, and `supplemental_attribute_summary` split one component
/// across two contradictory type buckets.
///
/// The destination's type comes from the rows it already has. Where it has none
/// the catalog has no other record of it — these rows become its only ones — so
/// the label carries over unchanged; that case is documented rather than
/// guessed at.
#[test]
fn reassigning_a_component_relabels_the_rows_it_moves() {
    let mut store = create_store(None, true).unwrap();
    store
        .add_supplemental_attribute_associations(vec![
            typed_attach(1, "ThermalStandard", 10, "GeographicInfo"),
            typed_attach(2, "RenewableDispatch", 20, "GeographicInfo"),
        ])
        .unwrap();

    assert_eq!(
        store
            .replace_supplemental_attribute_component_id(1, 2)
            .unwrap(),
        1
    );

    // Component 2 is a RenewableDispatch, and now every row says so.
    let rows = store
        .list_supplemental_attribute_associations(&all_attachments())
        .unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.component_id, 2);
        assert_eq!(row.component_type, "RenewableDispatch", "{row:?}");
    }

    // Which is what the type filters and the summary see.
    let by = |t: &str| {
        store
            .list_supplemental_attribute_associations(
                &SupplementalAttributeFilter::new().component_types([t.to_string()]),
            )
            .unwrap()
            .len()
    };
    assert_eq!(by("RenewableDispatch"), 2);
    assert_eq!(by("ThermalStandard"), 0);
    let summary = store.supplemental_attribute_summary().unwrap();
    assert_eq!(summary.len(), 1, "{summary:?}");
    assert_eq!(summary[0].component_type, "RenewableDispatch");

    // The directed-edge catalog follows the same rule, on whichever end moves.
    let mut store = create_store(None, true).unwrap();
    store
        .add_parent_child_associations(vec![
            typed_edge(1, "ThermalStandard", 100, "Bus"),
            typed_edge(2, "RenewableDispatch", 101, "Bus"),
            typed_edge(200, "Bus", 1, "ThermalStandard"),
        ])
        .unwrap();
    store.replace_parent_child_component_id(1, 2).unwrap();

    let edges = store
        .list_parent_child_associations(&ParentChildFilter::new())
        .unwrap();
    for e in &edges {
        if e.parent_id == 2 {
            assert_eq!(e.parent_type, "RenewableDispatch", "{e:?}");
        }
        if e.child_id == 2 {
            assert_eq!(e.child_type, "RenewableDispatch", "{e:?}");
        }
        // The end that did not move keeps its own label.
        if e.parent_id == 200 {
            assert_eq!(e.parent_type, "Bus", "{e:?}");
        }
    }
    assert_eq!(
        store
            .list_parent_child_associations(
                &ParentChildFilter::new().parent_types(["ThermalStandard".to_string()])
            )
            .unwrap()
            .len(),
        0,
        "nothing should still claim the source's type"
    );
}
