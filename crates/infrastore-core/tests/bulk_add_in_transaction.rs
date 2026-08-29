//! Bulk adds issued one after another inside a single transaction, against an
//! in-memory catalog: the shape of a client that buffers additions and flushes
//! every N of them while holding one transaction open over the whole load.
//!
//! The association insert must not open a SQLite *statement journal*. With an
//! enclosing savepoint open, every statement that needs one truncates the
//! sub-journal on close, and the in-memory sub-journal (`memjrnl`) is a chunk
//! list walked from the front — so the cost of each insert grows with every
//! page the transaction has touched so far, and a load that flushes in batches
//! goes quadratic. `INSERT ... RETURNING` is one such statement; measured, it
//! took a 100k-row load flushed in ten batches from ~1 s to ~22 s, the tenth
//! batch fifty times slower than the first. `last_insert_rowid()` after a plain
//! `INSERT` is the same id without the journal. The batch counts below are
//! smaller than that measurement: the growth shows up long before the tenth.

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, CatalogMode, Compression, Features, ListFilter, OwnerCategory, SingleTimeSeries,
    SupplementalAttributeAssociation, TimeSeriesData, TypedArray, create_store_with_catalog,
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

fn scratch_store(dir: &tempfile::TempDir) -> infrastore_core::Store {
    create_store_with_catalog(
        Some(&dir.path().join("scratch.h5")),
        false,
        Compression::None,
        CatalogMode::InMemory,
    )
    .unwrap()
}

/// The load must stay flat: the median of the second half of the batches within
/// a loose multiple of the median of the first half. A healthy run is ~1.1x and
/// the guarded regression grows with every batch — an order of magnitude past
/// the bound by the last one — so comparing halves separates the two by a wide
/// margin while a single stalled batch, which a shared CI runner will hand out
/// sooner or later, moves neither median. Comparing the last batch against the
/// first would turn that stall into a failure: each batch here is a few tens of
/// milliseconds, so one scheduling hiccup is 4x on its own.
fn assert_flat(elapsed: &[std::time::Duration]) {
    fn median(batches: &[std::time::Duration]) -> f64 {
        let mut secs: Vec<f64> = batches.iter().map(|d| d.as_secs_f64()).collect();
        secs.sort_by(f64::total_cmp);
        secs[secs.len() / 2]
    }

    let (head, tail) = elapsed.split_at(elapsed.len() / 2);
    let (first, last) = (median(head), median(tail));
    assert!(
        last < 4.0 * first,
        "the last {} batches took a median {last:.3}s against {first:.3}s for the first {} ({elapsed:?})",
        tail.len(),
        head.len()
    );
}

#[test]
fn successive_bulk_adds_in_one_transaction_do_not_slow_down() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = scratch_store(&dir);

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

    assert_flat(&elapsed);
}

/// The association tables' insert goes through the same helper per row, so a
/// bulk attachment under a transaction has the same journal to avoid.
#[test]
fn successive_bulk_attachments_in_one_transaction_do_not_slow_down() {
    const ATTACH_BATCH: usize = 20_000;
    let dir = tempfile::tempdir().unwrap();
    let mut store = scratch_store(&dir);

    store.begin_transaction().unwrap();
    let mut elapsed = Vec::with_capacity(BATCHES);
    let mut ids = Vec::with_capacity(BATCHES * ATTACH_BATCH);
    for b in 0..BATCHES {
        let assocs = (0..ATTACH_BATCH)
            .map(|i| SupplementalAttributeAssociation {
                component_id: (b * ATTACH_BATCH + i) as i64 + 1,
                component_type: "Generator".into(),
                attribute_id: 7,
                attribute_type: "GeographicInfo".into(),
                id: None,
            })
            .collect();
        let t = Instant::now();
        ids.extend(
            store
                .add_supplemental_attribute_associations(assocs)
                .unwrap(),
        );
        elapsed.push(t.elapsed());
    }
    store.commit_transaction().unwrap();

    assert_eq!(
        ids,
        (1..=(BATCHES * ATTACH_BATCH) as i64).collect::<Vec<_>>()
    );
    assert_flat(&elapsed);
}

#[test]
fn list_keys_with_id_reports_the_ids_the_writes_handed_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = scratch_store(&dir);
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

    // The array-group listing carries the same id beside the hash.
    let groups = store.list_array_groups(ListFilter::new()).unwrap();
    assert_eq!(groups.len(), added.len());
    let mut by_owner: std::collections::HashMap<i64, i64> = groups
        .into_iter()
        .map(|(key, _hash, id)| (key.identity().owner_id, id.expect("a minted id")))
        .collect();
    for a in &added {
        assert_eq!(by_owner.remove(&a.key.identity().owner_id), Some(a.id));
    }
    assert!(by_owner.is_empty());
}
