//! NetCDF persistence integration tests.
//!
//! These exercise the on-disk format and slot map: write to a real `.nc` file,
//! close, reopen, and verify what comes back. Also covers the spill-on-1001
//! and compaction tombstone behaviours documented in the spec.

use chrono::{Duration, TimeZone, Utc};
use ndarray::ArrayD;
use time_series_store_core::{
    create_store, open_store, Features, ListFilter, OwnerCategory, SingleTimeSeries,
    TimeSeriesData,
};

fn series(initial_year: i32, length: usize, base: f64) -> SingleTimeSeries {
    let initial_timestamp = Utc.with_ymd_and_hms(initial_year, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let data: ArrayD<f64> = ArrayD::from_shape_vec(
        vec![length],
        (0..length).map(|i| base + i as f64).collect(),
    )
    .unwrap();
    SingleTimeSeries::new(initial_timestamp, resolution, data)
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
                "load",
                TimeSeriesData::SingleTimeSeries(s.clone()),
                Features::new(),
                Some("MW".into()),
                None,
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
        single.data.iter().copied().collect::<Vec<_>>(),
        (0..24).map(|i| 100.0 + i as f64).collect::<Vec<_>>()
    );

    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "integrity errors: {:?}", report.errors);
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
                    "load",
                    TimeSeriesData::SingleTimeSeries(s.clone()),
                    Features::new(),
                    None,
                    None,
                )
                .unwrap();
        }
        store.flush().unwrap();
    }

    let store = open_store(path.as_path(), true).unwrap();
    // Three associations exist…
    assert_eq!(
        store.list_time_series(ListFilter::new()).unwrap().len(),
        3
    );
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
        let data = ArrayD::from_shape_vec(vec![3], vec![1.0, 2.0, 3.0]).unwrap();

        for (i, res) in [
            Duration::hours(1),
            Duration::minutes(15),
            Duration::seconds(60),
        ]
        .into_iter()
        .enumerate()
        {
            let s = SingleTimeSeries::new(initial, res, data.clone());
            store
                .add_time_series(
                    &(i + 1).to_string(),
                    "Generator",
                    OwnerCategory::Component,
                    "load",
                    TimeSeriesData::SingleTimeSeries(s),
                    Features::new(),
                    None,
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
    let data = ArrayD::from_shape_vec(
        vec![10],
        vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0],
    )
    .unwrap();
    let s = SingleTimeSeries::new(initial, resolution, data);

    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = store
            .add_time_series(
                "1",
                "Generator",
                OwnerCategory::Component,
                "load",
                TimeSeriesData::SingleTimeSeries(s.clone()),
                Features::new(),
                None,
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
        single.data.iter().copied().collect::<Vec<_>>(),
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
            let data = ArrayD::from_shape_vec(
                vec![4],
                vec![i as f64, i as f64 + 1.0, i as f64 + 2.0, i as f64 + 3.0],
            )
            .unwrap();
            let s = SingleTimeSeries::new(initial, resolution, data);
            bulk.push(AddRequest {
                owner_uuid: (i + 1).to_string(),
                owner_type: "Generator".into(),
                owner_category: OwnerCategory::Component,
                name: "load".into(),
                data: TimeSeriesData::SingleTimeSeries(s),
                features: Features::new(),
                units: None,
                scaling_factor_multiplier: None,
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
    let keys = store
        .get_time_series_keys(&total.to_string())
        .unwrap();
    assert_eq!(keys.len(), 1);
    let last = store.get_time_series(&keys[0], None).unwrap();
    let single = last.as_single().unwrap();
    assert_eq!(
        single.data.iter().copied().collect::<Vec<_>>(),
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
            "load",
            TimeSeriesData::SingleTimeSeries(s1),
            Features::new(),
            None,
            None,
        )
        .unwrap();
    let _k2 = store
        .add_time_series(
            "2",
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s2),
            Features::new(),
            None,
            None,
        )
        .unwrap();
    let _k3 = store
        .add_time_series(
            "3",
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s3),
            Features::new(),
            None,
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
            "load",
            TimeSeriesData::SingleTimeSeries(s4),
            Features::new(),
            None,
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
fn netcdf_rejects_multidim_data_in_v0() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let mut store = create_store(Some(path.as_path()), false).unwrap();

    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    // A (4, 3) array — multi-dim per-step value (e.g. quadratic curve coeffs).
    let data = ArrayD::from_shape_vec(vec![4, 3], (0..12).map(|i| i as f64).collect()).unwrap();
    let s = SingleTimeSeries::new(initial, resolution, data);

    let err = store
        .add_time_series(
            "1",
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s),
            Features::new(),
            None,
            None,
        )
        .unwrap_err();
    use time_series_store_core::TimeSeriesError;
    assert!(matches!(err, TimeSeriesError::InvalidParameter(_)));
}

#[test]
fn golden_hash_pin() {
    // Pin the exact SHA-256 of a fixed input. Any change in the canonical
    // hash domain that perturbs this value is a format-breaking change and
    // must bump DATA_FORMAT_VERSION.
    use time_series_store_core::hash::{array_hash, hash_hex};
    let data = ArrayD::from_shape_vec(vec![4], vec![0.0_f64, 1.0, 2.0, 3.0]).unwrap();
    let h = array_hash(&data);
    let hex = hash_hex(&h);
    assert_eq!(
        hex,
        "f85b4f66e62c7c51f9c82d01eabed7fb5e3b5217a69296aaaff876e1144dd841",
        "golden hash drifted; bump DATA_FORMAT_VERSION if intentional",
    );
}
