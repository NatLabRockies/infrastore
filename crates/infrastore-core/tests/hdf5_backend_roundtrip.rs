//! Store-level integration tests for the direct-HDF5 backend: round-trip
//! static + forecast data through the full `Store` API, reopen, and
//! persist_to.

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    Deterministic, Features, ListFilter, OwnerCategory, SingleTimeSeries, TimeSeriesData,
    TypedArray, create_store, open_store,
};

fn series(length: usize, base: f64) -> SingleTimeSeries {
    let initial_timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let values: Vec<f64> = (0..length).map(|i| base + i as f64).collect();
    SingleTimeSeries::new(
        initial_timestamp,
        Duration::hours(1),
        TypedArray::from_f64(vec![length], &values),
        "load",
    )
}

fn forecast(horizon: usize, count: usize, base: f64) -> Deterministic {
    let initial_timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let values: Vec<f64> = (0..horizon * count).map(|i| base + i as f64).collect();
    Deterministic::new(
        initial_timestamp,
        Duration::hours(1),
        Duration::hours(horizon as i64),
        Duration::hours(1),
        count,
        TypedArray::from_f64(vec![horizon, count], &values),
        "fc",
    )
    .unwrap()
}

#[test]
fn hdf5_store_round_trip_reopen_and_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");

    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(series(24, 100.0)),
                Features::new(),
            )
            .unwrap();
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::Deterministic(forecast(12, 4, 0.0)),
                Features::new(),
            )
            .unwrap();
        store.flush().unwrap();
    }

    // Reopen: `open_store` validates the backend attribute from the file.
    let mut store = open_store(path.as_path(), false).unwrap();
    let keys = store
        .list_metadata(
            ListFilter::new()
                .owner_id(1)
                .owner_category(OwnerCategory::Component),
        )
        .unwrap();
    assert_eq!(keys.len(), 2);
    for key in &keys {
        let _ = store
            .read_by_id(key.id.unwrap(), infrastore_core::ReadWindow::full())
            .unwrap();
    }
    assert!(store.verify_integrity().unwrap().ok());

    // persist_to must reopen the source afterwards (regression: it used to
    // hard-code a different backend type and fail on hdf5-backend files).
    let dest = dir.path().join("copy.h5");
    store.persist_to(dest.as_path()).unwrap();
    let copy = open_store(dest.as_path(), true).unwrap();
    assert_eq!(
        copy.list_metadata(
            ListFilter::new()
                .owner_id(1)
                .owner_category(OwnerCategory::Component),
        )
        .unwrap()
        .len(),
        2
    );
    assert!(copy.verify_integrity().unwrap().ok());

    // The original store must still be usable after persist (its backend was
    // swapped out for the copy and reopened).
    let got = store
        .read_by_id(keys[0].id.unwrap(), infrastore_core::ReadWindow::full())
        .unwrap();
    drop(got);
}
