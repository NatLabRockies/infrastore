//! Tests for `ListFilter::name_glob` (round-2 plan item 1.1): SQLite `GLOB`
//! pattern matching on the series name, threaded through every
//! `ListFilter`-taking query path.
//!
//! Every case runs against both backends. Glob matching itself is pure SQLite
//! and so cannot differ, but the filter feeds `build_static_reader` and
//! `remove_by_filter`, whose array-side work does differ between the in-memory
//! and NetCDF backends — and the persisted variant additionally proves the
//! filter still selects the same rows after a catalog write/reopen cycle.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, ListFilter, OwnerCategory, SingleTimeSeries, Store, TimeSeriesData, TypedArray,
};

mod common;
use common::{for_each_backend, for_each_backend_mut};

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
fn populate(store: &mut Store) {
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
}

#[test]
fn glob_star_and_question_wildcards() {
    for_each_backend(populate, |store, (), backend| {
        let names = store
            .list_names(ListFilter::new().name_glob("wind_*"))
            .unwrap();
        assert_eq!(names, vec!["wind_dir", "wind_speed"], "{backend}");

        // `?` matches exactly one character.
        let names = store
            .list_names(ListFilter::new().name_glob("wind_di?"))
            .unwrap();
        assert_eq!(names, vec!["wind_dir"], "{backend}");

        // GLOB is case-sensitive: capital-W series is not matched.
        let keys = store
            .list_keys(ListFilter::new().name_glob("wind*"))
            .unwrap();
        assert_eq!(keys.len(), 2, "{backend}");
        let keys = store
            .list_keys(ListFilter::new().name_glob("Wind*"))
            .unwrap();
        assert_eq!(keys.len(), 1, "{backend}");
    });
}

#[test]
fn glob_composes_with_other_filters_as_and() {
    for_each_backend(populate, |store, (), backend| {
        // Glob + owner filter.
        let keys = store
            .list_keys(ListFilter::new().owner_id(2).name_glob("*ind*"))
            .unwrap();
        assert_eq!(keys.len(), 1, "{backend}");
        assert_eq!(keys[0].identity().name, "Wind_gust", "{backend}");

        // Exact name + glob both apply (AND): consistent pair matches...
        let keys = store
            .list_keys(ListFilter::new().name("wind_speed").name_glob("wind_*"))
            .unwrap();
        assert_eq!(keys.len(), 1, "{backend}");
        // ...contradictory pair matches nothing.
        let keys = store
            .list_keys(ListFilter::new().name("wind_speed").name_glob("solar_*"))
            .unwrap();
        assert!(keys.is_empty(), "{backend}");

        // list_time_series and list_owner_types honor it too.
        let rows = store
            .list_time_series(ListFilter::new().name_glob("solar_*"))
            .unwrap();
        assert_eq!(rows.len(), 1, "{backend}");
        assert_eq!(rows[0].name, "solar_irradiance", "{backend}");
        let owner_types = store
            .list_owner_types(ListFilter::new().name_glob("nope_*"))
            .unwrap();
        assert!(owner_types.is_empty(), "{backend}");
    });
}

#[test]
fn glob_no_match_is_empty_not_error() {
    for_each_backend(populate, |store, (), backend| {
        assert!(
            store
                .list_keys(ListFilter::new().name_glob("xyz*"))
                .unwrap()
                .is_empty(),
            "{backend}"
        );
        // Reader build over an empty match keeps its existing error semantics.
        assert!(
            store
                .build_static_reader(ListFilter::new().name_glob("xyz*"))
                .is_err(),
            "{backend}"
        );
    });
}

#[test]
fn glob_selects_the_same_series_for_a_static_reader() {
    for_each_backend(populate, |store, (), backend| {
        let reader = store
            .build_static_reader(
                ListFilter::new()
                    .name_glob("wind_*")
                    .resolution(Duration::hours(1)),
            )
            .unwrap();
        let mut names: Vec<&str> = reader
            .groups()
            .iter()
            .flat_map(|g| g.keys().iter().map(|k| k.name()))
            .collect();
        names.sort();
        assert_eq!(names, vec!["wind_dir", "wind_speed"], "{backend}");
    });
}

#[test]
fn remove_by_filter_with_glob() {
    for_each_backend_mut(populate, |store, (), backend| {
        assert_eq!(store.num_distinct_arrays().unwrap(), 4, "{backend}");
        let removed = store
            .remove_by_filter(ListFilter::new().name_glob("wind_*"))
            .unwrap();
        assert_eq!(removed, 2, "{backend}");
        let names = store.list_names(ListFilter::new()).unwrap();
        assert_eq!(names, vec!["Wind_gust", "solar_irradiance"], "{backend}");
        // The two removed series' arrays are now unreferenced and reclaimed on
        // both backends.
        assert_eq!(store.num_distinct_arrays().unwrap(), 2, "{backend}");
    });
}

#[test]
fn remove_by_filter_with_a_no_match_glob_removes_nothing() {
    for_each_backend_mut(populate, |store, (), backend| {
        let removed = store
            .remove_by_filter(ListFilter::new().name_glob("nope_*"))
            .unwrap();
        assert_eq!(removed, 0, "{backend}");
        assert_eq!(
            store.list_keys(ListFilter::new()).unwrap().len(),
            4,
            "{backend}"
        );
        assert_eq!(store.num_distinct_arrays().unwrap(), 4, "{backend}");
    });
}
