//! Cross-operation transactions: `begin_transaction` / `commit_transaction` /
//! `rollback_transaction`.
//!
//! The interesting cases are the ones a per-operation transaction cannot express.
//! A single `add_time_series_bulk` was already all-or-nothing, so what these
//! tests pin down is the *span*: several operations rolling back together, and —
//! the capability that does not exist outside a transaction — a **removal** being
//! undone. That one works because the array store is content-addressed and can
//! therefore be made append-only for the transaction's duration: frees are
//! deferred to the outermost commit, so a rollback restores catalog rows whose
//! data is still present.
//!
//! Both backends are exercised where the distinction matters: `MemoryBackend`
//! drops array bytes on `remove_array`, while the HDF5 backend tombstones and
//! leaves the variable until `compact`. Deferring frees is what keeps the two
//! behaving identically under rollback.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Features, ListFilter, OwnerCategory, SingleTimeSeries, Store, TimeSeriesData, TimeSeriesKey,
    TypedArray, create_store, open_store,
};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

/// A length-8 hourly series whose values are offset by `base`, so distinct
/// `base`s hash differently and equal ones share an array.
fn series(base: f64) -> SingleTimeSeries {
    let vals: Vec<f64> = (0..8).map(|i| base + i as f64).collect();
    SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![8], &vals),
        "load",
    )
}

fn add(store: &mut Store, owner: i64, base: f64) -> TimeSeriesKey {
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(base)),
            Features::new(),
            None,
        )
        .unwrap()
}

fn count(store: &Store) -> usize {
    store.list_keys(ListFilter::new()).unwrap().len()
}

/// Run `body` against a fresh store on each backend. Transactions touch both
/// halves of the artifact, and the two backends free arrays differently, so
/// every guarantee here is asserted twice.
fn each_backend(body: impl Fn(&mut Store, &str)) {
    {
        let mut store = create_store(None, true).unwrap();
        body(&mut store, "memory");
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.h5");
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        body(&mut store, "disk");
    }
}

// --- Adds -------------------------------------------------------------------

#[test]
fn rollback_undoes_adds_across_several_operations() {
    each_backend(|store, backend| {
        add(store, 1, 0.0);

        store.begin_transaction().unwrap();
        add(store, 2, 100.0);
        add(store, 3, 200.0);
        assert_eq!(count(store), 3, "{backend}: writes visible inside the txn");
        store.rollback_transaction().unwrap();

        assert_eq!(count(store), 1, "{backend}: only the pre-txn add survives");
        // The rolled-back arrays are unreferenced and must not linger in the
        // catalog's distinct-array tally.
        assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");
    });
}

#[test]
fn commit_makes_adds_durable() {
    each_backend(|store, backend| {
        store.begin_transaction().unwrap();
        add(store, 1, 0.0);
        add(store, 2, 100.0);
        store.commit_transaction().unwrap();

        assert_eq!(count(store), 2, "{backend}");
        assert!(!store.in_transaction(), "{backend}");
    });
}

/// A rollback must not remove an array that predates the transaction just
/// because an add inside re-referenced it. Content addressing means the second
/// add wrote nothing, so there is nothing to undo.
#[test]
fn rollback_keeps_a_shared_array_that_predates_the_transaction() {
    each_backend(|store, backend| {
        let k1 = add(store, 1, 0.0);
        let hash = store.get_metadata(k1.identity()).unwrap().data_hash;

        store.begin_transaction().unwrap();
        add(store, 2, 0.0); // identical data -> same hash, no new array written
        assert_eq!(store.count_array_references(&hash).unwrap(), (2, 0));
        store.rollback_transaction().unwrap();

        assert_eq!(store.count_array_references(&hash).unwrap(), (1, 0));
        // The surviving association must still be readable: the shared array's
        // bytes were never ours to remove.
        assert!(
            store.get_time_series(k1.identity(), None).is_ok(),
            "{backend}"
        );
    });
}

// --- Removals: the capability that does not exist outside a transaction -----

#[test]
fn rollback_restores_removed_series_and_their_data() {
    each_backend(|store, backend| {
        let k1 = add(store, 1, 0.0);
        let k2 = add(store, 2, 100.0);
        assert_eq!(store.num_distinct_arrays().unwrap(), 2);

        store.begin_transaction().unwrap();
        store.remove_time_series(k1.identity()).unwrap();
        store.remove_time_series(k2.identity()).unwrap();
        assert_eq!(
            count(store),
            0,
            "{backend}: removals visible inside the txn"
        );
        store.rollback_transaction().unwrap();

        assert_eq!(count(store), 2, "{backend}: both associations restored");
        assert_eq!(store.num_distinct_arrays().unwrap(), 2, "{backend}");
        // The point of deferring the free: the arrays are still readable, not
        // just the catalog rows.
        let v1 = store.get_time_series(k1.identity(), None).unwrap();
        let v2 = store.get_time_series(k2.identity(), None).unwrap();
        assert_eq!(
            v1.as_single().unwrap().data.to_f64_vec().unwrap()[0],
            0.0,
            "{backend}"
        );
        assert_eq!(
            v2.as_single().unwrap().data.to_f64_vec().unwrap()[0],
            100.0,
            "{backend}"
        );
    });
}

#[test]
fn commit_applies_deferred_frees() {
    each_backend(|store, backend| {
        let k1 = add(store, 1, 0.0);
        add(store, 2, 100.0);

        store.begin_transaction().unwrap();
        store.remove_time_series(k1.identity()).unwrap();
        store.commit_transaction().unwrap();

        assert_eq!(count(store), 1, "{backend}");
        // The removal is durable and its array actually reclaimed, exactly as an
        // untransacted removal would have left it.
        assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");
    });
}

/// A hash removed and then re-added inside the same transaction must survive the
/// commit: the deferred free is decided against the catalog as the commit will
/// leave it, not against the state at removal time.
#[test]
fn deferred_free_is_skipped_when_the_array_is_referenced_again() {
    each_backend(|store, backend| {
        let k1 = add(store, 1, 0.0);
        let hash = store.get_metadata(k1.identity()).unwrap().data_hash;

        store.begin_transaction().unwrap();
        store.remove_time_series(k1.identity()).unwrap();
        let k2 = add(store, 2, 0.0); // same content, new owner
        store.commit_transaction().unwrap();

        assert_eq!(store.count_array_references(&hash).unwrap(), (1, 0));
        let restored = store.get_time_series(k2.identity(), None).unwrap();
        assert_eq!(
            restored.as_single().unwrap().data.to_f64_vec().unwrap()[0],
            0.0,
            "{backend}"
        );
    });
}

/// Mixed adds and removals unwind together — the case the per-operation
/// transactions could not express, and the reason a client no longer needs a
/// compensating-removal undo log.
#[test]
fn rollback_undoes_a_mixed_add_and_remove_span() {
    each_backend(|store, backend| {
        let k1 = add(store, 1, 0.0);

        store.begin_transaction().unwrap();
        add(store, 2, 100.0);
        store.remove_time_series(k1.identity()).unwrap();
        assert_eq!(count(store), 1, "{backend}: one added, one removed");
        store.rollback_transaction().unwrap();

        assert_eq!(count(store), 1, "{backend}");
        // Specifically the *original* series, not the added one.
        let restored = store.get_time_series(k1.identity(), None).unwrap();
        assert_eq!(
            restored.as_single().unwrap().data.to_f64_vec().unwrap()[0],
            0.0,
            "{backend}"
        );
        assert_eq!(store.num_distinct_arrays().unwrap(), 1, "{backend}");
    });
}

// --- Bulk adds compose with transactions ------------------------------------

/// A transaction must not cost the block-sized writes and feature-set dedup that
/// `bulk_add` provides; the two compose, with the transaction spanning several
/// bulk commits.
#[test]
fn rollback_undoes_several_bulk_commits() {
    each_backend(|store, backend| {
        store.begin_transaction().unwrap();
        for chunk in 0..3 {
            let mut bulk = store.bulk_add();
            for i in 0..4 {
                bulk.add(
                    chunk * 4 + i,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(series((chunk * 4 + i) as f64 * 10.0)),
                    Features::new(),
                    None,
                );
            }
            bulk.commit().unwrap();
        }
        assert_eq!(count(store), 12, "{backend}");
        store.rollback_transaction().unwrap();

        assert_eq!(count(store), 0, "{backend}: all three batches undone");
        assert_eq!(store.num_distinct_arrays().unwrap(), 0, "{backend}");
    });
}

// --- Nesting ----------------------------------------------------------------

#[test]
fn inner_rollback_leaves_the_outer_transaction_open() {
    each_backend(|store, backend| {
        store.begin_transaction().unwrap();
        add(store, 1, 0.0);

        store.begin_transaction().unwrap();
        add(store, 2, 100.0);
        store.rollback_transaction().unwrap();

        assert!(store.in_transaction(), "{backend}: outer still open");
        assert_eq!(count(store), 1, "{backend}: only the inner add is undone");
        store.commit_transaction().unwrap();

        assert_eq!(count(store), 1, "{backend}");
        assert!(!store.in_transaction(), "{backend}");
    });
}

#[test]
fn outer_rollback_discards_committed_inner_transactions() {
    each_backend(|store, backend| {
        store.begin_transaction().unwrap();
        store.begin_transaction().unwrap();
        add(store, 1, 0.0);
        store.commit_transaction().unwrap();
        assert!(store.in_transaction(), "{backend}: outer still open");

        store.rollback_transaction().unwrap();
        assert_eq!(count(store), 0, "{backend}: inner commit was not durable");
        assert_eq!(store.num_distinct_arrays().unwrap(), 0, "{backend}");
    });
}

// --- Interaction with existing guards ---------------------------------------

/// A failing operation inside a transaction unwinds only itself; the transaction
/// stays open and usable, and its earlier work is intact.
#[test]
fn a_failed_operation_does_not_abort_the_transaction() {
    each_backend(|store, backend| {
        store.begin_transaction().unwrap();
        let k1 = add(store, 1, 0.0);

        // Duplicate key: rejected by the per-operation savepoint.
        let dup = store.add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(0.0)),
            Features::new(),
            None,
        );
        assert!(dup.is_err(), "{backend}");

        assert!(store.in_transaction(), "{backend}");
        assert_eq!(count(store), 1, "{backend}: the good add survived");
        store.commit_transaction().unwrap();
        assert!(
            store.get_time_series(k1.identity(), None).is_ok(),
            "{backend}"
        );
    });
}

/// The DST guard is evaluated on post-removal state within one operation, so a
/// transaction does not change it: removing a backing `SingleTimeSeries` alone
/// still fails, and the transaction survives the failure.
#[test]
fn dst_guard_still_applies_inside_a_transaction() {
    let mut store = create_store(None, true).unwrap();
    let k1 = add(&mut store, 1, 0.0);
    store
        .transform_single_time_series(Duration::hours(2), Duration::hours(1), None, None)
        .unwrap();

    store.begin_transaction().unwrap();
    let err = store.remove_time_series(k1.identity());
    assert!(err.is_err(), "removing the backing STS alone must fail");
    assert!(store.in_transaction());
    store.rollback_transaction().unwrap();

    assert!(store.get_time_series(k1.identity(), None).is_ok());
}

#[test]
fn compact_is_rejected_while_a_transaction_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let mut store = create_store(Some(path.as_path()), false).unwrap();
    store.begin_transaction().unwrap();
    assert!(store.compact().is_err());
    store.rollback_transaction().unwrap();
    assert!(store.compact().is_ok());
}

#[test]
fn commit_or_rollback_without_a_transaction_is_an_error() {
    let mut store = create_store(None, true).unwrap();
    assert!(store.commit_transaction().is_err());
    assert!(store.rollback_transaction().is_err());
    assert!(!store.in_transaction());
}

#[test]
fn a_read_only_store_cannot_begin_a_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        add(&mut store, 1, 0.0);
        store.flush().unwrap();
    }
    let mut store = open_store(path.as_path(), true).unwrap();
    assert!(store.begin_transaction().is_err());
}

// --- Durability across a reopen ---------------------------------------------

/// Rollback must leave the on-disk artifact as it was, not merely the in-memory
/// view: reopening the store shows the pre-transaction state.
#[test]
fn rollback_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let k1 = add(&mut store, 1, 0.0);

        store.begin_transaction().unwrap();
        add(&mut store, 2, 100.0);
        store.remove_time_series(k1.identity()).unwrap();
        store.rollback_transaction().unwrap();
        store.flush().unwrap();
    }
    let store = open_store(path.as_path(), true).unwrap();
    assert_eq!(count(&store), 1);
    let keys = store.list_keys(ListFilter::new()).unwrap();
    let restored = store.get_time_series(keys[0].identity(), None).unwrap();
    assert_eq!(
        restored.as_single().unwrap().data.to_f64_vec().unwrap()[0],
        0.0
    );
}
