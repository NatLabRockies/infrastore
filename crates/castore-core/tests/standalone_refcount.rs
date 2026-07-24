//! Reference-counting coverage for **standalone** (`arr_{hash}`) NetCDF arrays —
//! the storage kind used by `NonSequentialTimeSeries` (plain standalone) and the
//! dense forecasts `Deterministic` / `Probabilistic` / `Scenarios` (windowed
//! standalone). The packed (`sts_*`) path is exercised in `round_trip.rs`,
//! `api_additions.rs`, and `netcdf_roundtrip.rs`; standalone arrays reclaim
//! differently (NetCDF cannot delete a variable, so the last-reference case
//! leaves an unreachable variable rather than a reusable slot) and were
//! previously only checked at the reader-slot level, never through a delete.
//!
//! `count_array_references` deliberately tallies only `SingleTimeSeries` /
//! `DeterministicSingleTimeSeries`, so the observable here is
//! `num_distinct_arrays()` (distinct catalog `data_hash`es) — which drops to the
//! correct value when the last reference is removed regardless of whether the
//! physical variable lingers on disk.

use castore_core::{
    Deterministic, Features, KeyIdentity, ListFilter, NonSequentialTimeSeries, OwnerCategory,
    Probabilistic, Scenarios, Store, TimeSeriesData, TimeSeriesKey, TypedArray, create_store,
    open_store,
};
use chrono::{DateTime, Duration, TimeZone, Utc};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

// --- Builders: one per standalone-backed type. `base` shifts every value so two
// different `base`s produce two distinct content hashes, and equal `base`s
// produce byte-identical (thus shared) arrays. ---------------------------------

fn nonseq(base: f64) -> TimeSeriesData {
    // Irregular timestamps -> plain `Standalone` layout.
    let timestamps = vec![
        t0(),
        t0() + Duration::hours(3),
        t0() + Duration::hours(4),
        t0() + Duration::days(2),
    ];
    let data = TypedArray::from_f64(vec![4], &[base, base + 1.0, base + 2.0, base + 3.0]);
    TimeSeriesData::NonSequentialTimeSeries(
        NonSequentialTimeSeries::new(timestamps, data, "availability").unwrap(),
    )
}

fn deterministic(base: f64) -> TimeSeriesData {
    // H=2, C=3 -> shape [2, 3], `StandaloneWindowed { count_axis: 1 }`.
    let vals: Vec<f64> = (0..6).map(|i| base + i as f64).collect();
    TimeSeriesData::Deterministic(
        Deterministic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            TypedArray::from_f64(vec![2, 3], &vals),
            "det_load",
        )
        .unwrap(),
    )
}

fn probabilistic(base: f64) -> TimeSeriesData {
    // P=3, H=2, C=2 -> shape [3, 2, 2], `StandaloneWindowed { count_axis: 2 }`.
    let vals: Vec<f64> = (0..12).map(|i| base + i as f64).collect();
    TimeSeriesData::Probabilistic(
        Probabilistic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(4),
            2,
            vec![0.1, 0.5, 0.9],
            TypedArray::from_f64(vec![3, 2, 2], &vals),
            "prob_load",
        )
        .unwrap(),
    )
}

fn scenarios(base: f64) -> TimeSeriesData {
    // S=4, H=3, C=2 -> shape [4, 3, 2], `StandaloneWindowed { count_axis: 2 }`.
    let vals: Vec<f64> = (0..24).map(|i| base + i as f64).collect();
    TimeSeriesData::Scenarios(
        Scenarios::new(
            t0(),
            Duration::hours(1),
            Duration::hours(3),
            Duration::hours(6),
            2,
            4,
            TypedArray::from_f64(vec![4, 3, 2], &vals),
            "scenarios_load",
        )
        .unwrap(),
    )
}

fn add(store: &mut Store, owner: i64, data: TimeSeriesData) -> TimeSeriesKey {
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            data,
            Features::new(),
            None,
        )
        .unwrap()
}

/// The core shared-then-decrement cycle for one standalone-backed builder:
/// two owners with identical data share one array; removing the first leaves the
/// second reading correctly with the array still present; removing the second
/// drops the last reference and integrity still holds.
fn shares_and_reclaims(build: impl Fn(f64) -> TimeSeriesData) {
    let mut store = create_store(None, true).unwrap();

    let k1 = add(&mut store, 1, build(7.0));
    let k2 = add(&mut store, 2, build(7.0));

    // Byte-identical data across two owners -> one content-addressed array.
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 2);

    // Remove the first reference: the array survives because k2 still holds it,
    // and the survivor still reads back its data.
    store.remove_time_series(k1.identity()).unwrap();
    assert_eq!(
        store.num_distinct_arrays().unwrap(),
        1,
        "array must survive while a second reference exists"
    );
    store.get_time_series(k2.identity(), None).unwrap();

    // Remove the last reference: the array is now unreferenced and dropped.
    store.remove_time_series(k2.identity()).unwrap();
    assert_eq!(store.num_distinct_arrays().unwrap(), 0);
    assert!(store.list_keys(ListFilter::new()).unwrap().is_empty());
    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "integrity errors: {:?}", report.errors);
}

#[test]
fn nonseq_shares_and_reclaims_standalone_array() {
    shares_and_reclaims(nonseq);
}

#[test]
fn deterministic_shares_and_reclaims_standalone_array() {
    shares_and_reclaims(deterministic);
}

#[test]
fn probabilistic_shares_and_reclaims_standalone_array() {
    shares_and_reclaims(probabilistic);
}

#[test]
fn scenarios_shares_and_reclaims_standalone_array() {
    shares_and_reclaims(scenarios);
}

// --- Deletion-path coverage for standalone arrays: `remove_by_filter` and
// `remove_time_series_bulk` were only proven to reclaim packed arrays. --------

#[test]
fn remove_by_filter_reclaims_standalone_array() {
    let mut store = create_store(None, true).unwrap();
    // Three distinct standalone arrays (distinct `base`s -> distinct hashes).
    for owner in 1..=3 {
        add(&mut store, owner, nonseq(owner as f64 * 10.0));
    }
    assert_eq!(store.num_distinct_arrays().unwrap(), 3);

    let removed = store
        .remove_by_filter(ListFilter::new().owner_id(2))
        .unwrap();
    assert_eq!(removed, 1);
    // Owner-2's array had no other reference, so it is dropped.
    assert_eq!(store.num_distinct_arrays().unwrap(), 2);
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 2);
}

#[test]
fn remove_bulk_reclaims_shared_standalone_only_when_last_reference_gone() {
    let mut store = create_store(None, true).unwrap();
    // Two owners share one standalone array; a third owner holds a distinct one.
    let k1 = add(&mut store, 1, nonseq(5.0));
    let k2 = add(&mut store, 2, nonseq(5.0));
    let k3 = add(&mut store, 3, nonseq(99.0));
    assert_eq!(store.num_distinct_arrays().unwrap(), 2);

    // Removing only one of the two sharers keeps the shared array alive.
    store.remove_time_series_bulk(&[k1.identity()]).unwrap();
    assert_eq!(
        store.num_distinct_arrays().unwrap(),
        2,
        "shared array must survive while k2 references it"
    );

    // Removing the last sharer and the distinct array reclaims both.
    let removed = store
        .remove_time_series_bulk(&[k2.identity(), k3.identity()])
        .unwrap();
    assert_eq!(removed, 2);
    assert_eq!(store.num_distinct_arrays().unwrap(), 0);
}

// --- dtype sweep on the standalone refcount path. Every existing dedup/refcount
// test uses f64; content addressing hashes raw bytes, so exercise a signed int
// and a bool array end to end through share -> decrement -> reclaim. -----------

fn nonseq_typed(data: TypedArray) -> TimeSeriesData {
    let timestamps = vec![t0(), t0() + Duration::hours(1), t0() + Duration::hours(5)];
    TimeSeriesData::NonSequentialTimeSeries(
        NonSequentialTimeSeries::new(timestamps, data, "flags").unwrap(),
    )
}

fn standalone_dtype_cycle(data: TypedArray) {
    let mut store = create_store(None, true).unwrap();
    let k1 = add(&mut store, 1, nonseq_typed(data.clone()));
    let k2 = add(&mut store, 2, nonseq_typed(data.clone()));
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);

    // Dtype and values survive the round trip through the shared array.
    let got = store.get_time_series(k1.identity(), None).unwrap();
    assert_eq!(got.as_non_sequential().unwrap().data, data);

    store.remove_time_series(k1.identity()).unwrap();
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);
    store.remove_time_series(k2.identity()).unwrap();
    assert_eq!(store.num_distinct_arrays().unwrap(), 0);
    assert!(store.verify_integrity().unwrap().ok());
}

#[test]
fn standalone_refcount_i64() {
    let data = TypedArray::from_slice(vec![3], &[10i64, -20, 30]).unwrap();
    standalone_dtype_cycle(data);
}

#[test]
fn standalone_refcount_bool() {
    let data = TypedArray::from_slice(vec![3], &[true, false, true]).unwrap();
    standalone_dtype_cycle(data);
}

// --- On-disk persistence: a standalone array orphaned by a delete must stay
// gone across flush + reopen, survivors must still read, and a fresh add after
// the delete must round-trip. NetCDF cannot physically remove the variable, so
// this guards the catalog side (the dropped hash never reappears) and that a
// reopened store agrees on the distinct-array count. --------------------------

#[test]
fn standalone_orphan_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("standalone.nc");

    // Three distinct standalone forecasts on disk.
    let k2_identity;
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let _k1 = add(&mut store, 1, deterministic(1.0));
        let k2 = add(&mut store, 2, deterministic(100.0));
        let _k3 = add(&mut store, 3, deterministic(200.0));
        k2_identity = k2.identity().clone();
        assert_eq!(store.num_distinct_arrays().unwrap(), 3);
        store.flush().unwrap();
    }

    // Reopen writable and drop owner 1's array (no other reference).
    {
        let mut store = open_store(path.as_path(), false).unwrap();
        assert_eq!(store.num_distinct_arrays().unwrap(), 3);
        let k1 = KeyIdentity {
            owner_id: 1,
            ..k2_identity.clone()
        };
        store.remove_time_series(&k1).unwrap();
        assert_eq!(store.num_distinct_arrays().unwrap(), 2);
        store.flush().unwrap();
    }

    // Reopen again: the orphaned hash must not resurrect, survivors read, and a
    // fresh distinct add after the orphaning round-trips.
    {
        let mut store = open_store(path.as_path(), false).unwrap();
        assert_eq!(
            store.num_distinct_arrays().unwrap(),
            2,
            "orphaned standalone array must not reappear after reopen"
        );
        // Surviving owner-2 forecast still reads its original values.
        let got = store.get_time_series(&k2_identity, None).unwrap();
        assert!(got.as_deterministic().is_some());

        let _k4 = add(&mut store, 4, deterministic(500.0));
        assert_eq!(store.num_distinct_arrays().unwrap(), 3);
        store.flush().unwrap();
        let report = store.verify_integrity().unwrap();
        assert!(report.ok(), "integrity errors: {:?}", report.errors);
    }
}
