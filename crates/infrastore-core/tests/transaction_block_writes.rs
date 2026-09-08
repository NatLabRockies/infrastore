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
//! unwinds it, and an abandoned transaction leaves behind nothing it was still
//! holding.
//!
//! Two edges bound the buffering, and both are pinned here too. A block of one
//! is not a block: it fills a growth-pool slot, because sizing a dataset to one
//! column is the mistake `add_time_series_bulk` already refuses for a batch of
//! one. And the memory an open span holds is capped twice — per pool at the
//! width the block writer spills a batch at, and across every pool at a byte
//! budget — so a long enough span writes blocks out early instead of growing
//! without limit. The per-pool cap is the chunk budget and not the growth
//! pool's thousand columns on purpose: a bulk add issued inside a transaction is
//! buffered the same way, and a thousand-column cap cut a wide one into ten
//! times the datasets it writes outside a transaction, which a columnar read
//! then pays for chunk by chunk.
//!
//! Chunk shapes below are `(rows, cols)` with `rows > 1` wherever one timestamp
//! row would fall under `MIN_CHUNK_BYTES`. At 24 f64 steps that is every layout
//! here -- a thousand columns is an 8,000-byte row -- so the small ones take
//! every row they have and the thousand-column pool takes five. Only a very
//! wide element shape clears the floor on one row: the `[1024]` elements in the
//! spill test are 8 KiB each, so 128 of them are a 1 MiB row.

use chrono::{DateTime, Duration, TimeZone, Utc};
use hdf5_metno as h5;
use infrastore_core::{
    AddRequest, ListFilter, NonSequentialTimeSeries, OwnerCategory, PersistentTimeSeries,
    ReadWindow, SingleTimeSeries, Store, TimeSeriesData, TypedArray, create_store, open_store,
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

/// The chunk rows `MIN_CHUNK_BYTES` gives a `cols`-wide `f64` dataset of
/// `length` rows, mirroring `storage::common::packed_chunk_rows` so a test can
/// name a chunk without hard-coding the floor.
fn rows_per_chunk(cols: usize, length: usize) -> usize {
    let row = cols * 8;
    if row >= 32 * 1024 {
        1
    } else {
        (32 * 1024usize).div_ceil(row).clamp(1, length)
    }
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
            (vec![24, 7], Some(vec![24, 7]))
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
                Some(vec![5, DEFAULT_COLS_PER_DATASET])
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
            (vec![24, 3], Some(vec![24, 3]))
        )])
    );
}

/// The same, through the buffered guard: a `BulkAdd` holding one request is the
/// single add too.
///
/// `BulkAdd::commit` does not go through `add_time_series_bulk`, so the one-item
/// test has to live below both of them — in `flush_bulk_add` — or a Rust caller
/// buffering exactly one request still claims a one-column dataset per commit.
#[test]
fn a_one_item_bulk_add_guard_fills_a_slot_outside_a_transaction() {
    use infrastore_core::storage::common::DEFAULT_COLS_PER_DATASET;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("guarded.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        for owner in 1..4 {
            let mut batch = store.bulk_add();
            batch.push(request(owner, owner as f64 * 100.0));
            batch.commit().unwrap();
        }
        store.flush().unwrap();
    }
    let layout = packed_layout(&path);
    assert_eq!(layout.len(), 1, "one shared pool, not three datasets");
    assert_eq!(layout["sts_f64_s_24_PT1H"].0[0], 24);
    assert_eq!(
        layout["sts_f64_s_24_PT1H"].0[1], DEFAULT_COLS_PER_DATASET,
        "the growth pool's default width, not a block of one"
    );
}

/// A transaction spanning a *single* packed add fills a growth-pool slot, the
/// way that add would outside one — it does not size a dataset to a block of
/// one.
///
/// This is the shape the feature is most often used in: "several operations
/// atomic together" is usually a removal and a replacement, or one add beside
/// some catalog work, not a bulk ingest. Sizing a dataset to it would chunk
/// `(1, 1)`, giving a scalar `f64` series an eight-byte chunk and one chunk per
/// timestep — the same per-chunk overhead that made nineteen one-item bulk adds
/// write a 36 MB file, which is what the delegation above exists to avoid.
/// Nineteen 30,500-step series added in nineteen separate transactions measured
/// 36 MB written that way against 5.9 MB through the pool.
///
/// From two columns up the block is what a bulk add of the same items writes,
/// and stays that way — that is the invariant this file's first test pins.
#[test]
fn a_transaction_around_one_add_fills_a_slot_rather_than_sizing_a_dataset() {
    use infrastore_core::storage::common::DEFAULT_COLS_PER_DATASET;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("one_at_a_time.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        for owner in 1..6 {
            store.begin_transaction().unwrap();
            store.add(request(owner, owner as f64 * 100.0)).unwrap();
            store.commit_transaction().unwrap();
        }
        store.flush().unwrap();
    }
    assert_eq!(
        packed_layout(&path),
        BTreeMap::from([(
            "sts_f64_s_24_PT1H".to_string(),
            (
                vec![24, DEFAULT_COLS_PER_DATASET],
                Some(vec![5, DEFAULT_COLS_PER_DATASET])
            )
        )]),
        "five one-add transactions share one pool, not five datasets"
    );

    let store = open_store(&path, true).unwrap();
    assert_eq!(store.list_metadata(ListFilter::new()).unwrap().len(), 5);
    for owner in 1..6 {
        assert_eq!(first_value(&store, owner), owner as f64 * 100.0);
    }
    assert!(store.verify_integrity().unwrap().ok());
}

/// The twelve-point event timeline every irregular series below shares, so they
/// all land in one [`PackGroup::Irregular`] cohort.
fn axis() -> Vec<DateTime<Utc>> {
    (0..12).map(|i| t0() + Duration::hours(3 * i)).collect()
}

/// One `NonSequentialTimeSeries` on that axis, distinct per `base`.
fn irregular(owner: i64, base: f64) -> AddRequest {
    let vals: Vec<f64> = (0..12).map(|i| base + i as f64).collect();
    AddRequest::new(
        owner,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::NonSequentialTimeSeries(
            NonSequentialTimeSeries::new(axis(), TypedArray::from_f64(vec![12], &vals), "spot")
                .unwrap(),
        ),
    )
}

/// A `PersistentTimeSeries` on the same axis. It pools with the above: a
/// `PackGroup` is keyed by the time axis, never by the series type.
fn persistent(owner: i64, base: f64) -> AddRequest {
    let vals: Vec<f64> = (0..12).map(|i| base + i as f64).collect();
    AddRequest::new(
        owner,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::PersistentTimeSeries(
            PersistentTimeSeries::new(axis(), TypedArray::from_f64(vec![12], &vals), "sp").unwrap(),
        ),
    )
}

/// [`first_value`] for an irregular row, which reads back as its own type.
fn first_irregular_value(store: &Store, owner: i64) -> f64 {
    let row = store
        .list_metadata(ListFilter::new().owner_id(owner))
        .unwrap()
        .pop()
        .expect("a row for this owner");
    store
        .read_by_id(row.id.unwrap(), ReadWindow::full())
        .unwrap()
        .as_non_sequential()
        .expect("a NonSequentialTimeSeries")
        .data
        .to_f64_vec()
        .unwrap()[0]
}

/// `packed_layout` keyed by dataset *kind* rather than name, because a
/// standalone dataset is named for its content hash. `nsts` is a pooled
/// irregular cohort, `arr` a standalone array.
fn kinds(path: &Path) -> BTreeMap<String, Vec<Vec<usize>>> {
    let mut out: BTreeMap<String, Vec<Vec<usize>>> = BTreeMap::new();
    for (name, (shape, _)) in packed_layout(path) {
        let kind = name.split('_').next().unwrap().to_string();
        out.entry(kind).or_default().push(shape);
    }
    out
}

// --- Irregular cohorts ------------------------------------------------------

/// A cohort of irregular series added one at a time inside a span pools into
/// one dataset, exactly as the bulk add of the same requests does.
///
/// Packing an irregular series is a bet that its axis will be shared, and
/// `resolve_irregular_layouts` settles that bet before the write from the
/// requests it can see. Inside a span it cannot see them: each add is one
/// request against a file holding no pool for the axis -- and because a
/// standalone array never *becomes* a pool, the next add saw exactly the same
/// thing. Six series on one timeline came out as six standalone datasets where
/// the bulk add of them writes one `(12, 6)` cohort. The bet is now settled
/// where the answer is known, in the block.
#[test]
fn irregular_singles_in_a_span_pool_like_a_bulk_add_of_them() {
    let dir = tempfile::tempdir().unwrap();
    let looped = dir.path().join("looped.h5");
    let bulked = dir.path().join("bulked.h5");
    {
        let mut store = create_store(Some(&looped), false).unwrap();
        store.begin_transaction().unwrap();
        for owner in 1..7 {
            store.add(irregular(owner, owner as f64 * 100.0)).unwrap();
        }
        store.commit_transaction().unwrap();
        store.flush().unwrap();
    }
    {
        let mut store = create_store(Some(&bulked), false).unwrap();
        store
            .add_time_series_bulk((1..7).map(|o| irregular(o, o as f64 * 100.0)).collect())
            .unwrap();
        store.flush().unwrap();
    }

    let layout = kinds(&looped);
    assert_eq!(
        layout,
        kinds(&bulked),
        "the span writes the bulk add's file"
    );
    assert_eq!(layout.get("nsts").map(Vec::len), Some(1), "{layout:?}");
    assert_eq!(layout["nsts"][0], vec![12, 6]);
    assert!(!layout.contains_key("arr"), "nothing left standalone");

    let store = open_store(&looped, true).unwrap();
    for owner in 1..7 {
        assert_eq!(first_irregular_value(&store, owner), owner as f64 * 100.0);
    }
    assert!(store.verify_integrity().unwrap().ok());
}

/// Both irregular types pool together when they share an axis, which is what
/// keying a `PackGroup` by the time axis rather than the series type means --
/// and buffering must not quietly change it.
#[test]
fn the_two_irregular_types_share_one_buffered_cohort() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        store.begin_transaction().unwrap();
        store.add(irregular(1, 100.0)).unwrap();
        store.add(persistent(2, 200.0)).unwrap();
        store.add(irregular(3, 300.0)).unwrap();
        store.commit_transaction().unwrap();
        store.flush().unwrap();
    }
    let layout = kinds(&path);
    assert_eq!(layout.get("nsts").map(Vec::len), Some(1), "{layout:?}");
    assert_eq!(layout["nsts"][0], vec![12, 3]);
}

/// A span holding a *single* irregular series still writes it standalone: the
/// bet is settled the same way, just with the answer "no cohort".
///
/// This is the irregular half of the block-of-one rule. Its regular sibling
/// fills a growth-pool slot, because a `SingleTimeSeries` pool is shared by
/// every series on the resolution; an irregular pool is shared only by the
/// series on that exact axis, so a cohort of one is a dataset spread across
/// `length` chunks for no reason.
#[test]
fn a_lone_irregular_series_in_a_span_stays_standalone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lone.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        store.begin_transaction().unwrap();
        store.add(irregular(1, 100.0)).unwrap();
        store.commit_transaction().unwrap();
        store.flush().unwrap();
    }
    let layout = kinds(&path);
    assert_eq!(layout.get("arr").map(Vec::len), Some(1), "{layout:?}");
    assert!(!layout.contains_key("nsts"), "no cohort of one");
}

/// ...unless the file already holds a pool for that axis, which is the other
/// half of the bet and outlives the span that made it.
#[test]
fn a_lone_irregular_series_joins_a_pool_the_file_already_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("join.h5");
    {
        let mut store = create_store(Some(&path), false).unwrap();
        store
            .add_time_series_bulk(vec![irregular(1, 100.0), irregular(2, 200.0)])
            .unwrap();
        store.flush().unwrap();
    }
    assert_eq!(kinds(&path)["nsts"], vec![vec![12, 2]]);
    {
        let mut store = open_store(&path, false).unwrap();
        store.begin_transaction().unwrap();
        store.add(irregular(3, 300.0)).unwrap();
        store.commit_transaction().unwrap();
        store.flush().unwrap();
    }
    let layout = kinds(&path);
    assert!(!layout.contains_key("arr"), "joined the pool: {layout:?}");
    assert_eq!(layout["nsts"].len(), 2, "a sibling of the same pool");

    let store = open_store(&path, true).unwrap();
    assert_eq!(first_irregular_value(&store, 3), 300.0);
    assert!(store.verify_integrity().unwrap().ok());
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
    // One distinct array is a block of one, which fills a growth-pool slot
    // rather than claiming a one-column dataset — see
    // `a_transaction_holding_one_array_fills_a_slot_rather_than_sizing_a_dataset`.
    assert_eq!(
        packed_layout(&path),
        BTreeMap::from([(
            "sts_f64_s_24_PT1H".to_string(),
            (vec![24, 1000], Some(vec![5, 1000]))
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
            (vec![24, 3], Some(vec![24, 3]))
        )])
    );
    for (owner, base) in [(1, 100.0), (2, 200.0), (4, 400.0)] {
        assert_eq!(first_value(&store, owner), base);
    }
    assert_eq!(store.list_metadata(ListFilter::new()).unwrap().len(), 3);
    assert!(store.verify_integrity().unwrap().ok());
}

/// A transaction abandoned by dropping the store — the shape a killed process
/// leaves — no longer strands the arrays it was still holding. They were never
/// written, so there is nothing to strand: `compact` used to be the only way to
/// reclaim the columns such a span had already filled.
///
/// The guarantee is exactly that, and no wider: it covers what is *still
/// buffered* when the store goes away. A span that spilled a block first — by
/// filling a pool to its width cap, crossing the byte budget, flushing, or
/// asking for a physical location — has written those arrays, and an
/// abandonment after that leaves them behind for `compact`, as every add
/// already did before any of this.
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
/// width its pool spills at, so the datasets are the ones the equivalent bulk
/// add would have produced.
///
/// One of the two ceilings on that width is the per-chunk byte budget, which a
/// wide element shape makes small enough to cross in a test: `f64` elements of
/// `[1024]` are 8 KiB each, which caps a 1 MiB timestamp-row chunk at 128
/// columns. The other is the global byte budget, exercised in the backend's own
/// tests. What is *not* a ceiling is the growth pool's thousand columns: a
/// scalar `f64` span of 1,002 stays one block, exactly as the bulk add of the
/// same 1,002 writes one dataset.
#[test]
fn a_scalar_span_wider_than_the_growth_pool_stays_one_block() {
    use infrastore_core::storage::common::DEFAULT_COLS_PER_DATASET;

    const TOTAL: i64 = DEFAULT_COLS_PER_DATASET as i64 + 2;

    let dir = tempfile::tempdir().unwrap();
    let looped = dir.path().join("wide_span.h5");
    add_singly_in_a_transaction(&looped, 1..TOTAL + 1);

    let expected = BTreeMap::from([(
        "sts_f64_s_24_PT1H".to_string(),
        (
            vec![24, TOTAL as usize],
            Some(vec![rows_per_chunk(TOTAL as usize, 24), TOTAL as usize]),
        ),
    )]);
    assert_eq!(
        packed_layout(&looped),
        expected,
        "one block wider than the growth pool, not a spill at a thousand"
    );

    let bulked = dir.path().join("bulked.h5");
    {
        let mut store = create_store(Some(&bulked), false).unwrap();
        store
            .add_time_series_bulk(
                (1..TOTAL + 1)
                    .map(|o| request(o, o as f64 * 100.0))
                    .collect(),
            )
            .unwrap();
        store.flush().unwrap();
    }
    assert_eq!(packed_layout(&bulked), expected);

    let store = open_store(&looped, true).unwrap();
    assert_eq!(
        store.list_metadata(ListFilter::new()).unwrap().len(),
        TOTAL as usize
    );
    for owner in [1, DEFAULT_COLS_PER_DATASET as i64, TOTAL] {
        assert_eq!(first_value(&store, owner), owner as f64 * 100.0);
    }
    assert!(store.verify_integrity().unwrap().ok());
}

/// A bulk add issued *inside* a transaction is buffered like the single adds
/// are, and must come out as the dataset it writes outside one.
///
/// This is the shape the Julia binding's transaction takes — it stages its
/// adds client-side and commits them as bulk adds of ten thousand inside an
/// open transaction — and it is where a growth-pool-width cap on the buffer
/// was first felt: a hundred-thousand-series store came out as a hundred
/// datasets of a thousand columns instead of ten of ten thousand, and every
/// per-timestep read across it paid the tenfold chunk count.
#[test]
fn a_bulk_add_inside_a_transaction_writes_the_dataset_it_writes_outside_one() {
    const TOTAL: i64 = 1_200;
    let items = || {
        (1..=TOTAL)
            .map(|o| request(o, o as f64 * 100.0))
            .collect::<Vec<_>>()
    };

    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside.h5");
    {
        let mut store = create_store(Some(&outside), false).unwrap();
        store.add_time_series_bulk(items()).unwrap();
        store.flush().unwrap();
    }
    let inside = dir.path().join("inside.h5");
    {
        let mut store = create_store(Some(&inside), false).unwrap();
        store.begin_transaction().unwrap();
        store.add_time_series_bulk(items()).unwrap();
        // The binding flushes between its batches; a flush inside the span
        // writes the block out as it stands and must not narrow it.
        store.flush().unwrap();
        store.commit_transaction().unwrap();
        store.flush().unwrap();
    }

    let layout = packed_layout(&inside);
    assert_eq!(layout, packed_layout(&outside));
    assert_eq!(
        layout,
        BTreeMap::from([(
            "sts_f64_s_24_PT1H".to_string(),
            (
                vec![24, TOTAL as usize],
                Some(vec![rows_per_chunk(TOTAL as usize, 24), TOTAL as usize]),
            ),
        )]),
        "one batch-wide dataset, not growth-pool-sized pieces"
    );

    let store = open_store(&inside, true).unwrap();
    for owner in [1, 1_000, 1_001, TOTAL] {
        assert_eq!(first_value(&store, owner), owner as f64 * 100.0);
    }
    assert!(store.verify_integrity().unwrap().ok());
}

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
                (vec![2, 3, ELEMENTS], Some(vec![2, 3, ELEMENTS]))
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
