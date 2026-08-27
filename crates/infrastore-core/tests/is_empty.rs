//! Tests for `Store::is_empty` — the store-wide emptiness predicate.
//!
//! The contract that matters is *coverage*: emptiness must account for every
//! persistent table, not just the ones a caller happens to know about. A
//! consumer (InfrastructureSystems.jl) skips writing the artifact entirely when
//! the store reports empty, so a table left out of the predicate is silently
//! dropped on round trip. Each catalog therefore gets a case of its own, held
//! in isolation.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    KeyIdentity, ListFilter, OwnerCategory, ParentChildAssociation, ParentChildFilter,
    SingleTimeSeries, SupplementalAttributeAssociation, SupplementalAttributeFilter,
    TimeSeriesData, TimeSeriesType, TypedArray, create_store,
};

mod common;
use common::{for_each_backend, for_each_backend_mut};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
}

fn sts(name: &str) -> TimeSeriesData {
    TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![4], &[1.0, 2.0, 3.0, 4.0]),
        name,
    ))
}

fn attach(component_id: i64, attribute_id: i64) -> SupplementalAttributeAssociation {
    SupplementalAttributeAssociation {
        component_id,
        component_type: "Generator".into(),
        attribute_id,
        attribute_type: "GeographicInfo".into(),
        id: None,
    }
}

fn edge(parent_id: i64, child_id: i64) -> ParentChildAssociation {
    ParentChildAssociation {
        parent_id,
        parent_type: "Generator".into(),
        child_id,
        child_type: "Bus".into(),
        id: None,
    }
}

#[test]
fn fresh_store_is_empty() {
    for_each_backend(
        |_store| (),
        |store, _, backend| {
            assert!(store.is_empty().unwrap(), "{backend}");
        },
    );
}

#[test]
fn time_series_alone_makes_the_store_non_empty() {
    for_each_backend_mut(
        |store| {
            store
                .add_time_series(
                    1,
                    "Generator",
                    OwnerCategory::Component,
                    sts("load"),
                    Default::default(),
                )
                .unwrap()
        },
        |store, key, backend| {
            assert!(!store.is_empty().unwrap(), "{backend}");
            // Emptiness comes back once the last series goes.
            store
                .remove_time_series(&KeyIdentity {
                    owner_id: 1,
                    owner_category: OwnerCategory::Component,
                    time_series_type: TimeSeriesType::SingleTimeSeries,
                    name: key.key.name().to_string(),
                    resolution: key.key.resolution(),
                    interval: None,
                    features: key.key.features().clone(),
                })
                .unwrap();
            assert!(store.is_empty().unwrap(), "{backend}");
        },
    );
}

#[test]
fn supplemental_attribute_associations_alone_make_the_store_non_empty() {
    for_each_backend_mut(
        |store| {
            store
                .add_supplemental_attribute_associations(vec![attach(1, 100)])
                .unwrap();
        },
        |store, _, backend| {
            assert!(!store.is_empty().unwrap(), "{backend}");
            store
                .remove_supplemental_attribute_associations(&SupplementalAttributeFilter::default())
                .unwrap();
            assert!(store.is_empty().unwrap(), "{backend}");
        },
    );
}

/// The case the client-side conjunction gets wrong: `parent_child_associations`
/// is unaccounted for outside the store, so a store holding only edges reports
/// empty to a caller that only knows the other two tables.
#[test]
fn parent_child_associations_alone_make_the_store_non_empty() {
    for_each_backend_mut(
        |store| {
            store
                .add_parent_child_associations(vec![edge(1, 100)])
                .unwrap();
        },
        |store, _, backend| {
            assert!(!store.is_empty().unwrap(), "{backend}");
            store
                .remove_parent_child_associations(&ParentChildFilter::default())
                .unwrap();
            assert!(store.is_empty().unwrap(), "{backend}");
        },
    );
}

/// With all three catalogs populated, emptiness returns only after the last
/// one is drained — no single table short-circuits the answer.
#[test]
fn emptiness_returns_only_after_the_last_table_drains() {
    let mut store = create_store(None, true).unwrap();
    assert!(store.is_empty().unwrap());

    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            sts("load"),
            Default::default(),
        )
        .unwrap();
    store
        .add_supplemental_attribute_associations(vec![attach(1, 100)])
        .unwrap();
    store
        .add_parent_child_associations(vec![edge(1, 200)])
        .unwrap();
    assert!(!store.is_empty().unwrap());

    assert_eq!(store.remove_by_filter(ListFilter::new()).unwrap(), 1);
    assert!(
        !store.is_empty().unwrap(),
        "attachments and edges still remain"
    );

    store
        .remove_supplemental_attribute_associations(&SupplementalAttributeFilter::default())
        .unwrap();
    assert!(!store.is_empty().unwrap(), "edges still remain");

    store
        .remove_parent_child_associations(&ParentChildFilter::default())
        .unwrap();
    assert!(store.is_empty().unwrap());
}
