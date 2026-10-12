//! A rewrite (`persist_to`, `compact`) writes each packed pool in whole blocks
//! of at most the store's write-buffer budget, one dataset per block, from
//! either backend, and every array survives the trip.
//!
//! The budget is what makes this testable at test size: below it a pool is one
//! block, which every other test already covers, and the multi-block path is
//! what the materialization exists for — it is the bound on how much of a
//! large store a save holds in memory at once.

use chrono::{Duration, TimeZone, Utc};
use hdf5_metno as h5;
use infrastore_core::{
    AddRequest, CatalogMode, Compression, ListFilter, OwnerCategory, ReadWindow, SingleTimeSeries,
    Store, TimeSeriesData, TypedArray,
};
use std::path::Path;

const COUNT: usize = 10;
const LENGTH: usize = 24;
/// One column's bytes: `LENGTH` f64 steps.
const COLUMN_BYTES: usize = LENGTH * 8;
const POOL: &str = "sts_f64_s_24_PT1H";

fn request(owner: i64) -> AddRequest {
    let values: Vec<f64> = (0..LENGTH)
        .map(|i| owner as f64 * 100.0 + i as f64)
        .collect();
    AddRequest::new(
        owner,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            Duration::hours(1),
            TypedArray::from_f64(vec![LENGTH], &values),
            "load",
        )),
    )
}

fn fill(store: &mut Store) {
    let mut bulk = store.bulk_add();
    for owner in 0..COUNT as i64 {
        bulk.push(request(owner));
    }
    bulk.commit().unwrap();
}

/// Width of every dataset of the test pool, in name order.
fn pool_widths(path: &Path) -> Vec<usize> {
    let file = h5::File::open(path).unwrap();
    let group = file.group("time_series/single").unwrap();
    let mut names: Vec<String> = group
        .member_names()
        .unwrap()
        .into_iter()
        .filter(|n| n.starts_with(POOL) && !n.ends_with("_h"))
        .collect();
    names.sort();
    names
        .iter()
        .map(|n| group.dataset(n).unwrap().shape()[1])
        .collect()
}

fn read_all(store: &Store) -> Vec<(i64, TimeSeriesData)> {
    let mut rows = store.list_metadata(ListFilter::default()).unwrap();
    rows.sort_by_key(|m| m.owner_id);
    rows.into_iter()
        .map(|m| {
            let data = store
                .read_by_id(m.id.unwrap(), ReadWindow::default())
                .unwrap();
            (m.owner_id, data)
        })
        .collect()
}

fn assert_blocks_and_round_trip(expected: &[(i64, TimeSeriesData)], saved: &Path) {
    let widths = pool_widths(saved);
    assert_eq!(
        widths,
        vec![4, 4, 2],
        "a ten-column pool under a four-column budget is three datasets"
    );
    let store = Store::open(saved, true).unwrap();
    assert_eq!(
        read_all(&store),
        expected,
        "every array reads back as written"
    );
    let ids: Vec<_> = store
        .list_metadata(ListFilter::default())
        .unwrap()
        .into_iter()
        .map(|m| m.id.unwrap())
        .collect();
    assert_eq!(
        store
            .read_by_ids(&ids, ReadWindow::default())
            .unwrap()
            .len(),
        COUNT,
        "the bulk read spans every block"
    );
}

#[test]
fn persist_from_memory_writes_each_pool_in_budget_sized_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let mut store =
        Store::create_with_catalog(None, true, Compression::None, CatalogMode::InMemory).unwrap();
    fill(&mut store);
    let expected = read_all(&store);
    store.set_write_buffer_bytes(COLUMN_BYTES * 4).unwrap();

    let out = dir.path().join("out.h5");
    store.persist_to(&out).unwrap();
    drop(store);
    assert_blocks_and_round_trip(&expected, &out);
}

#[test]
fn persist_from_disk_writes_each_pool_in_budget_sized_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.h5");
    let mut store = Store::create(Some(&src), false).unwrap();
    fill(&mut store);
    // The source is one ten-column dataset; only the rewrite splits it.
    assert_eq!(pool_widths(&src), vec![COUNT]);
    let expected = read_all(&store);
    store.set_write_buffer_bytes(COLUMN_BYTES * 4).unwrap();

    let out = dir.path().join("out.h5");
    store.persist_to(&out).unwrap();
    drop(store);
    assert_blocks_and_round_trip(&expected, &out);
}

#[test]
fn compact_writes_each_pool_in_budget_sized_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.h5");
    let mut store = Store::create(Some(&path), false).unwrap();
    fill(&mut store);
    let expected = read_all(&store);
    store.set_write_buffer_bytes(COLUMN_BYTES * 4).unwrap();

    store.compact().unwrap();
    assert_eq!(
        read_all(&store),
        expected,
        "the live handle reads the rewritten file"
    );
    drop(store);
    assert_blocks_and_round_trip(&expected, &path);
}
