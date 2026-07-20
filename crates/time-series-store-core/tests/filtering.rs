//! Tests for `ListFilter::name_glob` (round-2 plan item 1.1): SQLite `GLOB`
//! pattern matching on the series name, threaded through every
//! `ListFilter`-taking query path.

use chrono::{DateTime, Duration, TimeZone, Utc};
use time_series_store_core::{
    AddRequest, ListFilter, OwnerCategory, SingleTimeSeries, TimeSeriesData, TypedArray,
    create_store,
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

/// owner 1 gets wind_speed/wind_dir, owner 2 gets solar_irradiance/Wind_gust.
fn populated() -> time_series_store_core::Store {
    let mut store = create_store(None, true).unwrap();
    for (owner, name, base) in [
        (1, "wind_speed", 1.0),
        (1, "wind_dir", 2.0),
        (2, "solar_irradiance", 3.0),
        (2, "Wind_gust", 4.0),
    ] {
        store
            .add(AddRequest::new(
                owner,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(sts(name, base, 4)),
            ))
            .unwrap();
    }
    store
}

#[test]
fn glob_star_and_question_wildcards() {
    let store = populated();

    let names = store
        .list_names(ListFilter::new().name_glob("wind_*"))
        .unwrap();
    assert_eq!(names, vec!["wind_dir", "wind_speed"]);

    // `?` matches exactly one character.
    let names = store
        .list_names(ListFilter::new().name_glob("wind_di?"))
        .unwrap();
    assert_eq!(names, vec!["wind_dir"]);

    // GLOB is case-sensitive: capital-W series is not matched.
    let keys = store
        .list_keys(ListFilter::new().name_glob("wind*"))
        .unwrap();
    assert_eq!(keys.len(), 2);
    let keys = store
        .list_keys(ListFilter::new().name_glob("Wind*"))
        .unwrap();
    assert_eq!(keys.len(), 1);
}

#[test]
fn glob_composes_with_other_filters_as_and() {
    let store = populated();

    // Glob + owner filter.
    let keys = store
        .list_keys(ListFilter::new().owner_id(2).name_glob("*ind*"))
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].identity().name, "Wind_gust");

    // Exact name + glob both apply (AND): consistent pair matches...
    let keys = store
        .list_keys(ListFilter::new().name("wind_speed").name_glob("wind_*"))
        .unwrap();
    assert_eq!(keys.len(), 1);
    // ...contradictory pair matches nothing.
    let keys = store
        .list_keys(ListFilter::new().name("wind_speed").name_glob("solar_*"))
        .unwrap();
    assert!(keys.is_empty());

    // list_time_series and list_owner_types honor it too.
    let rows = store
        .list_time_series(ListFilter::new().name_glob("solar_*"))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "solar_irradiance");
    let owner_types = store
        .list_owner_types(ListFilter::new().name_glob("nope_*"))
        .unwrap();
    assert!(owner_types.is_empty());
}

#[test]
fn glob_no_match_is_empty_not_error() {
    let store = populated();
    assert!(
        store
            .list_keys(ListFilter::new().name_glob("xyz*"))
            .unwrap()
            .is_empty()
    );
    // Reader build over an empty match keeps its existing error semantics.
    assert!(
        store
            .build_static_reader(ListFilter::new().name_glob("xyz*"))
            .is_err()
    );
}

#[test]
fn remove_by_filter_with_glob() {
    let mut store = populated();
    let removed = store
        .remove_by_filter(ListFilter::new().name_glob("wind_*"))
        .unwrap();
    assert_eq!(removed, 2);
    let names = store.list_names(ListFilter::new()).unwrap();
    assert_eq!(names, vec!["Wind_gust", "solar_irradiance"]);
}
