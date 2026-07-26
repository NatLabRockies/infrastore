//! Content-addressed array sharing and reference-count accounting that cut
//! across time-series types:
//!
//! * `count_array_references` beyond the `(1, 1)` STS+DST case already covered
//!   in `forecasts.rs` — multi-STS `(N, 0)`, DST-without-STS `(0, 1)`, and
//!   counts that change as references are removed.
//! * The persist-time layout planner promoting a hash shared by a *packed*
//!   (`SingleTimeSeries`) and a *standalone* (`NonSequentialTimeSeries`) key to
//!   standalone, so both read back correctly (`store.rs` `plans` logic).
//! * `rename_time_series` leaving the backing array, its hash, and the shared
//!   reference count untouched.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Features, NonSequentialTimeSeries, OwnerCategory, SingleTimeSeries, Store, TimeSeriesData,
    TimeSeriesType, TypedArray, create_store, open_store,
};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

/// A length-8 hourly `SingleTimeSeries` named "load"; long enough that a
/// horizon-2/interval-1 transform derives a valid DST view.
fn sts_series() -> SingleTimeSeries {
    let vals: Vec<f64> = (0..8).map(|i| i as f64).collect();
    SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![8], &vals),
        "load",
    )
}

fn add_sts(store: &mut Store, owner: i64) -> infrastore_core::TimeSeriesKey {
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts_series()),
            Features::new(),
            None,
        )
        .unwrap()
}

// --- count_array_references: cases beyond (1, 1) --------------------------------

#[test]
fn count_array_references_multiple_sts_then_decrements() {
    let mut store = create_store(None, true).unwrap();
    // Two SingleTimeSeries with identical data share one array; no DST exists.
    let k1 = add_sts(&mut store, 1);
    let k2 = add_sts(&mut store, 2);
    let hash = store.get_metadata(k1.identity()).unwrap().data_hash;

    assert_eq!(store.count_array_references(&hash).unwrap(), (2, 0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);

    // Removing one sharer drops the count to (1, 0); the array survives.
    store.remove_time_series(k1.identity()).unwrap();
    assert_eq!(store.count_array_references(&hash).unwrap(), (1, 0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);

    // Removing the last reference clears the count and reclaims the array.
    store.remove_time_series(k2.identity()).unwrap();
    assert_eq!(store.count_array_references(&hash).unwrap(), (0, 0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 0);
}

#[test]
fn removing_the_last_sts_backing_a_dst_is_refused() {
    let mut store = create_store(None, true).unwrap();
    let sts_key = add_sts(&mut store, 5);

    // Derive a single DST view sharing the STS's array.
    let derived = store
        .transform_single_time_series(Duration::hours(2), Duration::hours(1), None, None)
        .unwrap();
    assert_eq!(derived, 1);

    let hash = store.get_metadata(sts_key.identity()).unwrap().data_hash;
    assert_eq!(store.count_array_references(&hash).unwrap(), (1, 1));

    // A DST is a view over the STS's array: removing its last backing
    // SingleTimeSeries would orphan it, so the remove is refused and rolled
    // back.
    let err = store.remove_time_series(sts_key.identity()).unwrap_err();
    assert!(matches!(
        err,
        infrastore_core::TimeSeriesError::InvalidParameter(_)
    ));
    assert_eq!(store.count_array_references(&hash).unwrap(), (1, 1));

    // Removing the derived DST first unblocks the STS removal.
    let dst_key = store
        .list_keys(infrastore_core::ListFilter::new())
        .unwrap()
        .into_iter()
        .find(|k| k.time_series_type() == TimeSeriesType::DeterministicSingleTimeSeries)
        .expect("the derived DST key must be listed");
    store.remove_time_series(dst_key.identity()).unwrap();
    store.remove_time_series(sts_key.identity()).unwrap();
    assert_eq!(store.count_array_references(&hash).unwrap(), (0, 0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 0);
}

#[test]
fn bulk_remove_of_dst_and_backing_sts_is_order_independent() {
    let mut store = create_store(None, true).unwrap();
    let sts_key = add_sts(&mut store, 5);
    store
        .transform_single_time_series(Duration::hours(2), Duration::hours(1), None, None)
        .unwrap();
    let hash = store.get_metadata(sts_key.identity()).unwrap().data_hash;
    let dst_key = store
        .list_keys(infrastore_core::ListFilter::new())
        .unwrap()
        .into_iter()
        .find(|k| k.time_series_type() == TimeSeriesType::DeterministicSingleTimeSeries)
        .expect("the derived DST key must be listed");

    // An STS-only bulk remove would orphan the DST and is refused atomically.
    let err = store
        .remove_time_series_bulk(&[sts_key.identity()])
        .unwrap_err();
    assert!(matches!(
        err,
        infrastore_core::TimeSeriesError::InvalidParameter(_)
    ));
    assert_eq!(store.count_array_references(&hash).unwrap(), (1, 1));

    // Removing both in one batch passes: the orphan check runs on the
    // post-removal state, so the STS-before-DST order does not matter.
    let removed = store
        .remove_time_series_bulk(&[sts_key.identity(), dst_key.identity()])
        .unwrap();
    assert_eq!(removed, 2);
    assert_eq!(store.count_array_references(&hash).unwrap(), (0, 0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 0);
}

// --- cross-type layout promotion at persist time -------------------------------

#[test]
fn shared_hash_across_packed_and_standalone_persists_as_standalone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.nc");

    // Byte-identical data referenced by a packed (STS) key and a standalone
    // (NonSeq) key -> one content-addressed array with two competing layouts.
    let values = [1.0, 2.0, 3.0, 4.0];
    let sts_data = TypedArray::from_f64(vec![4], &values);
    let ns_data = TypedArray::from_f64(vec![4], &values);

    let sts_id;
    let ns_id;
    {
        let mut store = create_store(None, true).unwrap();
        let sts_key = store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                    t0(),
                    Duration::hours(1),
                    sts_data.clone(),
                    "load",
                )),
                Features::new(),
                None,
            )
            .unwrap();
        let timestamps = vec![
            t0(),
            t0() + Duration::hours(3),
            t0() + Duration::hours(4),
            t0() + Duration::days(2),
        ];
        let ns_key = store
            .add_time_series(
                2,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::NonSequentialTimeSeries(
                    NonSequentialTimeSeries::new(timestamps, ns_data.clone(), "avail").unwrap(),
                ),
                Features::new(),
                None,
            )
            .unwrap();

        // Despite differing layouts, they share one array.
        assert_eq!(store.num_distinct_arrays().unwrap(), 1);
        sts_id = sts_key.identity().clone();
        ns_id = ns_key.identity().clone();

        // Persisting an in-memory store re-plans each array's layout. The shared
        // hash must be promoted to standalone (the packed layout is invalid for
        // the NonSeq key), and both keys must still read it back.
        store.persist_to(path.as_path()).unwrap();
    }

    let store = open_store(path.as_path(), true).unwrap();
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);

    let sts_got = store.get_time_series(&sts_id, None).unwrap();
    assert_eq!(sts_got.as_single().unwrap().data, sts_data);
    let ns_got = store.get_time_series(&ns_id, None).unwrap();
    assert_eq!(ns_got.as_non_sequential().unwrap().data, ns_data);

    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "integrity errors: {:?}", report.errors);
}

// --- rename preserves the array / hash / refcount ------------------------------

#[test]
fn rename_preserves_the_shared_array_and_refcount() {
    let mut store = create_store(None, true).unwrap();
    // Two owners share one array (identical data).
    let k1 = add_sts(&mut store, 1);
    let k2 = add_sts(&mut store, 2);
    let hash = store.get_metadata(k1.identity()).unwrap().data_hash;
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);
    assert_eq!(store.count_array_references(&hash).unwrap(), (2, 0));

    // Rename one sharer.
    let renamed = store.rename_time_series(k1.identity(), "renamed").unwrap();

    // Rename touches only the name: same hash, same shared array, same refcount.
    assert_eq!(
        store.get_metadata(renamed.identity()).unwrap().data_hash,
        hash
    );
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);
    assert_eq!(store.count_array_references(&hash).unwrap(), (2, 0));

    // Both the renamed series and the untouched sharer still read the data.
    assert_eq!(
        store
            .get_time_series(renamed.identity(), None)
            .unwrap()
            .as_single()
            .unwrap()
            .data,
        sts_series().data
    );
    assert_eq!(
        store
            .get_time_series(k2.identity(), None)
            .unwrap()
            .as_single()
            .unwrap()
            .data,
        sts_series().data
    );
}
