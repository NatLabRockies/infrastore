//! `Store::build_static_reader_over`: a reader over a span the caller names.
//!
//! The uniform reader takes its grid from the series it matched, so a store
//! whose `SingleTimeSeries` start at different instants — or run for different
//! lengths — has no reader at all. That is the common shape of a real system: a
//! year of load beside a week of an outage schedule, or one component's data
//! logged from an hour later than the rest. A window names the span instead, and
//! each column reads at an offset of its own.
//!
//! What is pinned here is mostly what the window *refuses*, because that is
//! where a windowed read could otherwise hand back plausible wrong numbers:
//! a column that does not cover the span, an anchor off a column's grid, and a
//! calendar period re-anchored where the dates would move.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, ListFilter, OwnerCategory, Period, ReadWindow, SingleTimeSeries, Store,
    TimeSeriesData, TypedArray, create_store,
};

fn t(hour: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap() + Duration::hours(hour)
}

fn store() -> Store {
    create_store(None, true).unwrap()
}

/// A `SingleTimeSeries` of `len` hourly values starting at hour `start`, whose
/// value at every step is `start + k` — so a read proves *which* row it landed
/// on, not merely that it read something.
fn hourly(store: &mut Store, owner: i64, name: &str, start: i64, len: usize) {
    let values: Vec<f64> = (0..len).map(|k| (start + k as i64) as f64).collect();
    let ts = SingleTimeSeries::new(
        t(start),
        Period::Fixed(Duration::hours(1)),
        TypedArray::from_f64(vec![len], &values),
        name,
    );
    store
        .add(AddRequest::new(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(ts),
        ))
        .unwrap();
}

fn hourly_filter() -> ListFilter {
    ListFilter::new().resolution(Duration::hours(1))
}

/// Every column's value at `at`, in column order.
fn read_at(
    store: &Store,
    reader: &mut infrastore_core::StaticReader,
    at: DateTime<Utc>,
) -> Vec<f64> {
    store.static_read(reader, at).unwrap();
    reader.groups()[0].values_to_vec::<f64>().unwrap()
}

#[test]
fn ragged_starts_sweep_together_over_a_named_window() {
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24); // 00:00 .. 23:00
    hourly(&mut s, 2, "b", 7, 48); // 07:00 .. next-day 06:00

    // Without a window there is no reader at all: the two grids differ.
    let err = s.build_static_reader(hourly_filter()).unwrap_err();
    assert!(err.to_string().contains("requires a uniform grid"), "{err}");

    // With one, both columns read the same instants from different rows.
    let mut reader = s
        .build_static_reader_over(hourly_filter(), ReadWindow::from(t(7)).with_len(17))
        .unwrap();
    assert_eq!(reader.initial_timestamp(), t(7));
    assert_eq!(reader.length(), 17);
    assert_eq!(reader.groups()[0].num_columns(), 2);

    // Column 'a' is at its row 7, column 'b' at its row 0 -- both hour 7.
    assert_eq!(read_at(&s, &mut reader, t(7)), vec![7.0, 7.0]);
    assert_eq!(read_at(&s, &mut reader, t(23)), vec![23.0, 23.0]);
}

#[test]
fn an_omitted_length_runs_as_far_as_every_column_reaches() {
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24); // ends at hour 23
    hourly(&mut s, 2, "b", 7, 48); // ends at hour 54

    let reader = s
        .build_static_reader_over(hourly_filter(), ReadWindow::from(t(7)))
        .unwrap();
    // 'a' has 17 rows left from hour 7; 'b' has 48. The shorter one decides.
    assert_eq!(reader.length(), 17);
    assert_eq!(reader.timestamps().last(), Some(t(23)));
}

#[test]
fn a_column_that_does_not_cover_the_window_is_named() {
    let mut s = store();
    hourly(&mut s, 1, "short", 0, 24);
    hourly(&mut s, 2, "long", 0, 8784);

    let err = s
        .build_static_reader_over(hourly_filter(), ReadWindow::from(t(0)).with_len(8784))
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("'short'"), "{msg}");
    assert!(msg.contains("owner 1"), "{msg}");
    assert!(msg.contains("does not cover the reader window"), "{msg}");
    // The grids read as dates and durations, not as a Debug-printed TimeDelta.
    assert!(msg.contains("2024-01-01T00:00:00Z"), "{msg}");
    assert!(msg.contains("PT1H"), "{msg}");
    assert!(!msg.contains("TimeDelta"), "{msg}");
}

#[test]
fn an_anchor_off_a_columns_grid_is_refused() {
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24);
    hourly(&mut s, 2, "b", 7, 48);

    // Before 'b' starts: 'b' has no row there, and the store will not pad one.
    let err = s
        .build_static_reader_over(hourly_filter(), ReadWindow::from(t(3)).with_len(4))
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("'b'"), "{msg}");
    assert!(msg.contains("not on the grid"), "{msg}");
}

#[test]
fn an_anchor_between_steps_is_checked_not_floored() {
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24);

    let half_past = t(6) + Duration::minutes(30);
    let err = s
        .build_static_reader_over(hourly_filter(), ReadWindow::from(half_past).with_len(2))
        .unwrap_err();
    assert!(err.to_string().contains("not on the grid"), "{err}");
}

#[test]
fn a_window_a_single_series_covers_is_just_a_slice() {
    // One series, no raggedness: the window is still the useful thing, because
    // it shortens the axis to the span a simulation actually runs over.
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24);

    let mut reader = s
        .build_static_reader_over(hourly_filter(), ReadWindow::from(t(6)).with_len(3))
        .unwrap();
    assert_eq!(
        reader.timestamps().collect::<Vec<_>>(),
        vec![t(6), t(7), t(8)]
    );
    assert_eq!(read_at(&s, &mut reader, t(8)), vec![8.0]);
    // Past the window's own end, even though the series has more rows.
    assert!(s.static_read(&mut reader, t(9)).is_err());
}

#[test]
fn a_monthly_grid_is_refused_where_re_anchoring_would_move_the_dates() {
    // Jan-31 monthly: Jan-31, Feb-29, Mar-31. Re-anchored at its own Feb-29 it
    // would read Feb-29, Mar-29, Apr-29 -- right values, wrong dates. The same
    // rule `read_by_id` and `transform_single_time_series` are held to.
    let mut s = store();
    let jan31 = Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap();
    let ts = SingleTimeSeries::new(
        jan31,
        Period::Months(1),
        TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
        "monthly",
    );
    s.add(AddRequest::new(
        1,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::SingleTimeSeries(ts),
    ))
    .unwrap();

    let filter = || ListFilter::new().resolution(Period::Months(1));
    let feb29 = Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap();
    let err = s
        .build_static_reader_over(filter(), ReadWindow::from(feb29).with_len(2))
        .unwrap_err();
    assert!(err.to_string().contains("cannot be re-anchored"), "{err}");

    // The anchor that keeps the day of month is fine, and so is the series' own
    // start -- nothing was clamped away in either.
    assert!(
        s.build_static_reader_over(filter(), ReadWindow::from(jan31).with_len(3))
            .is_ok()
    );
}

#[test]
fn the_window_arguments_that_mean_nothing_here_are_errors() {
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24);

    // `count` counts forecast windows.
    let err = s
        .build_static_reader_over(hourly_filter(), ReadWindow::from(t(0)).with_count(2))
        .unwrap_err();
    assert!(err.to_string().contains("counts forecast windows"), "{err}");

    // A length with nothing to anchor it.
    let mut window = ReadWindow::full();
    window.len = Some(4);
    let err = s
        .build_static_reader_over(hourly_filter(), window)
        .unwrap_err();
    assert!(err.to_string().contains("needs a start"), "{err}");

    // An empty span.
    let err = s
        .build_static_reader_over(hourly_filter(), ReadWindow::from(t(0)).with_len(0))
        .unwrap_err();
    assert!(err.to_string().contains("is empty"), "{err}");
}

#[test]
fn no_window_is_the_grid_the_series_share() {
    // The delegation is exact: `build_static_reader` is this call with an empty
    // window, so a uniform store behaves as it always did.
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24);
    hourly(&mut s, 2, "b", 0, 24);

    let inherited = s.build_static_reader(hourly_filter()).unwrap();
    let explicit = s
        .build_static_reader_over(hourly_filter(), ReadWindow::full())
        .unwrap();
    assert_eq!(inherited.initial_timestamp(), explicit.initial_timestamp());
    assert_eq!(inherited.length(), explicit.length());
    assert_eq!(
        inherited.groups()[0].num_columns(),
        explicit.groups()[0].num_columns()
    );
}

/// The other answer to a store whose series share no grid: name the grid, and
/// the series that are not on it are not there to constrain the sweep.
///
/// `ListFilter::initial_timestamp` + `length` is the constructive remedy for
/// grid coherence, the role `ListFilter::zoneless` plays for spelling. The two
/// remedies answer different questions — a window sweeps a span across ragged
/// series, a filter drops the ragged ones — so the pins here are mostly about
/// keeping them distinguishable.
#[test]
fn the_grid_filter_selects_one_cohort() {
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24); // the stray
    hourly(&mut s, 2, "b", 7, 48);
    hourly(&mut s, 3, "c", 7, 48);

    let on_grid = || hourly_filter().initial_timestamp(t(7)).length(48);

    // The listing side: two of the three.
    let rows = s.list_metadata(on_grid()).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.owner_id != 1));

    // The reader side: the full 48, not the 17 all three share.
    let mut reader = s.build_static_reader(on_grid()).unwrap();
    assert_eq!(reader.length(), 48);
    assert_eq!(reader.groups()[0].num_columns(), 2);
    assert_eq!(read_at(&s, &mut reader, t(54)), vec![54.0, 54.0]);
}

#[test]
fn the_window_and_the_filter_answer_different_questions() {
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24);
    hourly(&mut s, 2, "b", 7, 48);

    // A window at hour 7 takes both columns and is capped by the shorter one.
    let windowed = s
        .build_static_reader_over(hourly_filter(), ReadWindow::from(t(7)))
        .unwrap();
    assert_eq!(windowed.groups()[0].num_columns(), 2);
    assert_eq!(windowed.length(), 17);

    // The filter at the same instant drops the series that starts elsewhere.
    let filtered = s
        .build_static_reader(hourly_filter().initial_timestamp(t(7)))
        .unwrap();
    assert_eq!(filtered.groups()[0].num_columns(), 1);
    assert_eq!(filtered.length(), 48);
}

#[test]
fn a_grid_no_row_is_on_matches_nothing_rather_than_erroring() {
    // A filter selects; it does not assert. That is why the reader's own
    // "nothing matched" error is what surfaces, not a complaint about the bound.
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24);

    assert!(
        s.list_metadata(hourly_filter().initial_timestamp(t(3)))
            .unwrap()
            .is_empty()
    );
    let err = s
        .build_static_reader(hourly_filter().initial_timestamp(t(3)))
        .unwrap_err();
    assert!(
        err.to_string().contains("no SingleTimeSeries match"),
        "{err}"
    );
}

#[test]
fn the_grid_filter_is_not_part_of_identity() {
    // Two series differing only in start would be one row to the catalog, so an
    // identity probe must not narrow by grid: adding the same key twice is a
    // duplicate however long each series is.
    let mut s = store();
    hourly(&mut s, 1, "a", 0, 24);
    let values: Vec<f64> = (0..48).map(|k| k as f64).collect();
    let ts = SingleTimeSeries::new(
        t(7),
        Period::Fixed(Duration::hours(1)),
        TypedArray::from_f64(vec![48], &values),
        "a",
    );
    let err = s
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(ts),
        ))
        .unwrap_err();
    assert!(
        matches!(err, infrastore_core::TimeSeriesError::DuplicateTimeSeries),
        "{err:?}"
    );
}
