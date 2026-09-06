//! Slicing a grid whose period is a calendar month.
//!
//! A `SingleTimeSeries` and a dense forecast are both stored as an anchor, a
//! [`Period`], and a count, so a slicing read has to hand back the same shape
//! anchored at the slice's own first point. For a `Period::Fixed` that is exact.
//! For a `Period::Months` it is not, because the end-of-month clamp is not
//! associative:
//!
//! ```text
//! add_to(Jan-31, 1) = Feb-29        add_to(Feb-29, 1) = Mar-29
//! add_to(Jan-31, 2) = Mar-31   !=   add_to(add_to(Jan-31, 1), 1) = Mar-29
//! ```
//!
//! A grid stepping monthly from Jan-31 is Jan-31, Feb-29, Mar-31, …, but
//! re-anchored at its own Feb-29 it becomes Feb-29, Mar-29, Apr-29 — the stored
//! values under dates the store does not hold. No anchor fixes it: the sub-grid
//! keeps the *original* anchor's day of month and no instant in the slice
//! carries it.
//!
//! So the invariant these tests hold is not "every slice succeeds" but the one
//! that actually matters: **a read never reports a grid other than the one it
//! stored.** Where the shape cannot express the answer the read is refused, and
//! the refusal is narrow — a slice that re-anchors faithfully still succeeds.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, Deterministic, ListFilter, OwnerCategory, Period, ReadWindow, SingleTimeSeries,
    Store, TimeRange, TimeReference, TimeSeriesData, TimeSeriesType, TransformPolicy, TypedArray,
    create_store,
};

/// 2024-01-31: the first of the month ends, so `add_to` clamps at February.
fn month_end_anchor() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap()
}

/// 2024-01-15: a day every month has, so nothing is ever clamped.
fn safe_anchor() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap()
}

fn ramp(n: usize) -> TypedArray {
    TypedArray::from_f64(vec![n], &(0..n).map(|i| i as f64).collect::<Vec<_>>())
}

/// The grid the store actually holds: `initial + k·period`, calendar-aware.
fn grid(initial: DateTime<Utc>, period: Period, length: usize) -> Vec<DateTime<Utc>> {
    (0..length)
        .map(|k| period.add_to(initial, k as i64).unwrap())
        .collect()
}

fn add(store: &mut Store, data: TimeSeriesData) -> infrastore_core::TimeSeriesId {
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            data,
        ))
        .unwrap()
}

fn monthly_series(
    store: &mut Store,
    initial: DateTime<Utc>,
    length: usize,
    name: &str,
) -> infrastore_core::TimeSeriesId {
    let mut s = SingleTimeSeries::new(initial, Period::Months(1), ramp(length), name);
    s.time_reference = Some(TimeReference::Utc);
    add(store, TimeSeriesData::SingleTimeSeries(s))
}

/// The grid a returned series describes, rebuilt the way every consumer does —
/// the CLI's CSV export, Python's `timestamps` / `to_arrow`, Julia's reader.
fn reported_grid(s: &SingleTimeSeries) -> Vec<DateTime<Utc>> {
    grid(s.initial_timestamp, s.resolution, s.length)
}

/// No `ReadWindow` slice of a month-anchored monthly series reports a grid the
/// store does not hold. Slices that cannot be re-anchored are refused; the rest
/// come back exact.
#[test]
fn a_window_slice_never_invents_a_grid() {
    let mut store = create_store(None, true).unwrap();
    let length = 14;
    let stored = grid(month_end_anchor(), Period::Months(1), length);
    let id = monthly_series(&mut store, month_end_anchor(), length, "monthly");

    // Exactly the slices whose own anchor cannot regenerate the stored grid,
    // computed here from the grid itself — so this pins the refusal from both
    // sides: no drifting slice slips through, and no expressible one is lost.
    let mut expected_refusals = Vec::new();
    let mut refused = Vec::new();
    for k in 0..length {
        for n in 1..=(length - k) {
            if (1..n).any(|j| Period::Months(1).add_to(stored[k], j as i64) != Some(stored[k + j]))
            {
                expected_refusals.push((k, n));
            }
            match store.read_by_id(id, ReadWindow::from(stored[k]).with_len(n)) {
                Ok(TimeSeriesData::SingleTimeSeries(got)) => {
                    assert_eq!(
                        reported_grid(&got),
                        stored[k..k + n],
                        "slice {k}..{}",
                        k + n
                    );
                    assert_eq!(
                        got.data.to_f64_vec().unwrap(),
                        (k..k + n).map(|i| i as f64).collect::<Vec<_>>(),
                    );
                }
                Ok(other) => panic!("expected a SingleTimeSeries, got {other:?}"),
                Err(e) => {
                    assert!(
                        e.to_string()
                            .contains("cannot be expressed as a grid of its own"),
                        "slice {k}..{} refused for the wrong reason: {e}",
                        k + n,
                    );
                    refused.push((k, n));
                }
            }
        }
    }
    assert!(
        !refused.is_empty(),
        "the drifting slices are no longer reachable"
    );
    assert_eq!(refused, expected_refusals);
}

/// The same read against an anchor no month clamps: nothing is refused, and
/// every slice is exact. This is the guard against over-refusing.
#[test]
fn a_window_slice_of_an_unclamped_monthly_series_always_works() {
    let mut store = create_store(None, true).unwrap();
    let length = 14;
    let stored = grid(safe_anchor(), Period::Months(1), length);
    let id = monthly_series(&mut store, safe_anchor(), length, "monthly");

    for k in 0..length {
        for n in 1..=(length - k) {
            let TimeSeriesData::SingleTimeSeries(got) = store
                .read_by_id(id, ReadWindow::from(stored[k]).with_len(n))
                .unwrap_or_else(|e| panic!("slice {k}..{} refused: {e}", k + n))
            else {
                unreachable!()
            };
            assert_eq!(reported_grid(&got), stored[k..k + n]);
            assert_eq!(
                got.data.to_f64_vec().unwrap(),
                (k..k + n).map(|i| i as f64).collect::<Vec<_>>(),
            );
        }
    }
}

/// The same rule on `read_by_ids_range` — the path the CLI's
/// `export --time-range` and every bulk export use, and the one that was writing
/// drifted timestamps into re-importable CSV.
#[test]
fn a_range_slice_never_invents_a_grid() {
    let mut store = create_store(None, true).unwrap();
    let length = 8;
    let stored = grid(month_end_anchor(), Period::Months(1), length);
    let id = monthly_series(&mut store, month_end_anchor(), length, "monthly");

    let mut refused = 0;
    for a in 0..length {
        for b in (a + 1)..=length {
            let end = if b < length {
                stored[b]
            } else {
                stored[length - 1] + Duration::days(1)
            };
            match store.read_by_ids_range(&[id], TimeRange::new(stored[a], end)) {
                Ok(v) => {
                    let TimeSeriesData::SingleTimeSeries(got) = &v[0] else {
                        unreachable!()
                    };
                    assert_eq!(reported_grid(got), stored[a..b], "range {a}..{b}");
                }
                Err(e) => {
                    assert!(
                        e.to_string()
                            .contains("cannot be expressed as a grid of its own"),
                        "range {a}..{b} refused for the wrong reason: {e}",
                    );
                    refused += 1;
                }
            }
        }
    }
    assert!(refused > 0, "the drifting ranges are no longer reachable");
}

/// A timestamp a read reports has to be a timestamp the store accepts back —
/// otherwise a caller cannot page through a series with the answers it is given.
#[test]
fn every_reported_timestamp_is_readable_again() {
    let mut store = create_store(None, true).unwrap();
    let length = 8;
    let stored = grid(month_end_anchor(), Period::Months(1), length);
    let id = monthly_series(&mut store, month_end_anchor(), length, "monthly");

    for (k, anchor) in stored.iter().enumerate() {
        let Ok(TimeSeriesData::SingleTimeSeries(sliced)) =
            store.read_by_id(id, ReadWindow::from(*anchor).with_len(length - k))
        else {
            continue; // refused up front, which is the other half of the contract
        };
        for reported in reported_grid(&sliced) {
            let again = store.read_by_id(id, ReadWindow::from(reported).with_len(1));
            assert!(
                again.is_ok(),
                "a read from {anchor} reported {reported}, then refused it: {}",
                again.unwrap_err(),
            );
        }
    }
}

/// A forecast whose windows step by a calendar month follows the same rule.
#[test]
fn a_forecast_window_slice_never_invents_a_grid() {
    let mut store = create_store(None, true).unwrap();
    let (h, count) = (3usize, 8usize);
    let mut d = Deterministic::new(
        month_end_anchor(),
        Period::Fixed(Duration::hours(1)),
        Period::Fixed(Duration::hours(3)),
        Period::Months(1),
        count,
        TypedArray::from_f64(
            vec![h, count],
            &(0..h * count).map(|i| i as f64).collect::<Vec<_>>(),
        ),
        "monthly_fc",
    )
    .unwrap();
    d.time_reference = Some(TimeReference::Utc);
    let stored: Vec<_> = (0..count).map(|k| d.window_start(k).unwrap()).collect();
    let id = add(&mut store, TimeSeriesData::Deterministic(d));

    let mut refused = 0;
    for k in 0..count {
        for n in 1..=(count - k) {
            match store.read_by_id(id, ReadWindow::from(stored[k]).with_count(n)) {
                Ok(TimeSeriesData::Deterministic(got)) => {
                    assert_eq!(got.count, n);
                    let reported: Vec<_> = (0..n).map(|j| got.window_start(j).unwrap()).collect();
                    assert_eq!(reported, stored[k..k + n], "windows {k}..{}", k + n);
                }
                Ok(other) => panic!("expected a Deterministic, got {other:?}"),
                Err(e) => {
                    assert!(
                        e.to_string()
                            .contains("cannot be expressed as a grid of its own"),
                        "windows {k}..{} refused for the wrong reason: {e}",
                        k + n,
                    );
                    refused += 1;
                }
            }
        }
    }
    assert!(
        refused > 0,
        "the drifting window slices are no longer reachable"
    );
}

/// A derived `DeterministicSingleTimeSeries` is a view onto its source's own
/// steps, so every window must label them with the source grid. Over a
/// month-anchored source that is impossible, and the transform is refused —
/// before it writes, rather than producing a row that mislabels on every read.
#[test]
fn a_derived_forecast_over_a_clamped_source_is_refused() {
    let mut store = create_store(None, true).unwrap();
    monthly_series(&mut store, month_end_anchor(), 12, "monthly");

    let err = store
        .transform_single_time_series(
            Period::Months(3),
            Period::Months(1),
            None,
            None,
            TransformPolicy::default(),
        )
        .expect_err("a clamped monthly source cannot back a window view");
    assert!(
        err.to_string().contains("clamped by shorter months"),
        "refused for the wrong reason: {err}",
    );
    assert!(
        store
            .list_metadata(ListFilter {
                time_series_type: Some(TimeSeriesType::DeterministicSingleTimeSeries),
                ..Default::default()
            })
            .unwrap()
            .is_empty(),
        "the refused transform still wrote a row",
    );
}

/// Over an unclamped monthly source the transform goes through, and every
/// window labels its steps with the source's own grid.
#[test]
fn a_derived_forecast_over_an_unclamped_source_labels_the_source_grid() {
    let mut store = create_store(None, true).unwrap();
    let length = 12;
    let source = grid(safe_anchor(), Period::Months(1), length);
    monthly_series(&mut store, safe_anchor(), length, "monthly");

    store
        .transform_single_time_series(
            Period::Months(3),
            Period::Months(1),
            None,
            None,
            TransformPolicy::default(),
        )
        .unwrap();

    let rows = store
        .list_metadata(ListFilter {
            time_series_type: Some(TimeSeriesType::DeterministicSingleTimeSeries),
            ..Default::default()
        })
        .unwrap();
    let TimeSeriesData::Deterministic(d) = store
        .read_by_id(rows[0].id.unwrap(), ReadWindow::full())
        .unwrap()
    else {
        unreachable!()
    };

    let h = d.horizon_count();
    for k in 0..d.count {
        assert_eq!(
            d.window_timestamps(k).unwrap(),
            source[k..k + h],
            "window {k}",
        );
    }
}

/// `read_by_ids_range` is the bounds form that clips. A forecast whose first
/// window starts *after* the range start is not a partial window — it is no
/// window at all before that point — so the range clips to the windows it does
/// cover instead of failing, and one forecast no longer fails a whole batch.
#[test]
fn a_range_wider_than_a_forecast_clips_instead_of_erroring() {
    let mut store = create_store(None, true).unwrap();
    let initial = Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap();

    let mut s = SingleTimeSeries::new(initial, Duration::hours(1), ramp(24), "load");
    s.time_reference = Some(TimeReference::Utc);
    let sid = add(&mut store, TimeSeriesData::SingleTimeSeries(s));

    let mut d = Deterministic::new(
        initial,
        Period::Fixed(Duration::hours(1)),
        Period::Fixed(Duration::hours(4)),
        Period::Fixed(Duration::hours(6)),
        4,
        TypedArray::from_f64(vec![4, 4], &(0..16).map(|i| i as f64).collect::<Vec<_>>()),
        "fc",
    )
    .unwrap();
    d.time_reference = Some(TimeReference::Utc);
    let fid = add(&mut store, TimeSeriesData::Deterministic(d));

    // A calendar-year export window, starting earlier than everything stored.
    let year = TimeRange::new(
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
    );
    let out = store
        .read_by_ids_range(&[sid, fid], year)
        .unwrap_or_else(|e| panic!("a whole-year range over a static series and a forecast: {e}"));

    let TimeSeriesData::SingleTimeSeries(got_static) = &out[0] else {
        unreachable!()
    };
    assert_eq!(got_static.length, 24);
    assert_eq!(got_static.initial_timestamp, initial);

    let TimeSeriesData::Deterministic(got_fc) = &out[1] else {
        unreachable!()
    };
    assert_eq!(got_fc.count, 4, "every window lies inside the year");
    assert_eq!(got_fc.initial_timestamp, initial);

    // A range that starts before the forecast and ends inside it still clips on
    // the end, which is the half that was always meant to.
    let partial = TimeRange::new(
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        initial + Duration::hours(13),
    );
    let TimeSeriesData::Deterministic(clipped) =
        &store.read_by_ids_range(&[fid], partial).unwrap()[0]
    else {
        unreachable!()
    };
    assert_eq!(clipped.count, 3);
    assert_eq!(clipped.initial_timestamp, initial);
}
