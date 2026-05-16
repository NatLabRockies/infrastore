//! End-to-end round-trip tests for the in-memory Store. These exercise the
//! full Store API surface defined in M0.

use std::collections::BTreeMap;

use chrono::{Duration, TimeZone, Utc};
use ndarray::{array, ArrayD};
use time_series_store_core::{
    create_store, FeatureValue, Features, ListFilter, OwnerCategory, SingleTimeSeries,
    TimeSeriesData, TimeSeriesError, TimeSeriesType,
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

fn features_with_year(year: i64) -> Features {
    let mut f: Features = BTreeMap::new();
    f.insert("model_year".into(), FeatureValue::Int(year));
    f
}

#[test]
fn add_and_get_round_trip() {
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 24, 100.0);

    let key = store
        .add_time_series(
            42,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            Some("MW".into()),
            None,
        )
        .unwrap();

    let got = store.get_time_series(&key, None).unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.data, s.data);
    assert_eq!(single.length, 24);
    assert_eq!(single.initial_timestamp, s.initial_timestamp);
    assert_eq!(single.resolution, s.resolution);
}

#[test]
fn duplicate_key_rejected() {
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 12, 1.0);

    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
            None,
        )
        .unwrap();

    let err = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
            None,
        )
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateTimeSeries));
}

#[test]
fn features_disambiguate_keys() {
    let mut store = create_store(None, true).unwrap();
    let s1 = series(2024, 12, 1.0);
    let s2 = series(2024, 12, 100.0);

    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s1.clone()),
            features_with_year(2030),
            None,
            None,
        )
        .unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s2.clone()),
            features_with_year(2035),
            None,
            None,
        )
        .unwrap();

    let all = store.list_time_series(ListFilter::new().owner_id(1)).unwrap();
    assert_eq!(all.len(), 2);

    // Subset filter — only the 2035 row.
    let only_2035 = store
        .list_time_series(ListFilter::new().owner_id(1).features(features_with_year(2035)))
        .unwrap();
    assert_eq!(only_2035.len(), 1);
    assert_eq!(only_2035[0].features, features_with_year(2035));
}

#[test]
fn deduplication_via_content_addressing() {
    // Two associations with identical data should share one underlying array.
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 24, 7.0);

    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
            None,
        )
        .unwrap();
    store
        .add_time_series(
            2,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
            None,
        )
        .unwrap();

    let counts = store.get_time_series_counts().unwrap();
    assert_eq!(counts.static_time_series, 2);
    assert_eq!(counts.components_with_time_series, 2);

    let report = store.verify_integrity().unwrap();
    assert!(report.ok(), "integrity errors: {:?}", report.errors);
}

#[test]
fn remove_keeps_array_when_other_refs_exist() {
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 12, 1.0);

    let k1 = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
            None,
        )
        .unwrap();
    let k2 = store
        .add_time_series(
            2,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
            None,
        )
        .unwrap();

    // Remove first association — array still referenced by k2, so k2 must work.
    store.remove_time_series(&k1).unwrap();
    let still_there = store.get_time_series(&k2, None).unwrap();
    assert_eq!(still_there.as_single().unwrap().data, s.data);

    // Remove second — array is now unreferenced. The store doesn't expose
    // the dropped-array fact directly, but verify_integrity should still pass.
    store.remove_time_series(&k2).unwrap();
    let report = store.verify_integrity().unwrap();
    assert!(report.ok());
}

#[test]
fn bulk_add_atomic_rollback() {
    use time_series_store_core::AddRequest;

    let mut store = create_store(None, true).unwrap();
    let s_ok = series(2024, 12, 1.0);
    let s_dup = series(2024, 12, 100.0);

    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s_ok.clone()),
            Features::new(),
            None,
            None,
        )
        .unwrap();

    // Bulk: first item succeeds, second collides with the existing (1,"load",None,{}).
    let bulk = vec![
        AddRequest {
            owner_id: 2,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            name: "load".into(),
            data: TimeSeriesData::SingleTimeSeries(s_ok.clone()),
            features: Features::new(),
            units: None,
            scaling_factor_multiplier: None,
        },
        AddRequest {
            owner_id: 1,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            name: "load".into(),
            data: TimeSeriesData::SingleTimeSeries(s_dup.clone()),
            features: Features::new(),
            units: None,
            scaling_factor_multiplier: None,
        },
    ];
    let err = store.add_time_series_bulk(bulk).unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateTimeSeries));

    // The first item must have been rolled back: owner 2 has nothing.
    let rows = store
        .list_time_series(ListFilter::new().owner_id(2))
        .unwrap();
    assert!(rows.is_empty(), "rollback failed: {:?}", rows);
}

#[test]
fn time_range_slicing() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let data = array![10.0, 20.0, 30.0, 40.0, 50.0, 60.0].into_dyn();
    let s = SingleTimeSeries::new(initial, resolution, data);

    let key = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s),
            Features::new(),
            None,
            None,
        )
        .unwrap();

    // Hours 2..5 (i.e. samples 30, 40, 50).
    let start = initial + Duration::hours(2);
    let end = initial + Duration::hours(5);
    let got = store.get_time_series(&key, Some((start, end))).unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.length, 3);
    assert_eq!(single.initial_timestamp, start);
    assert_eq!(
        single.data.iter().copied().collect::<Vec<_>>(),
        vec![30.0, 40.0, 50.0]
    );
}

#[test]
fn clear_by_owner() {
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 12, 1.0);

    for owner in [1, 2, 3] {
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
    assert_eq!(
        store.get_time_series_counts().unwrap().static_time_series,
        3
    );
    let removed = store.clear_time_series(Some(2)).unwrap();
    assert_eq!(removed, 1);
    let remaining = store
        .list_time_series(ListFilter::new().time_series_type(TimeSeriesType::SingleTimeSeries))
        .unwrap();
    assert_eq!(remaining.len(), 2);
}

#[test]
fn read_only_blocks_writes() {
    use std::path::Path;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");

    // Create a writable store at `path` (sidecar sqlite) and add a row.
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let s = series(2024, 12, 1.0);
        store
            .add_time_series(
                1,
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

    // Reopen read-only.
    let mut ro = time_series_store_core::open_store(path.as_path() as &Path, true).unwrap();
    let s = series(2024, 12, 1.0);
    let err = ro
        .add_time_series(
            2,
            "Generator",
            OwnerCategory::Component,
            "load",
            TimeSeriesData::SingleTimeSeries(s),
            Features::new(),
            None,
            None,
        )
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::ReadOnlyStore));
}

#[test]
fn distinct_resolutions_returned_sorted() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let data = array![1.0_f64, 2.0, 3.0].into_dyn();

    for (i, resolution) in [Duration::hours(1), Duration::minutes(15), Duration::hours(4)]
        .into_iter()
        .enumerate()
    {
        let s = SingleTimeSeries::new(initial, resolution, data.clone());
        store
            .add_time_series(
                i as i64 + 1,
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

    let resolutions = store.get_resolutions(None).unwrap();
    assert_eq!(
        resolutions,
        vec![
            Duration::minutes(15),
            Duration::hours(1),
            Duration::hours(4)
        ]
    );
}
