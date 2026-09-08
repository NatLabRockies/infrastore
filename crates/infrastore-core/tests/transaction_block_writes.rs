//! Single adds inside a transaction write the layout a bulk add writes.
//!
//! Outside a transaction a single `add_time_series` drops its array into the
//! first free slot of a thousand-column growth pool. That pool is chunked one
//! timestamp row across every column, so filling one column is a
//! read-decompress-modify-recompress-write of every chunk in it — and the pool
//! is sized for a cohort that may never arrive. Inside a transaction none of
//! that is owed to the file yet: nothing the span wrote is durable until its
//! outermost commit, so the adds accumulate into a pending block per pool and
//! are written together with the same block writer `add_time_series_bulk` uses.
//!
//! What these tests pin is that the two really are the same file — same dataset
//! names, same widths, same chunk dims — and that "accepted but not yet
//! written" is invisible everywhere else: reads see it, dedup sees it, rollback
//! unwinds it, and an abandoned transaction leaves nothing behind.

use chrono::{DateTime, Duration, TimeZone, Utc};
use hdf5_metno as h5;
use infrastore_core::{
    AddRequest, ListFilter, OwnerCategory, ReadWindow, SingleTimeSeries, Store, TimeSeriesData,
    TypedArray, create_store, open_store,
};
use std::collections::BTreeMap;
use std::path::Path;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

/// A length-24 hourly `f64` series whose values are offset by `base`, so
/// distinct `base`s hash differently and equal ones share one stored array.
fn request(owner: i64, base: f64) -> AddRequest {
    let vals: Vec<f64> = (0..24).map(|i| base + i as f64).collect();
    AddRequest::new(
        owner,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
            t0(),
            Duration::hours(1),
            TypedArray::from_f64(vec![24], &vals),
            "load",
        )),
    )
}

/// Every packed dataset in the file, by name, with its shape and chunk dims.
///
/// The `_h` hash companions are skipped: they are a fixed function of the
/// dataset they belong to, so comparing them adds nothing and their names are
/// already covered by the parent's.
fn packed_layout(path: &Path) -> BTreeMap<String, (Vec<usize>, Option<Vec<usize>>)> {
    let file = h5::File::open(path).unwrap();
    let group = file.group("time_series/single").unwrap();
    group
        .member_names()
        .unwrap()
        .into_iter()
        .filter(|n| !n.ends_with("_h"))
        .map(|name| {
            let ds = group.dataset(&name).unwrap();
            let entry = (ds.shape(), ds.chunk());
            (name, entry)
        })
        .collect()
}

/// Add `owners` one at a time inside one transaction, then close the store.
fn add_singly_in_a_transaction(path: &Path, owners: std::ops::Range<i64>) {
    let mut store = create_store(Some(path), false).unwrap();
    store.begin_transaction().unwrap();
    for owner in owners {
        store.add(request(owner, owner as f64 * 100.0)).unwrap();
    }
    store.commit_transaction().unwrap();
    store.flush().unwrap();
}

// --- Layout equivalence -----------------------------------------------------

/// The whole point: N single adds inside one transaction and one bulk add of
/// the same N items produce the same datasets, byte for byte in structure.
///
/// Before this, the transaction loop produced a single 1000-column pool chunked
/// `(1, 1000)` — the growth pool — while the bulk add produced one N-column
/// dataset chunked `(1, N)`.
#[test]
fn single_adds_in_a_transaction_match_a_bulk_add_of_the_same_items() {
    let dir = tempfile::tempdir().unwrap();
    let looped = dir.path().join("looped.h5");
    let bulked = dir.path().join("bulked.h5");

    add_singly_in_a_transaction(&looped, 1..8);
    {
        let mut store = create_store(Some(&bulked), false).unwrap();
        store
            .add_time_series_bulk((1..8).map(|o| request(o, o as f64 * 100.0)).collect())
            .unwrap();
        store.flush().unwrap();
    }

    let looped_layout = packed_layout(&looped);
    assert_eq!(looped_layout, packed_layout(&bulked));
    assert_eq!(
        looped_layout,
        BTreeMap::from([(
            "sts_f64_s_24_PT1H".to_string(),
            (vec![24, 7], Some(vec![1, 7]))
        )])
    );

    // And the values survive the reshuffle in the order the adds arrived.
    let store = open_store(&looped, true).unwrap();
    for owner in 1..8 {
        assert_eq!(first_value(&store, owner), owner as f64 * 100.0);
    }
    assert!(store.verify_integrity().unwrap().ok());
}

/// The un-transactioned single add is untouched: it still claims a growth pool
/// sized for a cohort it hopes to see, and still fills one slot per call.
#[test]
fn a_single_add_outside_a_transaction_still_fills_a_growth_pool() {
    use infrastore_core::storage::common::DEFAULT_COLS_PER_DATASET;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grown.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        for owner in 1..4 {
            store.add(request(owner, owner as f64 * 100.0)).unwrap();
        }
        store.flush().unwrap();
    }
    assert_eq!(
        packed_layout(&path),
        BTreeMap::from([(
            "sts_f64_s_24_PT1H".to_string(),
            (
                vec![24, DEFAULT_COLS_PER_DATASET],
                Some(vec![1, DEFAULT_COLS_PER_DATASET])
            )
        )])
    );
}

/// A one-item `add_time_series_bulk` outside a transaction is the single add.
///
/// It used to size a dataset to the batch — one column, an eight-byte chunk —
/// once per call, which is what the Julia binding's `add_time_series!` does on
/// every series it adds: nineteen calls left nineteen datasets `…_PT1H__0`
/// through `…__18`. Inside a transaction it instead coalesces, which is the
/// better answer and the one the delegation cannot give.
#[test]
fn one_item_bulk_adds_fill_a_slot_outside_a_transaction_and_coalesce_inside_one() {
    let dir = tempfile::tempdir().unwrap();

    let loose = dir.path().join("loose.h5");
    {
        let mut store = create_store(Some(&loose), false).unwrap();
        for owner in 1..4 {
            store
                .add_time_series_bulk(vec![request(owner, owner as f64 * 100.0)])
                .unwrap();
        }
        store.flush().unwrap();
    }
    let layout = packed_layout(&loose);
    assert_eq!(layout.len(), 1, "one shared pool, not three datasets");
    assert_eq!(layout["sts_f64_s_24_PT1H"].0[0], 24);

    let spanned = dir.path().join("spanned.h5");
    {
        let mut store = create_store(Some(&spanned), false).unwrap();
        store.begin_transaction().unwrap();
        for owner in 1..4 {
            store
                .add_time_series_bulk(vec![request(owner, owner as f64 * 100.0)])
                .unwrap();
        }
        store.commit_transaction().unwrap();
        store.flush().unwrap();
    }
    assert_eq!(
        packed_layout(&spanned),
        BTreeMap::from([(
            "sts_f64_s_24_PT1H".to_string(),
            (vec![24, 3], Some(vec![1, 3]))
        )])
    );
}

// --- Reads inside the span --------------------------------------------------

fn first_value(store: &Store, owner: i64) -> f64 {
    let row = store
        .list_metadata(ListFilter::new().owner_id(owner))
        .unwrap()
        .pop()
        .expect("a row for this owner");
    store
        .read_by_id(row.id.unwrap(), ReadWindow::full())
        .unwrap()
        .as_single()
        .unwrap()
        .data
        .to_f64_vec()
        .unwrap()[0]
}

/// A buffered array is stored, not merely promised: every read serves it, and
/// an add made after the read still lands and still commits.
#[test]
fn a_deferred_add_reads_back_before_the_transaction_commits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rw.h5");
    let mut store = create_store(Some(&path), false).unwrap();

    store.begin_transaction().unwrap();
    let a = store.add(request(1, 100.0)).unwrap();
    let b = store.add(request(2, 200.0)).unwrap();

    // Single-id, bulk, and by-hash reads.
    assert_eq!(first_value(&store, 1), 100.0);
    let both = store.read_by_ids(&[a, b], ReadWindow::full()).unwrap();
    assert_eq!(both.len(), 2);
    assert_eq!(
        both[1].as_single().unwrap().data.to_f64_vec().unwrap()[0],
        200.0
    );
    let hash = store.get_metadata_by_id(a).unwrap().unwrap().data_hash;
    assert_eq!(
        store
            .get_array_by_hash(&hash)
            .unwrap()
            .to_f64_vec()
            .unwrap()[0],
        100.0
    );

    // A sliced read of a buffered array, cut out of the buffer.
    let sliced = store
        .read_by_id(b, ReadWindow::from(t0() + Duration::hours(2)).with_len(3))
        .unwrap();
    assert_eq!(
        sliced.as_single().unwrap().data.to_f64_vec().unwrap(),
        vec![202.0, 203.0, 204.0]
    );

    // The columnar reader, which reads a timestamp row across the cohort.
    let mut reader = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    store
        .static_read(&mut reader, t0() + Duration::hours(1))
        .unwrap();
    let mut seen: Vec<f64> = reader
        .groups()
        .iter()
        .flat_map(|g| g.values_to_vec::<f64>().unwrap())
        .collect();
    seen.sort_by(f64::total_cmp);
    assert_eq!(seen, vec![101.0, 201.0]);

    // A third add after all that still lands, and the commit takes.
    store.add(request(3, 300.0)).unwrap();
    store.commit_transaction().unwrap();
    store.flush().unwrap();

    for (owner, base) in [(1, 100.0), (2, 200.0), (3, 300.0)] {
        assert_eq!(first_value(&store, owner), base);
    }
    assert!(store.verify_integrity().unwrap().ok());
}

/// Content addressing is unchanged by buffering: the same array added twice in
/// one span is stored once, under two ids, at one location.
#[test]
fn a_repeated_array_is_buffered_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dedup.h5");
    let mut store = create_store(Some(&path), false).unwrap();

    store.begin_transaction().unwrap();
    let first = store.add(request(1, 100.0)).unwrap();
    let second = store.add(request(2, 100.0)).unwrap();
    assert_ne!(first, second, "two associations");
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);

    let (ha, hb) = (
        store.get_metadata_by_id(first).unwrap().unwrap().data_hash,
        store.get_metadata_by_id(second).unwrap().unwrap().data_hash,
    );
    assert_eq!(ha, hb);
    store.commit_transaction().unwrap();
    store.flush().unwrap();

    assert_eq!(
        store.locate_array(&ha).unwrap(),
        store.locate_array(&hb).unwrap()
    );
    assert_eq!(
        packed_layout(&path),
        BTreeMap::from([(
            "sts_f64_s_24_PT1H".to_string(),
            (vec![24, 1], Some(vec![1, 1]))
        )])
    );
}

// --- Unwinding --------------------------------------------------------------

/// An outermost rollback of a span that only ever buffered leaves the file as
/// it found it — with no dataset at all, because nothing was ever written.
#[test]
fn rollback_of_a_buffered_span_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rolled.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        store.begin_transaction().unwrap();
        for owner in 1..5 {
            store.add(request(owner, owner as f64 * 100.0)).unwrap();
        }
        store.rollback_transaction().unwrap();
        assert_eq!(store.list_metadata(ListFilter::new()).unwrap().len(), 0);
        assert!(store.verify_integrity().unwrap().ok());
        store.flush().unwrap();
    }
    assert!(
        packed_layout(&path).is_empty(),
        "a rolled-back span that never reached the file leaves no pool behind"
    );
}

/// An inner rollback takes only its own level's buffered arrays out of the
/// block; the outer commit writes the survivors as one dataset.
#[test]
fn an_inner_rollback_drops_only_its_own_arrays_from_the_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested.h5");
    let mut store = create_store(Some(&path), false).unwrap();

    store.begin_transaction().unwrap();
    store.add(request(1, 100.0)).unwrap();
    store.add(request(2, 200.0)).unwrap();

    store.begin_transaction().unwrap();
    store.add(request(3, 300.0)).unwrap();
    store.rollback_transaction().unwrap();

    store.add(request(4, 400.0)).unwrap();
    store.commit_transaction().unwrap();
    store.flush().unwrap();

    // Three survivors in one three-column dataset: the rolled-back array left
    // the block before it was ever written, so it costs no column.
    assert_eq!(
        packed_layout(&path),
        BTreeMap::from([(
            "sts_f64_s_24_PT1H".to_string(),
            (vec![24, 3], Some(vec![1, 3]))
        )])
    );
    for (owner, base) in [(1, 100.0), (2, 200.0), (4, 400.0)] {
        assert_eq!(first_value(&store, owner), base);
    }
    assert_eq!(store.list_metadata(ListFilter::new()).unwrap().len(), 3);
    assert!(store.verify_integrity().unwrap().ok());
}

/// A transaction abandoned by dropping the store — the shape a killed process
/// leaves — no longer strands its arrays in the file. They were never written,
/// so there is nothing to strand: `compact` used to be the only way to reclaim
/// the columns such a span had already filled.
#[test]
fn an_abandoned_transaction_leaves_no_orphan_arrays() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abandoned.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        store.add(request(1, 100.0)).unwrap();
        store.flush().unwrap();
    }
    let before = packed_layout(&path);
    {
        let mut store = open_store(&path, false).unwrap();
        store.begin_transaction().unwrap();
        for owner in 2..6 {
            store.add(request(owner, owner as f64 * 100.0)).unwrap();
        }
        // No commit, no rollback, no flush: just go away.
    }
    assert_eq!(
        packed_layout(&path),
        before,
        "the abandoned span wrote nothing"
    );
    let store = open_store(&path, true).unwrap();
    assert_eq!(store.list_metadata(ListFilter::new()).unwrap().len(), 1);
    assert_eq!(first_value(&store, 1), 100.0);
    assert!(store.verify_integrity().unwrap().ok());
}

// --- Spilling ---------------------------------------------------------------

/// A pending block is not unbounded: it is written out once it reaches the
/// width the block writer itself spills at, so the memory one pool holds is the
/// memory the equivalent bulk add allocates — and the datasets are the ones
/// that bulk add would have produced.
///
/// The cap is a per-chunk byte budget, so a wide element shape makes it small
/// enough to cross in a test: `f64` elements of `[1024]` are 8 KiB each, which
/// caps a 1 MiB timestamp-row chunk at 128 columns.
#[test]
fn a_pending_block_spills_at_the_width_the_block_writer_spills_at() {
    const ELEMENTS: usize = 1024;
    const CAP: usize = (1 << 20) / (ELEMENTS * 8);
    const TOTAL: usize = CAP + 3;

    fn wide(owner: i64) -> AddRequest {
        let vals: Vec<f64> = (0..2 * ELEMENTS).map(|i| owner as f64 + i as f64).collect();
        AddRequest::new(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                t0(),
                Duration::hours(1),
                TypedArray::from_f64(vec![2, ELEMENTS], &vals),
                "load",
            )),
        )
    }

    let dir = tempfile::tempdir().unwrap();
    let looped = dir.path().join("looped.h5");
    {
        let mut store = create_store(Some(&looped), false).unwrap();
        store.begin_transaction().unwrap();
        for owner in 1..=TOTAL as i64 {
            store.add(wide(owner)).unwrap();
        }
        store.commit_transaction().unwrap();
        store.flush().unwrap();
    }

    let base = format!("sts_f64_{ELEMENTS}_2_PT1H");
    assert_eq!(
        packed_layout(&looped),
        BTreeMap::from([
            (
                base.clone(),
                (vec![2, CAP, ELEMENTS], Some(vec![1, CAP, ELEMENTS]))
            ),
            (
                format!("{base}__1"),
                (vec![2, 3, ELEMENTS], Some(vec![1, 3, ELEMENTS]))
            ),
        ]),
        "a full-width block mid-span, the remainder at commit"
    );

    // Which is exactly how the bulk path splits the same batch.
    let bulked = dir.path().join("bulked.h5");
    {
        let mut store = create_store(Some(&bulked), false).unwrap();
        store
            .add_time_series_bulk((1..=TOTAL as i64).map(wide).collect())
            .unwrap();
        store.flush().unwrap();
    }
    assert_eq!(packed_layout(&looped), packed_layout(&bulked));
}
