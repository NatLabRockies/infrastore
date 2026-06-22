//! End-to-end round-trip tests for the in-memory Store. These exercise the
//! full Store API surface defined in M0.

use std::collections::BTreeMap;

use chrono::{Duration, TimeZone, Utc};
use time_series_store_core::{
    FeatureValue, Features, ListFilter, NonSequentialTimeSeries, OwnerCategory, SingleTimeSeries,
    TimeSeriesData, TimeSeriesError, TimeSeriesType, TypedArray, create_store,
};

fn series(initial_year: i32, length: usize, base: f64) -> SingleTimeSeries {
    let initial_timestamp = Utc.with_ymd_and_hms(initial_year, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let values: Vec<f64> = (0..length).map(|i| base + i as f64).collect();
    let data = TypedArray::from_f64(vec![length], &values);
    SingleTimeSeries::new(initial_timestamp, resolution, data, "load")
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
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            Some("MW".into()),
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
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
        )
        .unwrap();

    let err = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
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
            TimeSeriesData::SingleTimeSeries(s1.clone()),
            features_with_year(2030),
            None,
        )
        .unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s2.clone()),
            features_with_year(2035),
            None,
        )
        .unwrap();

    let all = store
        .list_time_series(ListFilter::new().owner_id(1))
        .unwrap();
    assert_eq!(all.len(), 2);

    // Subset filter — only the 2035 row.
    let only_2035 = store
        .list_time_series(
            ListFilter::new()
                .owner_id(1)
                .features(features_with_year(2035)),
        )
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
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
        )
        .unwrap();
    store
        .add_time_series(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
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
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
            None,
        )
        .unwrap();
    let k2 = store
        .add_time_series(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s.clone()),
            Features::new(),
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
            TimeSeriesData::SingleTimeSeries(s_ok.clone()),
            Features::new(),
            None,
        )
        .unwrap();

    // Bulk: first item succeeds, second collides with the existing (1,"load",None,{}).
    let bulk = vec![
        AddRequest {
            owner_id: 2,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            data: TimeSeriesData::SingleTimeSeries(s_ok.clone()),
            features: Features::new(),
            units: None,

            logical_type: None,
        },
        AddRequest {
            owner_id: 1,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            data: TimeSeriesData::SingleTimeSeries(s_dup.clone()),
            features: Features::new(),
            units: None,

            logical_type: None,
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
    let data = TypedArray::from_f64(vec![6], &[10.0, 20.0, 30.0, 40.0, 50.0, 60.0]);
    let s = SingleTimeSeries::new(initial, resolution, data, "load");

    let key = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s),
            Features::new(),
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
    assert_eq!(single.data.to_f64_vec().unwrap(), vec![30.0, 40.0, 50.0]);
}

#[test]
fn clear_by_owner() {
    let mut store = create_store(None, true).unwrap();
    let s = series(2024, 12, 1.0);

    for owner in [1i64, 2, 3] {
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

    // Create a writable store at `path` (catalog sqlite) and add a row.
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let s = series(2024, 12, 1.0);
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s),
                Features::new(),
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
            TimeSeriesData::SingleTimeSeries(s),
            Features::new(),
            None,
        )
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::ReadOnlyStore));
}

#[test]
fn distinct_resolutions_returned_sorted() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let data = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);

    for (i, resolution) in [
        Duration::hours(1),
        Duration::minutes(15),
        Duration::hours(4),
    ]
    .into_iter()
    .enumerate()
    {
        let s = SingleTimeSeries::new(initial, resolution, data.clone(), "load");
        store
            .add_time_series(
                (i + 1) as i64,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(s),
                Features::new(),
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

#[test]
fn non_sequential_round_trip_and_time_slice() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let timestamps = vec![
        initial,
        initial + Duration::hours(3),
        initial + Duration::hours(4),
        initial + Duration::days(2),
    ];
    let data = TypedArray::from_f64(vec![4], &[10.0, 20.0, 30.0, 40.0]);
    let series = NonSequentialTimeSeries::new(timestamps.clone(), data, "availability").unwrap();
    let key = store
        .add_time_series(
            7,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(series),
            Features::new(),
            Some("MW".into()),
        )
        .unwrap();

    assert_eq!(
        key.time_series_type,
        TimeSeriesType::NonSequentialTimeSeries
    );
    assert_eq!(key.resolution, None);
    let metadata = store.get_metadata(&key).unwrap();
    assert_eq!(metadata.timestamps, Some(timestamps.clone()));
    assert_eq!(metadata.resolution, None);

    let got = store
        .get_time_series(
            &key,
            Some((initial + Duration::hours(2), initial + Duration::days(1))),
        )
        .unwrap();
    let irregular = got.as_non_sequential().unwrap();
    assert_eq!(irregular.timestamps, timestamps[1..3]);
    assert_eq!(irregular.data.to_f64_vec().unwrap(), vec![20.0, 30.0]);

    let counts = store.get_time_series_counts().unwrap();
    assert_eq!(counts.static_time_series, 1);
    assert!(
        store
            .get_resolutions(Some(TimeSeriesType::NonSequentialTimeSeries))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn non_sequential_validates_timestamps() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let data = TypedArray::from_f64(vec![2], &[1.0, 2.0]);
    assert!(NonSequentialTimeSeries::new(vec![initial], data.clone(), "test").is_err());
    assert!(NonSequentialTimeSeries::new(vec![initial, initial], data, "test").is_err());
}

#[test]
fn duplicate_non_sequential_key_is_rejected() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    for values in [[1.0, 2.0], [3.0, 4.0]] {
        let series = NonSequentialTimeSeries::new(
            vec![initial, initial + Duration::hours(1)],
            TypedArray::from_f64(vec![2], &values),
            "events",
        )
        .unwrap();
        let result = store.add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(series),
            Features::new(),
            None,
        );
        if values[0] == 1.0 {
            result.unwrap();
        } else {
            assert!(matches!(
                result.unwrap_err(),
                TimeSeriesError::DuplicateTimeSeries
            ));
        }
    }
}
