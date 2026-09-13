//! Regressions from the core correctness review: values the write path used to
//! accept that could not be read, exported, or listed back afterwards, plus the
//! read-side defects found alongside them.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, FeatureValue, Features, ListFilter, NonSequentialTimeSeries, OwnerCategory,
    PersistentTimeSeries, Probabilistic, ReadWindow, SingleTimeSeries, Store, TimeSeriesData,
    TimeSeriesError, TypedArray,
};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

fn req(owner: i64, data: TimeSeriesData) -> AddRequest {
    AddRequest::new(owner, "Generator", OwnerCategory::Component, data)
}

fn sts(start: DateTime<Utc>, resolution: Duration, values: &[f64]) -> TimeSeriesData {
    TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
        start,
        resolution,
        TypedArray::from_f64(vec![values.len()], values),
        "load",
    ))
}

fn assert_invalid(err: TimeSeriesError) {
    assert!(matches!(err, TimeSeriesError::InvalidParameter(_)), "{err}");
}

#[test]
fn a_year_the_catalog_cannot_spell_is_refused() {
    let mut store = Store::create(None, true).unwrap();
    store
        .add(req(1, sts(t0(), Duration::hours(1), &[1.0])))
        .unwrap();
    for year in [10000, -1] {
        let start = Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap();
        assert_invalid(
            store
                .add(req(2, sts(start, Duration::hours(1), &[1.0])))
                .unwrap_err(),
        );
    }
    for year in [0, 9999] {
        let start = Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap();
        store
            .add(req(
                year as i64 + 10,
                sts(start, Duration::hours(1), &[1.0]),
            ))
            .unwrap();
    }
    assert_eq!(store.list_metadata(ListFilter::new()).unwrap().len(), 3);
}

#[test]
fn non_finite_percentiles_are_refused() {
    for percentiles in [
        vec![f64::NAN, 0.5],
        vec![0.5, f64::NAN],
        vec![0.5, f64::INFINITY],
    ] {
        let prob = Probabilistic {
            percentiles,
            ..Probabilistic::new(
                t0(),
                Duration::hours(1),
                Duration::hours(1),
                Duration::hours(1),
                1,
                vec![0.1, 0.9],
                TypedArray::from_f64(vec![2, 1, 1], &[1.0, 2.0]),
                "p",
            )
            .unwrap()
        };
        let mut store = Store::create(None, true).unwrap();
        assert_invalid(
            store
                .add(req(1, TimeSeriesData::Probabilistic(prob)))
                .unwrap_err(),
        );
        assert!(store.list_metadata(ListFilter::new()).unwrap().is_empty());
    }
}

#[test]
fn a_sub_millisecond_resolution_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let mut store = Store::create(Some(path.as_path()), false).unwrap();
    store
        .add(req(1, sts(t0(), Duration::hours(1), &[1.0, 2.0])))
        .unwrap();
    let odd = Duration::hours(1) + Duration::microseconds(1);
    assert_invalid(store.add(req(2, sts(t0(), odd, &[3.0, 4.0]))).unwrap_err());
}

#[test]
fn a_whole_irregular_read_sees_its_own_transaction() {
    let stamps = vec![t0(), t0() + Duration::hours(3)];
    let values = TypedArray::from_f64(vec![2], &[1.0, 2.0]);
    let nsts = NonSequentialTimeSeries::new(stamps.clone(), values.clone(), "n").unwrap();
    let step = PersistentTimeSeries::new(stamps, values, "p").unwrap();

    let mut store = Store::create(None, true).unwrap();
    store.begin_transaction().unwrap();
    for data in [
        TimeSeriesData::NonSequentialTimeSeries(nsts),
        TimeSeriesData::PersistentTimeSeries(step),
    ] {
        let id = store.add(req(1, data)).unwrap();
        store.read_by_id(id, ReadWindow::full()).unwrap();
    }
    store.commit_transaction().unwrap();
}

#[test]
fn an_infinite_feature_refuses_the_openapi_export() {
    let mut store = Store::create(None, true).unwrap();
    let mut features = Features::new();
    features.insert("cap".into(), FeatureValue::Float(f64::INFINITY));
    store
        .add(req(1, sts(t0(), Duration::hours(1), &[1.0])).with_features(features))
        .unwrap();
    let err = store
        .export_time_series_associations_openapi(&ListFilter::new())
        .unwrap_err();
    assert_invalid(err);
}

#[test]
fn a_negative_zero_filter_matches_nothing_on_either_path() {
    let mut store = Store::create(None, true).unwrap();
    let mut features = Features::new();
    features.insert("x".into(), FeatureValue::Float(0.0));
    store
        .add(req(1, sts(t0(), Duration::hours(1), &[1.0])).with_features(features))
        .unwrap();
    let mut query = Features::new();
    query.insert("x".into(), FeatureValue::Float(-0.0));
    let filter = ListFilter::new().features(query);
    assert!(store.list_metadata(filter.clone()).unwrap().is_empty());
    assert!(!store.has_any_time_series(filter).unwrap());
}
