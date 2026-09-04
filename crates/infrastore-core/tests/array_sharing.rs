//! Content-addressed array sharing and reference-count accounting that cut
//! across time-series types:
//!
//! * `count_array_references` beyond the `(1, 1)` STS+DST case already covered
//!   in `forecasts.rs` — multi-STS `(N, 0)`, DST-without-STS `(0, 1)`, and
//!   counts that change as references are removed.
//! * The persist-time layout planner promoting a hash shared by a *packed*
//!   (`SingleTimeSeries`) and a *standalone* (`NonSequentialTimeSeries`) key to
//!   standalone, so both read back correctly (`store.rs` `plans` logic).

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

fn add_sts(store: &mut Store, owner: i64) -> infrastore_core::TimeSeriesId {
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts_series()),
            Features::new(),
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
    let hash = store.get_metadata_by_id(k1).unwrap().unwrap().data_hash;

    assert_eq!(store.count_array_references(&hash).unwrap(), (2, 0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);

    // Removing one sharer drops the count to (1, 0); the array survives.
    store.remove_by_ids(&[k1]).unwrap();
    assert_eq!(store.count_array_references(&hash).unwrap(), (1, 0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);

    // Removing the last reference clears the count and reclaims the array.
    store.remove_by_ids(&[k2]).unwrap();
    assert_eq!(store.count_array_references(&hash).unwrap(), (0, 0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 0);
}

#[test]
fn removing_the_last_sts_backing_a_dst_is_refused() {
    let mut store = create_store(None, true).unwrap();
    let sts_key = add_sts(&mut store, 5);

    // Derive a single DST view sharing the STS's array.
    let derived = store
        .transform_single_time_series(
            Duration::hours(2),
            Duration::hours(1),
            None,
            None,
            Default::default(),
        )
        .unwrap()
        .transformed;
    assert_eq!(derived, 1);

    let hash = store
        .get_metadata_by_id(sts_key)
        .unwrap()
        .unwrap()
        .data_hash;
    assert_eq!(store.count_array_references(&hash).unwrap(), (1, 1));

    // A DST is a view over the STS's array: removing its last backing
    // SingleTimeSeries would orphan it, so the remove is refused and rolled
    // back.
    let err = store.remove_by_ids(&[sts_key]).unwrap_err();
    assert!(matches!(
        err,
        infrastore_core::TimeSeriesError::InvalidParameter(_)
    ));
    assert_eq!(store.count_array_references(&hash).unwrap(), (1, 1));

    // Removing the derived DST first unblocks the STS removal.
    let dst_key = store
        .list_metadata(infrastore_core::ListFilter::new())
        .unwrap()
        .into_iter()
        .find(|m| m.time_series_type == TimeSeriesType::DeterministicSingleTimeSeries)
        .expect("the derived DST key must be listed");
    store.remove_by_ids(&[dst_key.id.unwrap()]).unwrap();
    store.remove_by_ids(&[sts_key]).unwrap();
    assert_eq!(store.count_array_references(&hash).unwrap(), (0, 0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 0);
}

#[test]
fn bulk_remove_of_dst_and_backing_sts_is_order_independent() {
    let mut store = create_store(None, true).unwrap();
    let sts_key = add_sts(&mut store, 5);
    store
        .transform_single_time_series(
            Duration::hours(2),
            Duration::hours(1),
            None,
            None,
            Default::default(),
        )
        .unwrap();
    let hash = store
        .get_metadata_by_id(sts_key)
        .unwrap()
        .unwrap()
        .data_hash;
    let dst_key = store
        .list_metadata(infrastore_core::ListFilter::new())
        .unwrap()
        .into_iter()
        .find(|m| m.time_series_type == TimeSeriesType::DeterministicSingleTimeSeries)
        .expect("the derived DST key must be listed");

    // An STS-only bulk remove would orphan the DST and is refused atomically.
    let err = store.remove_by_ids(&[sts_key]).unwrap_err();
    assert!(matches!(
        err,
        infrastore_core::TimeSeriesError::InvalidParameter(_)
    ));
    assert_eq!(store.count_array_references(&hash).unwrap(), (1, 1));

    // Removing both in one batch passes: the orphan check runs on the
    // post-removal state, so the STS-before-DST order does not matter.
    let removed = store
        .remove_by_ids(&[sts_key, dst_key.id.unwrap()])
        .unwrap();
    assert_eq!(removed, 2);
    assert_eq!(store.count_array_references(&hash).unwrap(), (0, 0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 0);
}

// --- cross-type layout promotion at persist time -------------------------------

#[test]
fn shared_hash_across_packed_and_standalone_persists_as_standalone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.h5");

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
            )
            .unwrap();

        // Despite differing layouts, they share one array.
        assert_eq!(store.num_distinct_arrays().unwrap(), 1);
        sts_id = sts_key;
        ns_id = ns_key;

        // Persisting an in-memory store re-plans each array's layout. The shared
        // hash must be promoted to standalone (the packed layout is invalid for
        // the NonSeq key), and both keys must still read it back.
        store.persist_to(path.as_path()).unwrap();
    }

    let store = open_store(path.as_path(), true).unwrap();
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);

    let sts_got = store
        .read_by_id(sts_id, infrastore_core::ReadWindow::full())
        .unwrap();
    assert_eq!(sts_got.as_single().unwrap().data, sts_data);
    let ns_got = store
        .read_by_id(ns_id, infrastore_core::ReadWindow::full())
        .unwrap();
    assert_eq!(ns_got.as_non_sequential().unwrap().data, ns_data);

    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "integrity errors: {:?}", report.errors);
}

// ---------------------------------------------------------------------------
// Physical location of a content-addressed array
// ---------------------------------------------------------------------------

/// `locate_array` tells a caller where to find an array's bytes with an outside
/// HDF5 tool. The hash alone cannot: a packed array is one *column* of a shared
/// dataset, recoverable only by scanning that dataset's companion `_h` hashes,
/// and a full packed pool spills into suffixed datasets so even the name is not
/// derivable from metadata.
#[test]
fn locate_array_names_the_dataset_and_column_of_a_packed_array() {
    use infrastore_core::ArrayLocation;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let hashes = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        // Two different arrays, so at least one lands past column 0.
        let mut hashes = Vec::new();
        for (i, owner) in [1i64, 2].iter().enumerate() {
            let vals: Vec<f64> = (0..8).map(|v| (v + i * 100) as f64).collect();
            let series = SingleTimeSeries::new(
                t0(),
                Duration::hours(1),
                TypedArray::from_f64(vec![8], &vals),
                "load",
            );
            let key = store
                .add_time_series(
                    *owner,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(series),
                    Features::new(),
                )
                .unwrap();
            hashes.push(store.get_metadata_by_id(key).unwrap().unwrap().data_hash);
        }
        store.flush().unwrap();
        hashes
    };

    let store = open_store(path.as_path(), true).unwrap();
    let mut columns = Vec::new();
    for hash in &hashes {
        match store.locate_array(hash).unwrap() {
            ArrayLocation::Packed { dataset, column } => {
                assert!(
                    dataset.starts_with("/time_series/single/sts_f64_"),
                    "an absolute path a user can paste into h5dump, got {dataset}"
                );
                columns.push(column);
            }
            other => panic!("a SingleTimeSeries is packed, got {other:?}"),
        }
    }
    columns.sort_unstable();
    assert_eq!(
        columns,
        vec![0, 1],
        "distinct arrays occupy distinct columns of the shared dataset"
    );

    // An unknown hash is NotFound, not a bogus location.
    assert!(store.locate_array(&[0u8; 32]).is_err());
}

/// An irregular series whose time axis nothing else shares gets its own
/// standalone dataset: a packed pool spreads one array over `length` chunks, so
/// a cohort of one would cost more than it saves.
#[test]
fn locate_array_names_the_standalone_dataset_of_a_lone_irregular_series() {
    use infrastore_core::ArrayLocation;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let hash = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let ns = NonSequentialTimeSeries::new(
            vec![t0(), t0() + Duration::hours(3)],
            TypedArray::from_f64(vec![2], &[1.0, 2.0]),
            "events",
        )
        .unwrap();
        let key = store
            .add_time_series(
                9,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::NonSequentialTimeSeries(ns),
                Features::new(),
            )
            .unwrap();
        let hash = store.get_metadata_by_id(key).unwrap().unwrap().data_hash;
        store.flush().unwrap();
        hash
    };

    let store = open_store(path.as_path(), true).unwrap();
    match store.locate_array(&hash).unwrap() {
        ArrayLocation::Standalone { dataset } => assert!(
            dataset.starts_with("/time_series/single/arr_"),
            "a standalone array is its own dataset, got {dataset}"
        ),
        other => panic!("a lone irregular series is standalone, got {other:?}"),
    }
}

/// Irregular series that *do* share a time axis are column-packed into one
/// timestamp-major dataset keyed by that axis — the layout a
/// [`Store::build_static_reader`] sweep over them reads one chunk per timestamp.
#[test]
fn irregular_series_sharing_a_time_axis_are_packed_into_one_cohort_dataset() {
    use infrastore_core::{AddRequest, ArrayLocation};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let stamps = vec![t0(), t0() + Duration::hours(3), t0() + Duration::hours(11)];
    // A fourth series on a *different* axis, to prove the pool is keyed by the
    // timestamps and not merely by shape.
    let other_stamps = vec![t0(), t0() + Duration::hours(4), t0() + Duration::hours(11)];

    let mut store = create_store(Some(path.as_path()), false).unwrap();
    let mut bulk = store.bulk_add();
    for owner in 1..=3 {
        let ns = NonSequentialTimeSeries::new(
            stamps.clone(),
            TypedArray::from_f64(vec![3], &[owner as f64, 10.0 + owner as f64, 99.0]),
            "outage",
        )
        .unwrap();
        bulk.push(AddRequest::new(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(ns),
        ));
    }
    let odd = NonSequentialTimeSeries::new(
        other_stamps,
        TypedArray::from_f64(vec![3], &[7.0, 8.0, 9.0]),
        "outage",
    )
    .unwrap();
    bulk.push(AddRequest::new(
        4,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::NonSequentialTimeSeries(odd),
    ));
    let keys = bulk.commit().unwrap();
    store.flush().unwrap();

    let hashes: Vec<[u8; 32]> = keys
        .iter()
        .map(|k| store.get_metadata_by_id(*k).unwrap().unwrap().data_hash)
        .collect();
    let mut cohort_datasets = Vec::new();
    for hash in &hashes[..3] {
        match store.locate_array(hash).unwrap() {
            ArrayLocation::Packed { dataset, .. } => {
                assert!(
                    dataset.starts_with("/time_series/single/nsts_f64_s_3_"),
                    "a cohort dataset is keyed by its timestamp vector, got {dataset}"
                );
                cohort_datasets.push(dataset);
            }
            other => panic!("a shared time axis packs, got {other:?}"),
        }
    }
    assert_eq!(
        cohort_datasets[0], cohort_datasets[2],
        "one time axis, one dataset"
    );
    // The odd one out is alone on its axis, so it stays standalone.
    assert!(matches!(
        store.locate_array(&hashes[3]).unwrap(),
        ArrayLocation::Standalone { .. }
    ));

    // Every value survives the packing, across a reopen that rebuilds the pool
    // index from the dataset names.
    drop(store);
    let store = open_store(path.as_path(), true).unwrap();
    for (owner, key) in keys.iter().enumerate().take(3) {
        match store
            .read_by_id(*key, infrastore_core::ReadWindow::full())
            .unwrap()
        {
            TimeSeriesData::NonSequentialTimeSeries(ns) => {
                assert_eq!(ns.timestamps, stamps);
                assert_eq!(
                    ns.data.to_f64_vec().unwrap(),
                    vec![owner as f64 + 1.0, 11.0 + owner as f64, 99.0]
                );
            }
            other => panic!("expected a NonSequentialTimeSeries, got {other:?}"),
        }
    }
    assert!(store.verify_integrity().unwrap().ok());
}

#[test]
fn locate_array_reports_no_on_disk_location_for_an_in_memory_store() {
    use infrastore_core::ArrayLocation;

    let mut store = create_store(None, true).unwrap();
    let key = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts_series()),
            Features::new(),
        )
        .unwrap();
    let hash = store.get_metadata_by_id(key).unwrap().unwrap().data_hash;
    assert_eq!(store.locate_array(&hash).unwrap(), ArrayLocation::InMemory);
    assert!(store.locate_array(&[0u8; 32]).is_err());
}
