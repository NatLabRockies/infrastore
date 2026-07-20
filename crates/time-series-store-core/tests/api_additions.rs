//! Tests for the Phase-1 additive API surface: `AddRequest`/`Store::add`,
//! bulk/filtered delete, time-sliced bulk read, discovery enumerations, rename,
//! and serde coverage. All additive — no on-disk format change.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use time_series_store_core::{
    AddRequest, Deterministic, FeatureValue, Features, KeyIdentity, ListFilter, OwnerCategory,
    Period, SingleTimeSeries, TimeSeriesData, TimeSeriesError, TimeSeriesMetadata, TimeSeriesType,
    TypedArray, create_store,
};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
}

fn sts(name: &str, base: f64, length: usize) -> SingleTimeSeries {
    let values: Vec<f64> = (0..length).map(|i| base + i as f64).collect();
    SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![length], &values),
        name,
    )
}

fn det(name: &str, base: f64) -> Deterministic {
    // H=2, count=3, interval 1h.
    let vals: Vec<f64> = (0..6).map(|i| base + i as f64).collect();
    Deterministic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        3,
        TypedArray::from_f64(vec![2, 3], &vals),
        name,
    )
    .unwrap()
}

// ---- 1.1 AddRequest builder + Store::add ----------------------------------

#[test]
fn store_add_preserves_logical_type() {
    let mut store = create_store(None, true).unwrap();
    let mut features: Features = BTreeMap::new();
    features.insert("scenario".into(), FeatureValue::Str("base".into()));

    let key = store
        .add(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)),
            )
            .with_features(features.clone())
            .with_units("MW")
            .with_logical_type("QuadraticFunctionData"),
        )
        .unwrap();

    let meta = store.get_metadata(key.identity()).unwrap();
    assert_eq!(meta.logical_type.as_deref(), Some("QuadraticFunctionData"));
    assert_eq!(meta.units.as_deref(), Some("MW"));
    assert_eq!(meta.features, features);
}

#[test]
fn bulk_push_preserves_logical_type() {
    let mut store = create_store(None, true).unwrap();
    let keys = {
        let mut bulk = store.bulk_add();
        bulk.push(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts("a", 1.0, 4)),
            )
            .with_logical_type("TypeA"),
        );
        bulk.push(
            AddRequest::new(
                2,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts("b", 2.0, 4)),
            )
            .with_logical_type("TypeB"),
        );
        bulk.commit().unwrap()
    };
    assert_eq!(keys.len(), 2);
    assert_eq!(
        store
            .get_metadata(keys[0].identity())
            .unwrap()
            .logical_type
            .as_deref(),
        Some("TypeA")
    );
    assert_eq!(
        store
            .get_metadata(keys[1].identity())
            .unwrap()
            .logical_type
            .as_deref(),
        Some("TypeB")
    );
}

// ---- 1.5 bulk / filtered delete -------------------------------------------

#[test]
fn remove_by_filter_removes_matching_and_reclaims_arrays() {
    let mut store = create_store(None, true).unwrap();
    for owner in 1..=3 {
        store
            .add(AddRequest::new(
                owner,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts("load", owner as f64 * 10.0, 4)),
            ))
            .unwrap();
    }
    // Distinct array per owner (distinct values) -> 3 arrays.
    assert_eq!(store.num_distinct_arrays().unwrap(), 3);

    let removed = store
        .remove_by_filter(ListFilter::new().owner_id(2))
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 2);
    // The owner-2 array is now unreferenced and dropped.
    assert_eq!(store.num_distinct_arrays().unwrap(), 2);
}

#[test]
fn remove_by_filter_empty_match_is_ok_zero() {
    let mut store = create_store(None, true).unwrap();
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)),
        ))
        .unwrap();
    let removed = store
        .remove_by_filter(ListFilter::new().owner_id(999))
        .unwrap();
    assert_eq!(removed, 0);
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 1);
}

#[test]
fn remove_bulk_rolls_back_on_missing_key() {
    let mut store = create_store(None, true).unwrap();
    let k1 = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)),
        ))
        .unwrap();
    // A bogus identity that matches nothing.
    let missing = KeyIdentity {
        owner_id: 999,
        owner_category: OwnerCategory::Component,
        time_series_type: TimeSeriesType::SingleTimeSeries,
        name: "nope".into(),
        resolution: Some(Period::fixed(Duration::hours(1))),
        interval: None,
        features: Features::new(),
    };
    let err = store
        .remove_time_series_bulk(&[k1.identity(), &missing])
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::NotFound));
    // Nothing removed: the valid key survives (all-or-nothing rollback).
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 1);
}

#[test]
fn remove_bulk_reclaims_shared_array_only_when_last_reference_gone() {
    let mut store = create_store(None, true).unwrap();
    // Two owners, identical data -> one shared array (content-addressed).
    let k1 = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 5.0, 4)),
        ))
        .unwrap();
    let k2 = store
        .add(AddRequest::new(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 5.0, 4)),
        ))
        .unwrap();
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);

    // Removing both in one batch reclaims the shared array.
    let removed = store
        .remove_time_series_bulk(&[k1.identity(), k2.identity()])
        .unwrap();
    assert_eq!(removed, 2);
    assert_eq!(store.num_distinct_arrays().unwrap(), 0);
}

// ---- 1.6 time-sliced bulk read --------------------------------------------

#[test]
fn bulk_read_range_matches_per_key_get_time_series() {
    let mut store = create_store(None, true).unwrap();
    let k1 = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 100.0, 8)),
        ))
        .unwrap();
    let k2 = store
        .add(AddRequest::new(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 200.0, 8)),
        ))
        .unwrap();

    let range = (t0() + Duration::hours(2), t0() + Duration::hours(5));
    let keys = [k1.identity(), k2.identity()];

    let sliced = store.bulk_read_range(&keys, Some(range)).unwrap();
    for (i, k) in keys.iter().enumerate() {
        let per_key = store.get_time_series(k, Some(range)).unwrap();
        assert_eq!(
            sliced[i], per_key,
            "sliced bulk differs from per-key at {i}"
        );
    }

    // None behaves exactly like bulk_read.
    let full = store.bulk_read_range(&keys, None).unwrap();
    assert_eq!(full, store.bulk_read(&keys).unwrap());
}

// ---- 1.7 discovery enumerations -------------------------------------------

#[test]
fn discovery_enumerations() {
    let mut store = create_store(None, true).unwrap();
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("load", 1.0, 4)),
        ))
        .unwrap();
    store
        .add(AddRequest::new(
            2,
            "Bus",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("voltage", 2.0, 4)),
        ))
        .unwrap();
    store
        .add(AddRequest::new(
            3,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::Deterministic(det("gen_forecast", 0.0)),
        ))
        .unwrap();

    // Intervals: only the forecast carries one (1h).
    let intervals = store.get_intervals(None).unwrap();
    assert_eq!(intervals, vec![Period::fixed(Duration::hours(1))]);
    assert!(
        store
            .get_intervals(Some(TimeSeriesType::SingleTimeSeries))
            .unwrap()
            .is_empty()
    );

    // Names: distinct, sorted.
    let names = store.list_names(ListFilter::new()).unwrap();
    assert_eq!(names, vec!["gen_forecast", "load", "voltage"]);
    // Filter interaction.
    let gen_names = store
        .list_names(ListFilter::new().owner_type("Generator"))
        .unwrap();
    assert_eq!(gen_names, vec!["gen_forecast", "load"]);

    // Owner types: distinct, sorted.
    let owner_types = store.list_owner_types(ListFilter::new()).unwrap();
    assert_eq!(owner_types, vec!["Bus", "Generator"]);

    // Empty store.
    let empty = create_store(None, true).unwrap();
    assert!(empty.get_intervals(None).unwrap().is_empty());
    assert!(empty.list_names(ListFilter::new()).unwrap().is_empty());
    assert!(
        empty
            .list_owner_types(ListFilter::new())
            .unwrap()
            .is_empty()
    );
}

// ---- 1.8 rename ------------------------------------------------------------

#[test]
fn rename_moves_the_association() {
    let mut store = create_store(None, true).unwrap();
    let key = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("old", 10.0, 4)),
        ))
        .unwrap();

    let new_key = store.rename_time_series(key.identity(), "new").unwrap();
    assert_eq!(new_key.name(), "new");

    // Old key is gone, new key readable.
    assert!(matches!(
        store.get_metadata(key.identity()),
        Err(TimeSeriesError::NotFound)
    ));
    assert_eq!(store.get_metadata(new_key.identity()).unwrap().name, "new");
}

#[test]
fn rename_collision_is_duplicate() {
    let mut store = create_store(None, true).unwrap();
    let a = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("a", 1.0, 4)),
        ))
        .unwrap();
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(sts("b", 2.0, 4)),
        ))
        .unwrap();
    // Renaming `a` to `b` collides with the existing series identity.
    let err = store.rename_time_series(a.identity(), "b").unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateTimeSeries));
}

#[test]
fn rename_missing_key_is_not_found() {
    let mut store = create_store(None, true).unwrap();
    let missing = KeyIdentity {
        owner_id: 1,
        owner_category: OwnerCategory::Component,
        time_series_type: TimeSeriesType::SingleTimeSeries,
        name: "nope".into(),
        resolution: Some(Period::fixed(Duration::hours(1))),
        interval: None,
        features: Features::new(),
    };
    assert!(matches!(
        store.rename_time_series(&missing, "x"),
        Err(TimeSeriesError::NotFound)
    ));
}

// ---- 1.9 serde coverage ----------------------------------------------------

#[test]
fn period_serializes_as_iso8601_string() {
    assert_eq!(
        serde_json::to_string(&Period::fixed(Duration::hours(1))).unwrap(),
        "\"PT1H\""
    );
    assert_eq!(
        serde_json::to_string(&Period::Months(1)).unwrap(),
        "\"P1M\""
    );
    let back: Period = serde_json::from_str("\"P1Y\"").unwrap();
    assert_eq!(back, Period::Months(12));
}

#[test]
fn metadata_and_data_json_round_trip() {
    let mut store = create_store(None, true).unwrap();
    let key = store
        .add(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts("load", 10.0, 4)),
            )
            .with_units("MW")
            .with_logical_type("QuadraticFunctionData"),
        )
        .unwrap();

    let meta = store.get_metadata(key.identity()).unwrap();
    let json = serde_json::to_string(&meta).unwrap();
    let back: TimeSeriesMetadata = serde_json::from_str(&json).unwrap();
    assert_eq!(meta, back);

    // Each TimeSeriesData variant round-trips.
    let data = store.get_time_series(key.identity(), None).unwrap();
    let d_json = serde_json::to_string(&data).unwrap();
    let d_back: TimeSeriesData = serde_json::from_str(&d_json).unwrap();
    assert_eq!(data, d_back);

    let forecast = TimeSeriesData::Deterministic(det("f", 0.0));
    let f_json = serde_json::to_string(&forecast).unwrap();
    assert_eq!(
        forecast,
        serde_json::from_str::<TimeSeriesData>(&f_json).unwrap()
    );
}
