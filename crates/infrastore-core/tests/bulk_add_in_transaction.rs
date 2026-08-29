//! Bulk adds issued one after another inside a single transaction, against an
//! in-memory catalog: the shape of a client that buffers additions and flushes
//! every N of them while holding one transaction open over the whole load.
//!
//! The association insert must not open a SQLite *statement journal*. With an
//! enclosing savepoint open, every statement that needs one truncates the
//! sub-journal on close, and the in-memory sub-journal (`memjrnl`) is a chunk
//! list walked from the front — so the cost of each insert grows with every
//! page the transaction has touched so far, and a load that flushes in batches
//! goes quadratic. `INSERT ... RETURNING` is one such statement; it took a
//! 10-batch load of 100k rows from ~1 s to ~22 s, with the last batch fifty
//! times slower than the first. `last_insert_rowid()` after a plain `INSERT`
//! is the same id without the journal.

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, CatalogMode, Compression, Features, ListFilter, OwnerCategory, SingleTimeSeries,
    TimeSeriesData, TypedArray, create_store_with_catalog,
};
use std::time::Instant;

const BATCHES: usize = 8;
const BATCH_SIZE: usize = 10_000;

fn requests(batch: usize) -> Vec<AddRequest> {
    let initial_timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    (0..BATCH_SIZE)
        .map(|i| {
            let owner = (batch * BATCH_SIZE + i) as i64 + 1;
            let data = TypedArray::from_f64(vec![24], &[owner as f64; 24]);
            let series = SingleTimeSeries::new(initial_timestamp, Duration::hours(1), data, "val");
            AddRequest {
                owner_id: owner,
                owner_type: "Generator".into(),
                owner_category: OwnerCategory::Component,
                data: TimeSeriesData::SingleTimeSeries(series),
                features: Features::new(),
            }
        })
        .collect()
}

#[test]
fn successive_bulk_adds_in_one_transaction_do_not_slow_down() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = create_store_with_catalog(
        Some(&dir.path().join("scratch.h5")),
        false,
        Compression::None,
        CatalogMode::InMemory,
    )
    .unwrap();

    store.begin_transaction().unwrap();
    let mut elapsed = Vec::with_capacity(BATCHES);
    let mut ids = Vec::with_capacity(BATCHES * BATCH_SIZE);
    for b in 0..BATCHES {
        let t = Instant::now();
        let added = store.add_time_series_bulk(requests(b)).unwrap();
        elapsed.push(t.elapsed());
        ids.extend(added.into_iter().map(|a| a.id));
    }
    store.commit_transaction().unwrap();

    // Ids are minted in add order and every one still resolves after the commit.
    assert_eq!(ids, (1..=(BATCHES * BATCH_SIZE) as i64).collect::<Vec<_>>());
    assert!(store.association_exists(*ids.last().unwrap()).unwrap());

    // Per-batch cost is flat in the size of the store. The bound is loose
    // enough for a noisy CI box (a healthy run is ~1.1x); the regression this
    // guards was an order of magnitude past it by the last batch.
    let first = elapsed[0].as_secs_f64();
    let last = elapsed[BATCHES - 1].as_secs_f64();
    assert!(
        last < 4.0 * first,
        "batch {BATCHES} took {last:.3}s against {first:.3}s for batch 1 ({:?})",
        elapsed
    );
}

#[test]
fn list_keys_with_id_reports_the_ids_the_writes_handed_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = create_store_with_catalog(
        Some(&dir.path().join("scratch.h5")),
        false,
        Compression::None,
        CatalogMode::InMemory,
    )
    .unwrap();
    let added = store.add_time_series_bulk(requests(0)).unwrap();

    let listed = store
        .list_keys_with_id(ListFilter::new().owner_category(OwnerCategory::Component))
        .unwrap();
    assert_eq!(listed.len(), added.len());
    let mut by_owner: std::collections::HashMap<i64, i64> = listed
        .into_iter()
        .map(|(key, id)| (key.identity().owner_id, id.expect("a minted id")))
        .collect();
    for a in &added {
        assert_eq!(by_owner.remove(&a.key.identity().owner_id), Some(a.id));
    }
    assert!(by_owner.is_empty());
}
