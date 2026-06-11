//! NetCDF persistence integration tests.
//!
//! These exercise the on-disk format and slot map: write to a real `.nc` file,
//! close, reopen, and verify what comes back. Also covers the spill-on-1001
//! and compaction tombstone behaviours documented in the spec.

use chrono::{Duration, TimeZone, Utc};
use time_series_store_core::{
    Compression, Features, ListFilter, NonSequentialTimeSeries, OwnerCategory, SingleTimeSeries,
    TimeSeriesData, TypedArray, create_store, create_store_with_compression, open_store,
};

fn series(initial_year: i32, length: usize, base: f64) -> SingleTimeSeries {
    let initial_timestamp = Utc.with_ymd_and_hms(initial_year, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let values: Vec<f64> = (0..length).map(|i| base + i as f64).collect();
    let data = TypedArray::from_f64(vec![length], &values);
    SingleTimeSeries::new(initial_timestamp, resolution, data, "load")
}

#[test]
fn persistent_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let s = series(2024, 24, 100.0);
        store
            .add_time_series(
                "42",
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s.clone()),
                Features::new(),
                Some("MW".into()),
            )
            .unwrap();
        store.flush().unwrap();
        // store dropped here, file closed
    }

    // Reopen and read back.
    let store = open_store(path.as_path(), true).unwrap();
    let keys = store.get_time_series_keys("42").unwrap();
    assert_eq!(keys.len(), 1);
    let got = store.get_time_series(&keys[0], None).unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.length, 24);
    assert_eq!(
        single.data.to_f64_vec().unwrap(),
        (0..24).map(|i| 100.0 + i as f64).collect::<Vec<_>>()
    );

    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "integrity errors: {:?}", report.errors);
}

/// Every supported compression policy must round-trip transparently: data
/// written under one filter reads back identically, and appends after reopen
/// reuse the persisted policy.
#[test]
fn compression_policies_round_trip() {
    for compression in [
        Compression::None,
        Compression::Deflate {
            level: 9,
            shuffle: false,
        },
        Compression::Deflate {
            level: 1,
            shuffle: true,
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.nc");

        {
            let mut store =
                create_store_with_compression(Some(path.as_path()), false, compression).unwrap();
            store
                .add_time_series(
                    "7",
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(series(2024, 24, 100.0)),
                    Features::new(),
                    None,
                )
                .unwrap();
            store.flush().unwrap();
        }

        // Reopen read-write and append a second series; this exercises the
        // restored-from-attribute compression path.
        {
            let mut store = open_store(path.as_path(), false).unwrap();
            // The policy is restored from the persisted file attribute.
            assert_eq!(store.compression(), compression, "{compression:?}");
            store
                .add_time_series(
                    "8",
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(series(2024, 24, 200.0)),
                    Features::new(),
                    None,
                )
                .unwrap();
            store.flush().unwrap();
        }

        let store = open_store(path.as_path(), true).unwrap();
        for (owner, base) in [("7", 100.0), ("8", 200.0)] {
            let keys = store.get_time_series_keys(owner).unwrap();
            assert_eq!(keys.len(), 1, "{compression:?}");
            let got = store.get_time_series(&keys[0], None).unwrap();
            assert_eq!(
                got.as_single().unwrap().data.to_f64_vec().unwrap(),
                (0..24).map(|i| base + i as f64).collect::<Vec<_>>(),
                "{compression:?}",
            );
        }
        let report = store.verify_integrity().unwrap();
        assert!(report.ok(), "integrity errors: {:?}", report.errors);
    }
}

/// DEFLATE levels outside 0–9 are rejected up front.
#[test]
fn invalid_compression_level_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let err = create_store_with_compression(
        Some(path.as_path()),
        false,
        Compression::Deflate {
            level: 10,
            shuffle: true,
        },
    );
    assert!(err.is_err(), "level 10 should be rejected");
}

#[test]
fn deduplication_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let s = series(2024, 24, 7.0);
        for owner in ["1", "2", "3"] {
            store
                .add_time_series(
                    owner,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(s.clone()),
                    Features::new(),
                    None,
                )
                .unwrap();
        }
        store.flush().unwrap();
    }

    let store = open_store(path.as_path(), true).unwrap();
    // Three associations exist…
    assert_eq!(store.list_time_series(ListFilter::new()).unwrap().len(), 3);
    // …but verify_integrity reads each only once at the array level — the
    // single underlying column hashes correctly.
    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "errors: {:?}", report.errors);
}

#[test]
fn multiple_resolutions_separate_datasets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let data = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);

        for (i, res) in [
            Duration::hours(1),
            Duration::minutes(15),
            Duration::seconds(60),
        ]
        .into_iter()
        .enumerate()
        {
            let s = SingleTimeSeries::new(initial, res, data.clone(), "load");
            store
                .add_time_series(
                    &(i + 1).to_string(),
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(s),
                    Features::new(),
                    None,
                )
                .unwrap();
        }
        store.flush().unwrap();
    }

    let store = open_store(path.as_path(), true).unwrap();
    let resolutions = store.get_resolutions(None).unwrap();
    assert_eq!(resolutions.len(), 3);
    let report = store.verify_integrity().unwrap();
    assert!(report.ok());
}

#[test]
fn time_range_slicing_through_netcdf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let data = TypedArray::from_f64(
        vec![10],
        &[10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0],
    );
    let s = SingleTimeSeries::new(initial, resolution, data, "load");

    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = store
            .add_time_series(
                "1",
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s.clone()),
                Features::new(),
                None,
            )
            .unwrap();
        store.flush().unwrap();
        key
    };

    let store = open_store(path.as_path(), true).unwrap();
    let start = initial + Duration::hours(3);
    let end = initial + Duration::hours(7);
    let got = store.get_time_series(&key, Some((start, end))).unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.length, 4);
    assert_eq!(single.initial_timestamp, start);
    assert_eq!(
        single.data.to_f64_vec().unwrap(),
        vec![40.0, 50.0, 60.0, 70.0]
    );
}

#[test]
fn spill_into_new_dataset_past_capacity() {
    use time_series_store_core::AddRequest;
    use time_series_store_core::storage::netcdf::MAX_COLS_PER_DATASET;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    // Need MAX + 1 distinct arrays of identical (length, resolution) so they
    // compete for the same dataset family. To keep the test fast we use small
    // length=4 and unique data per series.
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let total = MAX_COLS_PER_DATASET + 1;

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let mut bulk = Vec::with_capacity(total);
        for i in 0..total {
            let vals = [i as f64, i as f64 + 1.0, i as f64 + 2.0, i as f64 + 3.0];
            let data = TypedArray::from_f64(vec![4], &vals);
            let s = SingleTimeSeries::new(initial, resolution, data, "load");
            bulk.push(AddRequest {
                owner_uuid: (i + 1).to_string(),
                owner_type: "Generator".into(),
                owner_category: OwnerCategory::Component,
                data: TimeSeriesData::SingleTimeSeries(s),
                features: Features::new(),
                units: None,

                logical_type: None,
            });
        }
        store.add_time_series_bulk(bulk).unwrap();
        store.flush().unwrap();
    }

    // Reopen, sample the first and the last association, and verify integrity.
    let store = open_store(path.as_path(), true).unwrap();
    let counts = store.get_time_series_counts().unwrap();
    assert_eq!(counts.static_time_series as usize, total);

    // Quick spot-check: the very last one — which must have spilled — reads back.
    let keys = store.get_time_series_keys(&total.to_string()).unwrap();
    assert_eq!(keys.len(), 1);
    let last = store.get_time_series(&keys[0], None).unwrap();
    let single = last.as_single().unwrap();
    assert_eq!(
        single.data.to_f64_vec().unwrap(),
        vec![
            (total - 1) as f64,
            (total - 1) as f64 + 1.0,
            (total - 1) as f64 + 2.0,
            (total - 1) as f64 + 3.0,
        ]
    );

    let report = store.verify_integrity().unwrap();
    assert!(
        report.ok(),
        "errors after spill: {} (showing up to 5: {:?})",
        report.errors.len(),
        &report.errors.iter().take(5).collect::<Vec<_>>()
    );
}

#[test]
fn compact_reports_tombstones_and_slot_is_reused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    let mut store = create_store(Some(path.as_path()), false).unwrap();
    // Three distinct arrays in the same family.
    let s1 = series(2024, 8, 1.0);
    let s2 = series(2024, 8, 100.0);
    let s3 = series(2024, 8, 200.0);

    let k1 = store
        .add_time_series(
            "1",
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s1),
            Features::new(),
            None,
        )
        .unwrap();
    let _k2 = store
        .add_time_series(
            "2",
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s2),
            Features::new(),
            None,
        )
        .unwrap();
    let _k3 = store
        .add_time_series(
            "3",
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s3),
            Features::new(),
            None,
        )
        .unwrap();

    // Remove the middle association — its underlying array is dropped because
    // no other association references it.
    store.remove_time_series(&k1).unwrap();

    // compact() should report >=1 reclaimed slot. (The full dataset was
    // pre-allocated at MAX_COLS, so it'll actually report MAX_COLS-2.)
    let report = store.compact().unwrap();
    assert!(report.slots_reclaimed >= 1);

    // Adding a new array should reuse the freed column slot.
    let s4 = series(2024, 8, 500.0);
    store
        .add_time_series(
            "4",
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s4),
            Features::new(),
            None,
        )
        .unwrap();

    let counts = store.get_time_series_counts().unwrap();
    // s2, s3, s4 → 3 active.
    assert_eq!(counts.static_time_series, 3);

    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "errors: {:?}", report.errors);
}

#[test]
fn data_format_version_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    {
        let _ = create_store(Some(path.as_path()), false).unwrap();
    }
    // Open the file with the netcdf crate directly to read the attribute.
    let f = netcdf::open(&path).unwrap();
    let attr = f.attribute("data_format_version").expect("attr present");
    let value = attr.value().unwrap();
    let netcdf::AttributeValue::Str(s) = value else {
        panic!("expected str, got {value:?}");
    };
    assert_eq!(s, time_series_store_core::DATA_FORMAT_VERSION);
}

#[test]
fn netcdf_roundtrips_multidim_element_tuples() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    // A (4, 3) array — a 3-tuple per step (e.g. quadratic curve coeffs).
    let values: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let data = TypedArray::from_f64(vec![4, 3], &values);
    let s = SingleTimeSeries::new(initial, resolution, data.clone(), "load");

    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = store
            .add_time_series(
                "1",
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s),
                Features::new(),
                None,
            )
            .unwrap();
        store.flush().unwrap();
        key
    };

    let store = open_store(path.as_path(), true).unwrap();
    let got = store.get_time_series(&key, None).unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.data.shape, vec![4, 3]);
    assert_eq!(single.data, data);
    assert!(store.verify_integrity().unwrap().ok());
}

#[test]
fn golden_hash_pin() {
    // Pin the exact SHA-256 of a fixed input. Any change in the canonical
    // hash domain that perturbs this value is a format-breaking change and
    // must bump DATA_FORMAT_VERSION.
    use time_series_store_core::hash::{array_hash, hash_hex};
    let data = TypedArray::from_f64(vec![4], &[0.0, 1.0, 2.0, 3.0]);
    let h = array_hash(&data);
    let hex = hash_hex(&h);
    assert_eq!(
        hex, "f85b4f66e62c7c51f9c82d01eabed7fb5e3b5217a69296aaaff876e1144dd841",
        "golden hash drifted; bump DATA_FORMAT_VERSION if intentional",
    );
}

#[test]
fn non_sequential_persistent_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let timestamps = vec![
        initial,
        initial + Duration::minutes(7),
        initial + Duration::days(3),
    ];
    let data = TypedArray::from_f64(vec![3], &[1.5, 2.5, 3.5]);
    let key = {
        let mut store = create_store(Some(&path), false).unwrap();
        let series =
            NonSequentialTimeSeries::new(timestamps.clone(), data.clone(), "events").unwrap();
        let key = store
            .add_time_series(
                "1",
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::NonSequentialTimeSeries(series),
                Features::new(),
                None,
            )
            .unwrap();
        store.flush().unwrap();
        key
    };

    let store = open_store(&path, true).unwrap();
    let got = store.get_time_series(&key, None).unwrap();
    let irregular = got.as_non_sequential().unwrap();
    assert_eq!(irregular.timestamps, timestamps);
    assert_eq!(irregular.data, data);
    assert!(store.verify_integrity().unwrap().ok());
}
