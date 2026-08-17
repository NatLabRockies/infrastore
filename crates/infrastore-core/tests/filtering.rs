//! Tests for the descriptive `ListFilter` predicates, threaded through every
//! `ListFilter`-taking query path: `name_glob` (round-2 plan item 1.1), SQLite
//! `GLOB` pattern matching on the series name, and `component_field`, an exact
//! match on the owning component's field.
//!
//! Every case runs against both backends. Matching itself is pure SQLite
//! and so cannot differ, but the filter feeds `build_static_reader` and
//! `remove_by_filter`, whose array-side work does differ between the in-memory
//! and HDF5 backends — and the persisted variant additionally proves the
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

// ---- ListFilter::component_field -------------------------------------------

/// Two owners each carrying a `max_active_power` series and a `rating` series,
/// plus one series that declares no `component_field` at all.
fn populate_fields(store: &mut Store) {
    for (owner, name, field, base) in [
        (1, "max_active_power", Some("max_active_power"), 1.0),
        (1, "rating", Some("rating"), 2.0),
        (2, "max_active_power", Some("max_active_power"), 3.0),
        (2, "rating", Some("rating"), 4.0),
        (3, "legacy", None, 5.0),
    ] {
        let mut data = TimeSeriesData::SingleTimeSeries(sts(name, base, 4));
        if let Some(field) = field {
            data = data.with_component_field(field);
        }
        store
            .add(AddRequest::new(
                owner,
                "Generator",
                OwnerCategory::Component,
                data,
            ))
            .unwrap();
    }
}

#[test]
fn component_field_selects_across_owners() {
    for_each_backend(populate_fields, |store, (), backend| {
        // The point of the filter: one field, every component that varies it.
        let keys = store
            .list_keys(ListFilter::new().component_field("max_active_power"))
            .unwrap();
        let mut owners: Vec<i64> = keys.iter().map(|k| k.owner_id()).collect();
        owners.sort();
        assert_eq!(owners, vec![1, 2], "{backend}");

        // ...and it composes with the owner scope, which is the other half of
        // "the series for this field on this component".
        let scoped = store
            .list_keys(
                ListFilter::new()
                    .owner_id(1)
                    .component_field("max_active_power"),
            )
            .unwrap();
        assert_eq!(scoped.len(), 1, "{backend}");
        assert_eq!(scoped[0].name(), "max_active_power", "{backend}");
    });
}

#[test]
fn component_field_matching_is_exact_and_case_sensitive() {
    for_each_backend(populate_fields, |store, (), backend| {
        // No prefix or glob semantics: this is an equality predicate, matching
        // every other identifier filter in the catalog.
        for pattern in ["max_active", "max_active_power*", "Max_Active_Power"] {
            assert!(
                store
                    .list_keys(ListFilter::new().component_field(pattern))
                    .unwrap()
                    .is_empty(),
                "{pattern} should not match ({backend})"
            );
        }
    });
}

#[test]
fn component_field_never_matches_a_row_that_declares_none() {
    for_each_backend(populate_fields, |store, (), backend| {
        // SQL equality is never true against NULL, so the `legacy` row -- and
        // every row written before the column existed -- is unreachable through
        // this filter. Documented on `ListFilter::component_field`, and pinned
        // here because the partial index that serves the filter depends on it.
        let names = store
            .list_names(ListFilter::new().component_field("legacy"))
            .unwrap();
        assert!(names.is_empty(), "{backend}");
        assert_eq!(
            store.list_names(ListFilter::new()).unwrap().len(),
            3,
            "{backend}"
        );
    });
}

#[test]
fn component_field_selects_the_same_series_for_a_static_reader() {
    for_each_backend(populate_fields, |store, (), backend| {
        // The columnar sweep is the motivating case: one grid of every
        // component's max_active_power.
        let reader = store
            .build_static_reader(
                ListFilter::new()
                    .component_field("max_active_power")
                    .resolution(Duration::hours(1)),
            )
            .unwrap();
        let mut owners: Vec<i64> = reader
            .groups()
            .iter()
            .flat_map(|g| g.keys().iter().map(|k| k.owner_id()))
            .collect();
        owners.sort();
        assert_eq!(owners, vec![1, 2], "{backend}");
    });
}

#[test]
fn remove_by_filter_with_component_field() {
    for_each_backend_mut(populate_fields, |store, (), backend| {
        let removed = store
            .remove_by_filter(ListFilter::new().component_field("rating"))
            .unwrap();
        assert_eq!(removed, 2, "{backend}");
        let names = store.list_names(ListFilter::new()).unwrap();
        assert_eq!(names, vec!["legacy", "max_active_power"], "{backend}");
    });
}
