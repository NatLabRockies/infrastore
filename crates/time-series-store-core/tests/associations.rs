//! Tests for the two association catalogs: supplemental attributes attached to
//! components, and parent/child edges between components. Both replace logic
//! the consumers previously kept in SQLite databases of their own.
//!
//! Associations are independent of time series, so most of these exercise an
//! otherwise empty store.

use time_series_store_core::{
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
            .unwrap(),
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
    let path = dir.path().join("store.nc");
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
    let path = dir.path().join("store.nc");
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
    let path = dir.path().join("store.nc");
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
    let path = dir.path().join("store.nc");
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
