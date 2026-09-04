//! A process killed right after a write call returned.
//!
//! In its own file on purpose: the test below forks a child that aborts, and
//! libhdf5 opens files without `O_CLOEXEC`, so a forked child inherits every
//! HDF5 descriptor -- and every lock -- its parent's other threads hold at
//! that moment. Sharing a process with other store tests made them fail on
//! "unable to lock file" whenever this one's child was alive.

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, ListFilter, OwnerCategory, ReadWindow, SingleTimeSeries, Store, TimeSeriesData,
    TypedArray, create_store, open_store,
};

fn request(owner: i64, base: f64) -> AddRequest {
    let values: Vec<f64> = (0..24).map(|i| base + i as f64).collect();
    let series = SingleTimeSeries::new(
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        Duration::hours(1),
        TypedArray::from_f64(vec![24], &values),
        "load",
    );
    AddRequest::new(
        owner,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::SingleTimeSeries(series),
    )
}

fn add(store: &mut Store, owner: i64, base: f64) {
    store.add(request(owner, base)).unwrap();
}

fn first_value(store: &Store, owner: i64) -> f64 {
    let row = store
        .list_metadata(ListFilter::new().owner_id(owner))
        .unwrap()
        .pop()
        .expect("a row for this owner");
    let data = store
        .read_by_id(row.id.unwrap(), ReadWindow::full())
        .unwrap();
    data.as_single().unwrap().data.to_f64_vec().unwrap()[0]
}

/// Every write is durable in both halves before it returns: the arrays are
/// flushed ahead of the catalog commit (once per call, or once per
/// transaction), so a process killed right afterwards leaves no row naming an
/// array the file never received. libhdf5 writes its chunk cache back lazily,
/// and the catalog commit is durable at once, which is what made the window
/// real.
#[test]
fn writes_are_durable_before_they_return() {
    if let Ok(path) = std::env::var("INFRASTORE_CRASH_CHILD") {
        let mut store = open_store(std::path::Path::new(&path), false).unwrap();
        store.begin_transaction().unwrap();
        add(&mut store, 2, 10.0);
        store.commit_transaction().unwrap();
        store.add_time_series_bulk(vec![request(3, 20.0)]).unwrap();
        add(&mut store, 4, 30.0);
        // Neither closed nor flushed: the child dies with whatever libhdf5
        // still holds in its write-back caches.
        std::process::abort();
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        add(&mut store, 1, 0.0);
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "writes_are_durable_before_they_return"])
        .env("INFRASTORE_CRASH_CHILD", &path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "the child is meant to abort");

    let store = open_store(&path, true).unwrap();
    assert_eq!(first_value(&store, 1), 0.0);
    assert_eq!(first_value(&store, 2), 10.0);
    assert_eq!(first_value(&store, 3), 20.0);
    assert_eq!(first_value(&store, 4), 30.0);
    assert!(store.verify_integrity().unwrap().ok());
}
