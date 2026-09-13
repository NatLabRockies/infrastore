//! The transaction-lifetime shared-set cache, and the invalidations that keep it
//! honest.
//!
//! A run of adds interns each feature set once for the span instead of once per
//! call, which is what lets a client whose single add is a one-item batch produce
//! the catalog traffic a bulk add would. The cache remembers *writes*, so every
//! path that can undo one has to empty it. A stale entry does not make an add
//! slow, it makes it wrong: the add skips interning the set and files an
//! association naming a `feature_sets` row that is not there.
//!
//! Each test below removes an interned set by a different route and then re-adds
//! a series carrying it, asserting the features survive the round trip.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    FeatureValue, Features, NonSequentialTimeSeries, OwnerCategory, ReadWindow, SingleTimeSeries,
    Store, TimeSeriesData, TimeSeriesId, TypedArray,
};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

fn series(base: f64) -> SingleTimeSeries {
    let vals: Vec<f64> = (0..8).map(|i| base + i as f64).collect();
    SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![8], &vals),
        "load",
    )
}

/// The one feature set every add in these tests carries, so the second add is
/// always the one the cache would short-circuit.
fn feats() -> Features {
    let mut f = Features::new();
    f.insert("scenario".into(), FeatureValue::Str("high".into()));
    f.insert("year".into(), FeatureValue::Int(2030));
    f
}

fn add(store: &mut Store, owner: i64, base: f64) -> TimeSeriesId {
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(series(base)),
            feats(),
        )
        .unwrap()
}

/// The features the catalog hands back for `id`, which is empty rather than
/// absent when the association's set was never interned.
fn stored_features(store: &Store, id: TimeSeriesId) -> Features {
    store.get_metadata_by_id(id).unwrap().unwrap().features
}

fn each_backend(body: impl Fn(&mut Store, &str)) {
    {
        let mut store = Store::create(None, true).unwrap();
        body(&mut store, "memory");
    }
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.h5");
        let mut store = Store::create(Some(path.as_path()), false).unwrap();
        body(&mut store, "disk");
    }
}

/// Interning the set at an inner level and rolling *that* level back takes the
/// row with it, while the span continues. The next add carrying the same set
/// must intern it again.
#[test]
fn inner_rollback_reinterns_the_set() {
    each_backend(|store, backend| {
        store.begin_transaction().unwrap();
        store.begin_transaction().unwrap();
        add(store, 1, 0.0);
        store.rollback_transaction().unwrap();

        let id = add(store, 2, 100.0);
        store.commit_transaction().unwrap();

        assert_eq!(
            stored_features(store, id),
            feats(),
            "{backend}: the set was interned only at the level that rolled back"
        );
    });
}

/// The timestamp half of the cache: an irregular series writes its time axis at
/// an inner level, the level rolls back and removes the axis, and the outer span
/// adds another series on the same axis. The cache must not remember the removed
/// vector, or the committed row names an axis the file does not hold.
#[test]
fn inner_rollback_rewrites_the_time_axis() {
    let axis = vec![t0(), t0() + Duration::hours(3), t0() + Duration::days(2)];
    let irregular = |owner: i64, base: f64, store: &mut Store| {
        let data = TypedArray::from_f64(vec![3], &[base, base + 1.0, base + 2.0]);
        store
            .add_time_series(
                owner,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::NonSequentialTimeSeries(
                    NonSequentialTimeSeries::new(axis.clone(), data, "availability").unwrap(),
                ),
                Features::new(),
            )
            .unwrap()
    };
    each_backend(|store, backend| {
        store.begin_transaction().unwrap();
        store.begin_transaction().unwrap();
        irregular(1, 0.0, store);
        store.rollback_transaction().unwrap();

        let id = irregular(2, 100.0, store);
        store.commit_transaction().unwrap();

        let TimeSeriesData::NonSequentialTimeSeries(read) =
            store.read_by_id(id, ReadWindow::full()).unwrap()
        else {
            panic!("{backend}: expected a NonSequentialTimeSeries");
        };
        assert_eq!(
            read.timestamps, axis,
            "{backend}: the axis was written only at the level that rolled back"
        );
        let report = store.verify_integrity().unwrap();
        assert!(
            report.ok(),
            "{backend}: integrity errors: {:?}",
            report.errors
        );
    });
}

/// `clear_time_series` empties `feature_sets` outright, and can run inside a
/// transaction with adds on either side of it.
#[test]
fn clear_inside_a_transaction_reinterns_the_set() {
    each_backend(|store, backend| {
        store.begin_transaction().unwrap();
        add(store, 1, 0.0);
        store.clear_time_series(None).unwrap();

        let id = add(store, 2, 100.0);
        store.commit_transaction().unwrap();

        assert_eq!(
            stored_features(store, id),
            feats(),
            "{backend}: clear deleted every set, including the cached one"
        );
    });
}

/// The whole-span rollback: nothing the transaction interned survives, so a
/// fresh span re-interning the same set must write it.
#[test]
fn outer_rollback_reinterns_the_set() {
    each_backend(|store, backend| {
        store.begin_transaction().unwrap();
        add(store, 1, 0.0);
        store.rollback_transaction().unwrap();

        store.begin_transaction().unwrap();
        let id = add(store, 2, 100.0);
        store.commit_transaction().unwrap();

        assert_eq!(
            stored_features(store, id),
            feats(),
            "{backend}: the rolled-back span's intern must not be remembered"
        );
    });
}

/// The case the cache exists for: many adds in one span sharing a set all read
/// it back, and the set is stored once.
#[test]
fn a_span_of_adds_shares_one_interned_set() {
    each_backend(|store, backend| {
        store.begin_transaction().unwrap();
        let ids: Vec<_> = (0..20).map(|i| add(store, i, i as f64 * 10.0)).collect();
        store.commit_transaction().unwrap();

        for id in ids {
            assert_eq!(
                stored_features(store, id),
                feats(),
                "{backend}: every add in the span resolves its features"
            );
        }
    });
}
