//! Cross-cutting pins: timestamp precision, and the concurrency contract.
//!
//! Neither is a feature anyone designed — they are consequences of the
//! implementation that a caller can nonetheless depend on. Both are pinned so a
//! future change to either is a deliberate one:
//!
//! * **Precision.** The core stores a timestamp as an RFC3339 string but a
//!   `Period` as an integer count of **milliseconds**. Those two do not have the
//!   same resolution, and the bindings narrow it further (`datetime` is
//!   microsecond, Julia's `DateTime` is millisecond). Where a value is truncated
//!   rather than rejected, that is recorded here.
//! * **Concurrency.** `Store` is single-threaded by construction. This file
//!   asserts *which* auto-traits it has, what a second handle on one path does,
//!   and what a reader built before a mutation returns afterwards — no threads
//!   needed, since the point is the contract, not a race.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, Deterministic, ListFilter, NonSequentialTimeSeries, OwnerCategory, Period,
    SingleTimeSeries, Store, TimeSeriesData, TimeSeriesKey, TypedArray, create_store, open_store,
};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

fn add(store: &mut Store, owner: i64, data: TimeSeriesData) -> TimeSeriesKey {
    store
        .add(AddRequest::new(
            owner,
            "Generator",
            OwnerCategory::Component,
            data,
        ))
        .unwrap()
}

fn sts_at(name: &str, initial: DateTime<Utc>, resolution: impl Into<Period>) -> TimeSeriesData {
    TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
        initial,
        resolution,
        TypedArray::from_f64(vec![4], &[1.0, 2.0, 3.0, 4.0]),
        name,
    ))
}

// ===========================================================================
// 4.1 Timestamp and period precision
// ===========================================================================

#[test]
fn sub_second_resolutions_round_trip() {
    // Milliseconds are the `Period` unit, so anything down to 1 ms is exact.
    for (label, resolution) in [
        ("500ms", Duration::milliseconds(500)),
        ("1ms", Duration::milliseconds(1)),
        ("1s", Duration::seconds(1)),
        ("100ms", Duration::milliseconds(100)),
    ] {
        let mut store = create_store(None, true).unwrap();
        let key = add(&mut store, 1, sts_at("load", t0(), resolution));
        let period = Period::fixed(resolution);
        assert_eq!(key.resolution(), Some(period), "{label}");

        let got = store.get_time_series(key.identity(), None).unwrap();
        assert_eq!(got.as_single().unwrap().resolution, period, "{label}");

        // A time-range slice on the sub-second grid selects the right steps.
        let start = resolution;
        let end = resolution * 3;
        let sliced = store
            .get_time_series(key.identity(), Some((t0() + start, t0() + end)))
            .unwrap();
        assert_eq!(
            sliced.as_single().unwrap().data.to_f64_vec().unwrap(),
            vec![2.0, 3.0],
            "{label}: sliced values"
        );
        assert_eq!(
            sliced.as_single().unwrap().initial_timestamp,
            t0() + start,
            "{label}: sliced initial"
        );
    }
}

#[test]
fn sub_millisecond_resolutions_are_rejected_not_truncated() {
    // PIN: `Period::is_positive` tests `num_milliseconds() > 0`, so a
    // sub-millisecond duration is *not* positive and every forecast constructor
    // rejects it. A microsecond resolution is therefore unrepresentable rather
    // than silently rounded to 0 or 1 ms.
    for sub_ms in [
        Duration::microseconds(1),
        Duration::microseconds(999),
        Duration::nanoseconds(1),
        Duration::nanoseconds(999_999),
    ] {
        let period = Period::fixed(sub_ms);
        assert!(
            !period.is_positive(),
            "{sub_ms:?} must not count as a positive period"
        );
        assert_eq!(
            period.to_iso8601(),
            "PT0S",
            "{sub_ms:?}: sub-millisecond magnitude is lost in the ISO encoding"
        );

        // A forecast rejects it outright.
        assert!(
            Deterministic::new(
                t0(),
                sub_ms,
                Duration::hours(1),
                Duration::hours(1),
                1,
                TypedArray::from_f64(vec![1, 1], &[1.0]),
                "f",
            )
            .is_err(),
            "{sub_ms:?} accepted as a forecast resolution"
        );
    }

    // 1500 microseconds is 1 whole millisecond plus a remainder: PIN that the
    // remainder is dropped by the ISO encoding, so the stored resolution is
    // 1 ms, not 1.5 ms.
    let one_and_a_half = Period::fixed(Duration::microseconds(1_500));
    assert!(one_and_a_half.is_positive());
    assert_eq!(one_and_a_half.to_iso8601(), "PT0.001S");
    assert_eq!(
        Period::from_iso8601("PT0.001S").unwrap(),
        Period::fixed(Duration::milliseconds(1)),
        "the sub-millisecond remainder does not survive the encoding"
    );
}

#[test]
fn a_sub_millisecond_resolution_is_rejected_by_the_stores_read_path() {
    // A `SingleTimeSeries` constructor does *not* validate its resolution, so a
    // sub-millisecond one can be stored. PIN what happens next: a time-range
    // read errors rather than dividing by a zero-millisecond step.
    let mut store = create_store(None, true).unwrap();
    let key = add(
        &mut store,
        1,
        sts_at("load", t0(), Duration::microseconds(1)),
    );

    // A full read still works — no arithmetic on the resolution is needed.
    let got = store.get_time_series(key.identity(), None).unwrap();
    assert_eq!(got.as_single().unwrap().length, 4);

    // A sliced read needs a positive step and is rejected.
    let err = store
        .get_time_series(key.identity(), Some((t0(), t0() + Duration::seconds(1))))
        .unwrap_err();
    assert!(
        matches!(err, infrastore_core::TimeSeriesError::InvalidParameter(_)),
        "expected InvalidParameter, got {err:?}"
    );
}

#[test]
fn nanosecond_precision_timestamps_survive_the_catalog_round_trip() {
    // Timestamps are stored as RFC3339 strings, which carry sub-second digits, so
    // an initial timestamp keeps nanosecond precision even though a *period*
    // cannot. PIN that: the two have different resolutions.
    let precise = Utc
        .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc)
        + Duration::nanoseconds(123_456_789);
    assert_eq!(precise.timestamp_subsec_nanos(), 123_456_789);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = add(&mut store, 1, sts_at("load", precise, Duration::hours(1)));
        store.flush().unwrap();
        key
    };

    let store = open_store(path.as_path(), true).unwrap();
    let meta = store.get_metadata(key.identity()).unwrap();
    assert_eq!(
        meta.initial_timestamp,
        Some(precise),
        "nanoseconds must survive the RFC3339 catalog encoding"
    );
    let got = store.get_time_series(key.identity(), None).unwrap();
    assert_eq!(got.as_single().unwrap().initial_timestamp, precise);
    // And the key carries it too.
    assert_eq!(
        store.list_keys(ListFilter::new()).unwrap()[0]
            .identity()
            .name,
        "load"
    );
}

#[test]
fn a_sub_millisecond_offset_from_a_forecast_window_boundary_is_rejected() {
    // A `Period` is a whole number of milliseconds (see the `period.rs` module
    // docs), but a timestamp keeps nanoseconds — so a grid can be
    // millisecond-spaced and nanosecond-offset in its phase. The two paths that
    // consume a `time_range` treat an off-grid bound differently, by design:
    //
    //   * a *static* read floors/ceils the bounds onto the grid;
    //   * a *forecast* read requires the start to BE a window boundary.
    //
    // This previously diverged: `steps_between`'s `Fixed` branch tested only
    // `delta_ms % step_ms == 0`, and `delta_ms` truncates, so a start in the open
    // range `(boundary, boundary + 1ms)` passed the alignment check and was then
    // excluded by the window filter's exact `>=` — silently returning the *next*
    // window. `Fixed` now verifies the exact landing the way `Months` always has.
    let mut store = create_store(None, true).unwrap();

    // --- static: an off-grid start is floored onto the grid, as documented ---
    let static_key = add(&mut store, 1, sts_at("load", t0(), Duration::hours(1)));
    let nudged = t0() + Duration::hours(1) + Duration::nanoseconds(1);
    let sliced = store
        .get_time_series(
            static_key.identity(),
            Some((nudged, t0() + Duration::hours(3))),
        )
        .unwrap();
    assert_eq!(
        sliced.as_single().unwrap().initial_timestamp,
        t0() + Duration::hours(1),
        "a static read floors an off-grid start onto the grid"
    );

    // --- forecast: an off-grid start is rejected, at any magnitude ---
    let det = Deterministic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        3,
        TypedArray::from_f64(vec![2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        "det",
    )
    .unwrap();
    let fc_key = add(&mut store, 2, TimeSeriesData::Deterministic(det));

    for (label, offset) in [
        ("1ns", Duration::nanoseconds(1)),
        ("1us", Duration::microseconds(1)),
        ("999us", Duration::microseconds(999)),
        ("1ms", Duration::milliseconds(1)),
        ("1s", Duration::seconds(1)),
    ] {
        let start = t0() + Duration::hours(1) + offset;
        let err = store
            .get_time_series(fc_key.identity(), Some((start, t0() + Duration::hours(3))))
            .unwrap_err();
        assert!(
            matches!(err, infrastore_core::TimeSeriesError::InvalidParameter(_)),
            "{label} past a window boundary must be rejected, got {err:?}"
        );
    }

    // An exactly-aligned start selects the window the caller named.
    let exact = store
        .get_time_series(
            fc_key.identity(),
            Some((t0() + Duration::hours(1), t0() + Duration::hours(3))),
        )
        .unwrap();
    let exact = exact.as_deterministic().unwrap();
    assert_eq!(exact.initial_timestamp, t0() + Duration::hours(1));
    assert_eq!(exact.count, 2);
}

#[test]
fn a_forecast_on_a_nanosecond_offset_grid_reads_at_its_own_boundaries() {
    // The other side of the contract: a sub-millisecond `initial_timestamp` is
    // supported, so the grid's *phase* can be finer than a millisecond. A read at
    // the grid's own boundary — nanoseconds included — must work, and a bound
    // rounded to the millisecond must not.
    let initial = t0() + Duration::nanoseconds(500);
    let mut store = create_store(None, true).unwrap();
    let det = Deterministic::new(
        initial,
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        3,
        TypedArray::from_f64(vec![2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        "det",
    )
    .unwrap();
    let key = add(&mut store, 1, TimeSeriesData::Deterministic(det));

    // The exact window-1 boundary carries the same 500ns phase.
    let boundary = initial + Duration::hours(1);
    let got = store
        .get_time_series(
            key.identity(),
            Some((boundary, boundary + Duration::hours(2))),
        )
        .unwrap();
    let fc = got.as_deterministic().unwrap();
    assert_eq!(fc.initial_timestamp, boundary);
    assert_eq!(fc.count, 2);

    // The same instant rounded down to the millisecond is not a window boundary.
    for rounded in [t0() + Duration::hours(1), t0()] {
        assert!(
            store
                .get_time_series(
                    key.identity(),
                    Some((rounded, rounded + Duration::hours(2)))
                )
                .is_err(),
            "a millisecond-rounded bound is off a nanosecond-offset grid"
        );
    }
}

#[test]
fn pre_1970_initial_timestamps_round_trip() {
    // A negative Unix timestamp. Anything that routes a timestamp through an
    // unsigned integer or a millisecond count would corrupt these.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let cases = [
        Utc.with_ymd_and_hms(1969, 12, 31, 23, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(1900, 1, 1, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(1800, 6, 15, 12, 30, 45).unwrap(),
    ];

    let keys: Vec<TimeSeriesKey> = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let keys = cases
            .iter()
            .enumerate()
            .map(|(i, initial)| {
                add(
                    &mut store,
                    i as i64 + 1,
                    sts_at(&format!("old_{i}"), *initial, Duration::hours(1)),
                )
            })
            .collect();
        store.flush().unwrap();
        keys
    };

    let store = open_store(path.as_path(), true).unwrap();
    for (key, expected) in keys.iter().zip(&cases) {
        assert!(expected.timestamp() < 0, "{expected} should be pre-1970");
        let got = store.get_time_series(key.identity(), None).unwrap();
        assert_eq!(
            got.as_single().unwrap().initial_timestamp,
            *expected,
            "pre-1970 initial timestamp"
        );

        // A time-range slice still resolves against a negative epoch.
        let sliced = store
            .get_time_series(
                key.identity(),
                Some((
                    *expected + Duration::hours(1),
                    *expected + Duration::hours(3),
                )),
            )
            .unwrap();
        assert_eq!(
            sliced.as_single().unwrap().data.to_f64_vec().unwrap(),
            vec![2.0, 3.0]
        );
    }
}

#[test]
fn a_series_spanning_the_epoch_boundary_reads_correctly() {
    // Starting before 1970 and ending after it: the grid arithmetic must not
    // treat the sign change as a boundary.
    let initial = Utc.with_ymd_and_hms(1969, 12, 31, 22, 0, 0).unwrap();
    let values: Vec<f64> = (0..6).map(|i| i as f64).collect();
    let mut store = create_store(None, true).unwrap();
    let key = add(
        &mut store,
        1,
        TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
            initial,
            Duration::hours(1),
            TypedArray::from_f64(vec![6], &values),
            "spanning",
        )),
    );

    // Hour 2 is exactly the epoch.
    let epoch = Utc.timestamp_opt(0, 0).single().unwrap();
    assert_eq!(initial + Duration::hours(2), epoch);
    let sliced = store
        .get_time_series(key.identity(), Some((epoch, epoch + Duration::hours(2))))
        .unwrap();
    let single = sliced.as_single().unwrap();
    assert_eq!(single.initial_timestamp, epoch);
    assert_eq!(single.data.to_f64_vec().unwrap(), vec![2.0, 3.0]);
}

#[test]
fn a_century_spanning_non_sequential_series_round_trips() {
    // Explicit timestamps, so nothing is derived from a grid: the span is only
    // limited by what a `DateTime<Utc>` and an RFC3339 string can hold.
    let timestamps = vec![
        Utc.with_ymd_and_hms(1900, 1, 1, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(1969, 12, 31, 23, 59, 59).unwrap(),
        Utc.timestamp_opt(0, 0).single().unwrap(),
        Utc.with_ymd_and_hms(2024, 2, 29, 12, 0, 0).unwrap(), // leap day
        Utc.with_ymd_and_hms(2100, 12, 31, 23, 59, 59).unwrap(),
    ];
    let values: Vec<f64> = (0..timestamps.len()).map(|i| i as f64 * 10.0).collect();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let series = NonSequentialTimeSeries::new(
            timestamps.clone(),
            TypedArray::from_f64(vec![timestamps.len()], &values),
            "century",
        )
        .unwrap();
        let key = add(
            &mut store,
            1,
            TimeSeriesData::NonSequentialTimeSeries(series),
        );
        store.flush().unwrap();
        key
    };

    let store = open_store(path.as_path(), true).unwrap();
    let got = store.get_time_series(key.identity(), None).unwrap();
    let ns = got.as_non_sequential().unwrap();
    assert_eq!(ns.timestamps, timestamps, "a 200-year span must round trip");
    assert_eq!(ns.data.to_f64_vec().unwrap(), values);

    // A slice across the epoch selects by timestamp, not by index arithmetic.
    let sliced = store
        .get_time_series(
            key.identity(),
            Some((
                Utc.with_ymd_and_hms(1969, 1, 1, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            )),
        )
        .unwrap();
    let ns = sliced.as_non_sequential().unwrap();
    assert_eq!(ns.timestamps, timestamps[1..4].to_vec());
    assert_eq!(ns.data.to_f64_vec().unwrap(), values[1..4].to_vec());
}

#[test]
fn non_sequential_timestamps_keep_sub_second_precision() {
    // Two timestamps one millisecond apart are distinct (and strictly
    // increasing); one nanosecond apart also are, because the stored encoding is
    // RFC3339 rather than a millisecond count.
    let base = t0();
    let timestamps = vec![
        base,
        base + Duration::nanoseconds(1),
        base + Duration::microseconds(1),
        base + Duration::milliseconds(1),
    ];
    let values = vec![1.0f64, 2.0, 3.0, 4.0];

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let series = NonSequentialTimeSeries::new(
            timestamps.clone(),
            TypedArray::from_f64(vec![4], &values),
            "precise",
        )
        .unwrap();
        let key = add(
            &mut store,
            1,
            TimeSeriesData::NonSequentialTimeSeries(series),
        );
        store.flush().unwrap();
        key
    };

    let store = open_store(path.as_path(), true).unwrap();
    let got = store.get_time_series(key.identity(), None).unwrap();
    assert_eq!(
        got.as_non_sequential().unwrap().timestamps,
        timestamps,
        "sub-millisecond spacing must survive; a millisecond-quantized encoding \
         would collapse these four into one"
    );
}

// ===========================================================================
// 4.2 Concurrency contract
// ===========================================================================

#[test]
fn store_is_send_but_not_sync() {
    // PIN the auto-traits `Store` currently has, so gaining or losing either is
    // a deliberate change.
    //
    // `Send` holds: a `Store` can be moved to another thread, which is what lets
    // a server own one per connection or hand it to a blocking task.
    //
    // `Sync` does NOT hold: `rusqlite::Connection` contains `RefCell`s, so a
    // `&Store` cannot be shared across threads even for reads. A caller wanting
    // concurrent readers must wrap it (`Mutex<Store>`) or open one store per
    // thread. Because a negative bound cannot be written in a test, the `!Sync`
    // half is asserted by the `compile_fail` doc-test in
    // `tests/ui/store_is_not_sync.rs`'s stead: here we simply document it, and
    // the positive assertion below would stop compiling if `Sync` were added
    // *and* someone deleted this comment.
    fn assert_send<T: Send>() {}
    assert_send::<Store>();

    // `StaticReader` / `ForecastReader` are plain data and are both.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<infrastore_core::StaticReader>();
    assert_send_sync::<infrastore_core::ForecastReader>();
    assert_send_sync::<infrastore_core::TimeSeriesKey>();
    assert_send_sync::<infrastore_core::TimeSeriesData>();
    assert_send_sync::<infrastore_core::TypedArray>();
}

/// A `Store` really can cross a thread boundary, which is the practical content
/// of the `Send` bound above.
#[test]
fn a_store_can_be_moved_to_another_thread() {
    let mut store = create_store(None, true).unwrap();
    let key = add(&mut store, 1, sts_at("load", t0(), Duration::hours(1)));

    let handle = std::thread::spawn(move || {
        let got = store.get_time_series(key.identity(), None).unwrap();
        got.as_single().unwrap().data.to_f64_vec().unwrap()
    });
    assert_eq!(handle.join().unwrap(), vec![1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn a_second_read_only_handle_on_one_path_can_be_opened() {
    // Two handles on one on-disk store, in one process. PIN that a second
    // read-only open succeeds alongside the first (the NetCDF side takes a
    // shared HDF5 lock, and SQLite readers do not exclude each other).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = add(&mut store, 1, sts_at("load", t0(), Duration::hours(1)));
        store.flush().unwrap();
        key
    };

    let first = open_store(path.as_path(), true).unwrap();
    let second = open_store(path.as_path(), true).unwrap();
    for (label, store) in [("first", &first), ("second", &second)] {
        let got = store.get_time_series(key.identity(), None).unwrap();
        assert_eq!(
            got.as_single().unwrap().data.to_f64_vec().unwrap(),
            vec![1.0, 2.0, 3.0, 4.0],
            "{label}"
        );
    }
}

#[test]
fn a_read_only_handle_alongside_a_writable_one_is_pinned() {
    // PIN whichever way this lands: a read-write handle is already open when a
    // read-only one is requested. HDF5's file locking may or may not permit it,
    // and the SQLite side has a 5 s busy_timeout, so the outcome is
    // platform-dependent — the point is that it either works or fails cleanly,
    // never corrupts.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.nc");
    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = add(&mut store, 1, sts_at("load", t0(), Duration::hours(1)));
        store.flush().unwrap();
        key
    };

    let writable = open_store(path.as_path(), false).unwrap();
    match open_store(path.as_path(), true) {
        Ok(reader) => {
            // If it opens, it must read the flushed data correctly.
            let got = reader.get_time_series(key.identity(), None).unwrap();
            assert_eq!(
                got.as_single().unwrap().data.to_f64_vec().unwrap(),
                vec![1.0, 2.0, 3.0, 4.0]
            );
        }
        Err(e) => {
            // If it does not, the failure carries a diagnostic.
            assert!(!e.to_string().is_empty());
        }
    }
    drop(writable);

    // Once the writable handle is gone, a read-only open definitely works.
    let reader = open_store(path.as_path(), true).unwrap();
    assert!(reader.get_time_series(key.identity(), None).is_ok());
}

#[test]
fn a_reader_built_before_a_removal_of_a_shared_array_reads_stale_values() {
    // `build_static_reader` returns an *owned* `StaticReader`: it borrows nothing
    // from the store, so the borrow checker permits mutating the store and then
    // reading through the reader. This test and the next pin what that returns.
    //
    // Here the two series hold identical values, so they share one
    // content-addressed array. Removing one leaves the array alive for the other,
    // and the stale reader's read therefore SUCCEEDS — returning the column set
    // and values it snapshotted at build time, including the column whose
    // association no longer exists.
    let mut store = create_store(None, true).unwrap();
    let a = add(&mut store, 1, sts_at("a", t0(), Duration::hours(1)));
    add(&mut store, 2, sts_at("b", t0(), Duration::hours(1)));
    assert_eq!(
        store.num_distinct_arrays().unwrap(),
        1,
        "identical values share"
    );

    let mut reader = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    let columns_at_build = reader.groups()[0].num_columns();
    assert_eq!(columns_at_build, 2);

    store.static_read(&mut reader, t0()).unwrap();
    let before = reader.groups()[0].values_to_vec::<f64>().unwrap();
    assert_eq!(before, vec![1.0, 1.0]);

    store.remove_time_series(a.identity()).unwrap();
    assert_eq!(
        store.num_distinct_arrays().unwrap(),
        1,
        "the array survives for the remaining series"
    );

    // The reader's plan is a snapshot and still reports two columns.
    assert_eq!(
        reader.groups()[0].num_columns(),
        columns_at_build,
        "PIN: the reader does not notice the removal"
    );
    store.static_read(&mut reader, t0()).unwrap();
    assert_eq!(
        reader.groups()[0].values_to_vec::<f64>().unwrap(),
        before,
        "PIN: a stale reader returns the snapshot's values, not garbage"
    );

    // Rebuilding reflects the removal.
    let rebuilt = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    assert_eq!(rebuilt.groups()[0].num_columns(), 1);
}

#[test]
fn a_reader_built_before_a_removal_of_an_unshared_array_errors() {
    // The complement: the two series hold *different* values, so each has its own
    // array. Removing one reclaims its array, and the stale reader's next read
    // fails with `NotFound` rather than returning whatever now occupies the slot.
    //
    // This is the important half of the contract: a reclaimed slot is
    // reusable, so a silent success here could hand back another series' data.
    // Both bindings behave the same way — see `test_parity.py`'s
    // `test_a_reader_built_before_a_removal_errors_on_the_next_read`.
    let mut store = create_store(None, true).unwrap();
    let a = add(&mut store, 1, sts_at("a", t0(), Duration::hours(1)));
    add(
        &mut store,
        2,
        TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
            t0(),
            Duration::hours(1),
            TypedArray::from_f64(vec![4], &[100.0, 101.0, 102.0, 103.0]),
            "b",
        )),
    );
    assert_eq!(store.num_distinct_arrays().unwrap(), 2, "distinct values");

    let mut reader = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    store.static_read(&mut reader, t0()).unwrap();
    let mut before = reader.groups()[0].values_to_vec::<f64>().unwrap();
    before.sort_by(f64::total_cmp);
    assert_eq!(before, vec![1.0, 100.0]);

    store.remove_time_series(a.identity()).unwrap();
    assert_eq!(
        store.num_distinct_arrays().unwrap(),
        1,
        "a's array reclaimed"
    );

    let err = store.static_read(&mut reader, t0()).unwrap_err();
    assert!(
        matches!(err, infrastore_core::TimeSeriesError::NotFound),
        "PIN: a stale reader over a reclaimed array errors rather than reading a \
         reused slot; got {err:?}"
    );

    // A rebuilt reader works and sees only the survivor.
    let mut rebuilt = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    assert_eq!(rebuilt.groups()[0].num_columns(), 1);
    store.static_read(&mut rebuilt, t0()).unwrap();
    assert_eq!(
        rebuilt.groups()[0].values_to_vec::<f64>().unwrap(),
        vec![100.0]
    );
}

#[test]
fn a_reader_built_before_an_add_does_not_see_the_new_series() {
    // The complement: additions are invisible to an existing reader too, so a
    // caller stepping a timeline gets a stable column set for the whole sweep.
    let mut store = create_store(None, true).unwrap();
    add(&mut store, 1, sts_at("a", t0(), Duration::hours(1)));

    let mut reader = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    assert_eq!(reader.groups()[0].num_columns(), 1);

    add(&mut store, 2, sts_at("b", t0(), Duration::hours(1)));

    assert_eq!(
        reader.groups()[0].num_columns(),
        1,
        "PIN: the reader's column set is fixed at build time"
    );
    store.static_read(&mut reader, t0()).unwrap();
    assert_eq!(
        reader.groups()[0].values_to_vec::<f64>().unwrap(),
        vec![1.0]
    );

    // A rebuilt reader has both.
    let rebuilt = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    assert_eq!(rebuilt.groups()[0].num_columns(), 2);
}

#[test]
fn a_reader_survives_a_rename_of_the_series_it_points_at() {
    // A rename moves only the association row; the array is untouched and the
    // reader addresses arrays by hash. PIN that a sweep therefore keeps working,
    // while the reader's cached key still shows the old name.
    let mut store = create_store(None, true).unwrap();
    let key = add(&mut store, 1, sts_at("old", t0(), Duration::hours(1)));

    let mut reader = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    assert_eq!(reader.groups()[0].keys()[0].name(), "old");

    store.rename_time_series(key.identity(), "new").unwrap();

    assert_eq!(
        reader.groups()[0].keys()[0].name(),
        "old",
        "PIN: the reader's keys are a build-time snapshot"
    );
    store.static_read(&mut reader, t0()).unwrap();
    assert_eq!(
        reader.groups()[0].values_to_vec::<f64>().unwrap(),
        vec![1.0],
        "the values still read, because the array is addressed by hash"
    );
}
