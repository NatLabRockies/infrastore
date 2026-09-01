//! `PersistentTimeSeries`: the step-function read semantics, and the
//! per-column-breakpoint `StaticReader` built on them.
//!
//! The six cases that define the type — read at a breakpoint, between
//! breakpoints, after the last, before the first, a range read whose start is
//! not a breakpoint, and a reader over misaligned columns — all live here, plus
//! the storage-sharing claim that a persistent series and a non-sequential one
//! on the same breakpoints occupy one stored array.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    ElementType, Features, Instants, ListFilter, NonSequentialTimeSeries, OwnerCategory,
    PersistentTimeSeries, ReadWindow, StaticReader, Store, TimeRange, TimeSeriesData, TimeSeriesId,
    TimeSeriesType, TypedArray, create_store,
};

/// `2024-<month>-01T00:00:00Z`.
fn month(m: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, m, 1, 0, 0, 0).unwrap()
}

/// A step function over the given months, valued `10 * month`.
fn curve(name: &str, months: &[u32]) -> PersistentTimeSeries {
    let timestamps: Vec<_> = months.iter().map(|m| month(*m)).collect();
    let values: Vec<f64> = months.iter().map(|m| *m as f64 * 10.0).collect();
    PersistentTimeSeries::new(
        timestamps,
        TypedArray::from_f64(vec![months.len()], &values),
        name,
    )
    .unwrap()
}

fn add(store: &mut Store, owner_id: i64, series: PersistentTimeSeries) -> TimeSeriesId {
    store
        .add_time_series(
            owner_id,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::PersistentTimeSeries(series),
            Features::new(),
        )
        .unwrap()
}

/// The name of every reader column, in buffer order.
///
/// A group holds association ids, not rows, so a test that wants names asks the
/// store for them — the same round trip a caller makes.
fn column_names(store: &Store, reader: &StaticReader) -> Vec<String> {
    reader
        .groups()
        .iter()
        .flat_map(|g| g.ids())
        .map(|id| {
            store
                .get_metadata_by_id(*id)
                .unwrap()
                .expect("a reader column names a live row")
                .name
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The lookup itself
// ---------------------------------------------------------------------------

#[test]
fn index_in_force_at_covers_the_four_boundary_cases() {
    let c = curve("gas", &[1, 4, 7]);
    let value_at = |t| c.data.to_vec::<f64>().unwrap()[c.index_in_force_at(t).unwrap()];

    // 1. exactly at a breakpoint -> that breakpoint's value (right-continuous).
    assert_eq!(value_at(month(1)), 10.0);
    assert_eq!(value_at(month(4)), 40.0);
    assert_eq!(value_at(month(7)), 70.0);

    // 2. between breakpoints -> the previous value. This is the case that
    //    diverges from NonSequentialTimeSeries, where it is a hard error.
    assert_eq!(value_at(month(2)), 10.0);
    assert_eq!(value_at(month(4) + Duration::seconds(1)), 40.0);

    // 3. after the last breakpoint -> the last value, forever.
    assert_eq!(value_at(month(12)), 70.0);
    assert_eq!(
        value_at(Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap()),
        70.0
    );

    // 4. before the first breakpoint -> an error naming the series, never a
    //    clamp. A value there was never declared.
    let err = c
        .index_in_force_at(month(1) - Duration::milliseconds(1))
        .unwrap_err();
    assert!(err.contains("gas"), "{err}");
    assert!(err.contains("before the first breakpoint"), "{err}");
}

// ---------------------------------------------------------------------------
// Store round trip
// ---------------------------------------------------------------------------

#[test]
fn a_persistent_series_round_trips_through_an_on_disk_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let original = curve("gas_price", &[1, 4, 7, 10])
        .with_units("USD/MMBtu")
        .with_component_field("fuel_cost")
        .with_application_data(r#"{"as_time_series":false,"force_scalar_mode":"midpoint"}"#);
    let id = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let id = store
            .add_time_series(
                7,
                "ThermalStandard",
                OwnerCategory::Component,
                TimeSeriesData::PersistentTimeSeries(original.clone()),
                Features::new(),
            )
            .unwrap();
        store.flush().unwrap();
        id
    };

    let store = infrastore_core::open_store(path.as_path(), true).unwrap();
    let back = store.read_by_id(id, ReadWindow::full()).unwrap();
    let back = back.as_persistent().expect("reads back as persistent");
    assert_eq!(back.timestamps, original.timestamps);
    assert_eq!(
        back.data.to_vec::<f64>().unwrap(),
        original.data.to_vec::<f64>().unwrap()
    );
    // The descriptors survive, including the application payload the consumer's
    // expansion policy rides in.
    assert_eq!(back.units.as_deref(), Some("USD/MMBtu"));
    assert_eq!(back.component_field.as_deref(), Some("fuel_cost"));
    assert_eq!(
        back.application_data.as_deref(),
        Some(r#"{"as_time_series":false,"force_scalar_mode":"midpoint"}"#)
    );
    let row = store.get_metadata_by_id(id).unwrap().unwrap();
    assert_eq!(row.time_series_type, TimeSeriesType::PersistentTimeSeries);
}

#[test]
fn a_range_read_starts_at_the_breakpoint_in_force_not_the_next_one() {
    let mut store = create_store(None, true).unwrap();
    let id = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::PersistentTimeSeries(curve("gas", &[1, 4, 7, 10])),
            Features::new(),
        )
        .unwrap();
    let read_range = |range| {
        store
            .read_by_ids_range(&[id], range)
            .map(|mut all| all.remove(0))
    };

    // A window opening mid-April must still know April's value, so the slice
    // begins at the April breakpoint — one *earlier* than the first breakpoint
    // inside the window. The NonSequentialTimeSeries arm would start at July.
    let start = month(4) + Duration::days(10);
    let data = read_range(TimeRange::new(start, month(9))).unwrap();
    let sliced = data.as_persistent().unwrap();
    assert_eq!(sliced.timestamps, vec![month(4), month(7)]);
    assert_eq!(sliced.data.to_vec::<f64>().unwrap(), vec![40.0, 70.0]);

    // A window entirely inside one step still returns that step, so the caller
    // always gets a series defining a value at `start`.
    let inside = read_range(TimeRange::new(month(5), month(5) + Duration::days(1))).unwrap();
    assert_eq!(inside.as_persistent().unwrap().timestamps, vec![month(4)]);

    // A window opening before the first breakpoint is refused, not clamped.
    let err = read_range(TimeRange::new(month(1) - Duration::days(1), month(6)))
        .unwrap_err()
        .to_string();
    assert!(err.contains("before the first breakpoint"), "{err}");
}

/// A zero-width range selects nothing, as it does for every other type.
///
/// The hold-last rule and the half-open rule pull in opposite directions here:
/// a step function has a value in force at any instant from its first
/// breakpoint on, but `[t, t)` contains no instant for that value to attach to.
/// The half-open rule wins, which keeps `PersistentTimeSeries` consistent with
/// `SingleTimeSeries` — also a step function, whose overlap rule selects
/// nothing because an empty range cannot be overlapped — and with the forecasts
/// (`tests/forecasts.rs`) and `SingleTimeSeries` (`tests/indexing.rs`) that pin
/// the same behavior.
#[test]
fn a_zero_width_range_read_selects_no_breakpoints() {
    let mut store = create_store(None, true).unwrap();
    let id = add(&mut store, 1, curve("gas", &[1, 4, 7, 10]));
    let read_range = |range| {
        store
            .read_by_ids_range(&[id], range)
            .map(|mut all| all.remove(0))
    };
    let empty = |at| {
        let data = read_range(TimeRange::new(at, at))
            .unwrap_or_else(|e| panic!("zero-width range at {at} should select nothing, got {e}"));
        let series = data.as_persistent().unwrap().clone();
        assert!(series.timestamps.is_empty(), "{at}: timestamps");
        assert_eq!(series.data.length(), 0, "{at}: values");
    };

    // Mid-step: the April breakpoint is in force, and a non-empty window would
    // return it — but this window has no instant for it to be in force at.
    empty(month(5));
    // On a breakpoint, where the greatest `<= t` lookup lands exactly.
    empty(month(4));
    // After the last, where hold-last extends indefinitely.
    empty(month(11));
    // Before the first, where a *non-empty* window is an error. An empty one is
    // not: the undefined-before-the-first rule is about an instant the caller
    // asked for, and this caller asked for none.
    empty(month(1) - Duration::days(1));
}

#[test]
fn the_write_path_refuses_a_malformed_persistent_series() {
    let mut store = create_store(None, true).unwrap();
    let mut bad = curve("gas", &[1, 4, 7]);
    bad.timestamps = vec![month(4), month(1), month(7)];
    let err = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::PersistentTimeSeries(bad),
            Features::new(),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("strictly increasing"), "{err}");

    // Sub-millisecond breakpoints are refused for the same reason they are for
    // a NonSequentialTimeSeries: the C ABI and Julia exchange instants as i64
    // unix milliseconds.
    let mut fine = curve("gas", &[1, 4, 7]);
    fine.timestamps[1] += Duration::nanoseconds(1);
    let err = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::PersistentTimeSeries(fine),
            Features::new(),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("millisecond"), "{err}");
}

// ---------------------------------------------------------------------------
// Storage sharing: the "no storage change" claim
// ---------------------------------------------------------------------------

#[test]
fn a_persistent_and_a_non_sequential_series_share_one_stored_array() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let mut store = create_store(Some(path.as_path()), false).unwrap();

    let timestamps: Vec<_> = [1u32, 4, 7].iter().map(|m| month(*m)).collect();
    let values = [10.0f64, 40.0, 70.0];
    let persistent = PersistentTimeSeries::new(
        timestamps.clone(),
        TypedArray::from_f64(vec![3], &values),
        "shared",
    )
    .unwrap();
    let irregular =
        NonSequentialTimeSeries::new(timestamps, TypedArray::from_f64(vec![3], &values), "shared")
            .unwrap();

    let p_id = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::PersistentTimeSeries(persistent),
            Features::new(),
        )
        .unwrap();
    let n_id = store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(irregular),
            Features::new(),
        )
        .unwrap();
    // Distinct associations (the type is part of the identity)...
    assert_ne!(p_id, n_id);

    // ...over one content-addressed array. `PackGroup` is keyed by the time
    // axis, never by the series type, so identical bytes on identical
    // breakpoints dedup across the two types. This pins the plan's claim that
    // the storage layer needed no change at all.
    let rows = store.list_metadata(ListFilter::new()).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].data_hash, rows[1].data_hash);
    assert_eq!(store.num_distinct_arrays().unwrap(), 1);
}

// ---------------------------------------------------------------------------
// The projection read
// ---------------------------------------------------------------------------

/// Hold-last over a month curve, computed without the store. The reader tests
/// below reuse it; here it is the reference `project_onto` is checked against.
fn hold_last(months: &[u32], at: DateTime<Utc>) -> f64 {
    months
        .iter()
        .rev()
        .find(|m| month(**m) <= at)
        .map(|m| *m as f64 * 10.0)
        .expect("the sweep never asks before the first breakpoint")
}

#[test]
fn project_onto_agrees_with_hold_last_at_every_instant() {
    let months = [1, 4, 7, 10];
    let series = curve("gas_price", &months);

    // A swept grid: every breakpoint, every midpoint, and the millisecond on
    // each side of every breakpoint -- the boundary is where a step function
    // gets got wrong.
    let mut at = Vec::new();
    for day in 1..=365 {
        at.push(month(1) + Duration::days(day - 1));
    }
    for m in months {
        at.push(month(m));
        at.push(month(m) + Duration::milliseconds(1));
        if m > 1 {
            at.push(month(m) - Duration::milliseconds(1));
        }
    }
    // Far past the last breakpoint: held forward forever, not an error.
    at.push(month(10) + Duration::days(4000));

    let projected = series.project_onto(&at).unwrap();
    assert_eq!(projected.shape, vec![at.len()]);
    assert_eq!(projected.dtype, series.data.dtype);
    let values = projected.to_vec::<f64>().unwrap();
    for (i, t) in at.iter().enumerate() {
        assert_eq!(values[i], hold_last(&months, *t), "at {t}");
    }
}

#[test]
fn project_onto_is_a_gather_not_a_slice() {
    let series = curve("gas_price", &[1, 4, 7, 10]);
    // Unsorted, with a repeat. Each instant resolves independently and the
    // caller's order is the output order -- nothing is sorted or deduplicated.
    let at = [month(9), month(1), month(9), month(5)];
    let values = series.project_onto(&at).unwrap().to_vec::<f64>().unwrap();
    assert_eq!(values, vec![70.0, 10.0, 70.0, 40.0]);
}

#[test]
fn projecting_onto_no_instants_yields_an_empty_array() {
    // Consistent with a zero-width range selecting nothing, and it keeps the
    // element shape so a caller decodes the result without a special case.
    let series = curve("gas_price", &[1, 4]);
    let empty = series.project_onto(&[]).unwrap();
    assert_eq!(empty.shape, vec![0]);
    assert_eq!(empty.dtype, series.data.dtype);
    assert!(empty.bytes.is_empty());

    // The same for a multi-element series: `[0, 2]`, not `[0]`.
    let curve_2d = PersistentTimeSeries::new(
        vec![month(1), month(4)],
        TypedArray::from_f64(vec![2, 2], &[1.0, 2.0, 3.0, 4.0]),
        "pairs",
    )
    .unwrap();
    assert_eq!(curve_2d.project_onto(&[]).unwrap().shape, vec![0, 2]);
}

#[test]
fn one_instant_before_the_first_breakpoint_fails_the_whole_projection() {
    let series = curve("gas_price", &[1, 4, 7]);
    // The bad instant is last, after three that resolve fine: the call still
    // fails outright, and every index is resolved before a byte is copied, so
    // no partial answer exists to be mistaken for a whole one.
    let at = [
        month(2),
        month(5),
        month(8),
        month(1) - Duration::milliseconds(1),
    ];
    let err = series.project_onto(&at).unwrap_err();
    assert!(
        err.contains("before the first breakpoint"),
        "the message should say why: {err}"
    );
}

#[test]
fn a_composite_element_row_projects_whole() {
    // A `PiecewiseLinear` row is `[n, x1, y1, ..., xn, yn]`, zero-padded to a
    // fixed width. The projection copies rows, so the padding rides along and
    // the result decodes exactly as `data` does -- which is what a fuel cost
    // curve stored as a step function needs.
    let rows = vec![
        2.0, 10.0, 100.0, 20.0, 210.0, // January: two points
        2.0, 12.0, 130.0, 22.0, 250.0, // July: two points, different values
    ];
    let series = PersistentTimeSeries::new(
        vec![month(1), month(7)],
        TypedArray::from_f64(vec![2, 5], &rows),
        "fuel_cost_curve",
    )
    .unwrap()
    .with_element_type(ElementType::PiecewiseLinear);

    let projected = series
        .project_onto(&[month(3), month(9), month(1)])
        .unwrap();
    assert_eq!(projected.shape, vec![3, 5]);
    assert_eq!(
        projected.to_vec::<f64>().unwrap(),
        vec![
            2.0, 10.0, 100.0, 20.0, 210.0, // March holds January's row
            2.0, 12.0, 130.0, 22.0, 250.0, // September holds July's
            2.0, 10.0, 100.0, 20.0, 210.0, // and January is itself
        ]
    );
    // The element type is unchanged by a projection, so the result is decoded
    // the same way the stored array is.
    assert_eq!(series.element_type, ElementType::PiecewiseLinear);
}

#[test]
fn read_projected_evaluates_a_stored_curve() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = create_store(Some(dir.path().join("store.h5").as_path()), false).unwrap();
    let months = [1, 4, 7, 10];
    let id = add(&mut store, 7, curve("gas_price", &months));

    let at: Vec<_> = (1..=12).map(month).collect();
    let projected = store.read_projected(id, Instants::zoned(&at)).unwrap();
    assert_eq!(projected.shape, vec![12]);
    assert_eq!(
        projected.to_vec::<f64>().unwrap(),
        at.iter()
            .map(|t| hold_last(&months, *t))
            .collect::<Vec<_>>()
    );

    // An id naming no row is a stale reference, as it is for every other read.
    assert!(matches!(
        store.read_projected(TimeSeriesId(9999), Instants::zoned(&at)),
        Err(infrastore_core::TimeSeriesError::NotFound)
    ));
}

#[test]
fn read_projected_by_ids_keeps_each_curve_on_its_own_breakpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = create_store(Some(dir.path().join("store.h5").as_path()), false).unwrap();
    // Deliberately misaligned. A projection is per-series, so a cohort that
    // shares no timeline is still one call -- the persistent type's whole point.
    let quarterly = [1, 4, 7, 10];
    let semi = [1, 6];
    let ids = vec![
        add(&mut store, 1, curve("quarterly", &quarterly)),
        add(&mut store, 2, curve("semi", &semi)),
    ];

    let at = [month(3), month(8)];
    let out = store
        .read_projected_by_ids(&ids, Instants::zoned(&at))
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].to_vec::<f64>().unwrap(), vec![10.0, 70.0]);
    assert_eq!(out[1].to_vec::<f64>().unwrap(), vec![10.0, 60.0]);
}

#[test]
fn a_projection_over_any_other_type_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = create_store(Some(dir.path().join("store.h5").as_path()), false).unwrap();
    // The same breakpoints and values, filed as the type that has no value
    // between them.
    let nsts = NonSequentialTimeSeries::new(
        vec![month(1), month(4)],
        TypedArray::from_f64(vec![2], &[10.0, 40.0]),
        "irregular",
    )
    .unwrap();
    let id = store
        .add_time_series(
            5,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(nsts),
            Features::new(),
        )
        .unwrap();

    let err = store
        .read_projected(id, Instants::zoned(&[month(2)]))
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("NonSequentialTimeSeries") && message.contains("resampling policy"),
        "the error should name the actual type and say why: {message}"
    );
}

#[test]
fn a_projection_bound_must_be_spelled_the_way_the_series_is() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = create_store(Some(dir.path().join("store.h5").as_path()), false).unwrap();
    let mut zoneless = curve("wall_clock", &[1, 4]);
    zoneless.time_reference = Some(infrastore_core::TimeReference::Zoneless);
    let zoneless_id = add(&mut store, 1, zoneless);
    let mut zoned = curve("instants", &[1, 4]);
    zoned.time_reference = Some(infrastore_core::TimeReference::Utc);
    let zoned_id = add(&mut store, 2, zoned);

    let at = [month(2)];
    // Instants against a zoneless series, and wall clocks against one that
    // records instants: both are category errors, exactly as on a ranged read.
    assert!(
        store
            .read_projected(zoneless_id, Instants::zoned(&at))
            .is_err()
    );
    assert!(
        store
            .read_projected(zoned_id, Instants::zoneless(&at))
            .is_err()
    );
    // Each spelled its own way answers.
    assert!(
        store
            .read_projected(zoneless_id, Instants::zoneless(&at))
            .is_ok()
    );
    assert!(store.read_projected(zoned_id, Instants::zoned(&at)).is_ok());

    // An empty vector names no bound, so there is nothing to spell wrongly:
    // it answers with an empty array either way rather than a category error.
    assert_eq!(
        store
            .read_projected(zoneless_id, Instants::zoned(&[]))
            .unwrap()
            .shape,
        vec![0]
    );

    // One vector cannot serve both coherence groups at once.
    let err = store
        .read_projected_by_ids(&[zoneless_id, zoned_id], Instants::zoned(&at))
        .unwrap_err();
    assert!(
        err.to_string().contains("mixes zoneless"),
        "the error should name the mixed selection: {err}"
    );
}

// ---------------------------------------------------------------------------
// The per-column-breakpoint StaticReader
// ---------------------------------------------------------------------------

/// Hold-last, computed independently of the store, for a column to be checked
/// against.
fn expected(months: &[u32], at: DateTime<Utc>) -> Option<f64> {
    months
        .iter()
        .rev()
        .find(|m| month(**m) <= at)
        .map(|m| *m as f64 * 10.0)
}

#[test]
fn a_reader_resolves_each_column_on_its_own_breakpoints() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let mut store = create_store(Some(path.as_path()), false).unwrap();

    // Deliberately misaligned: monthly, quarterly, and a single breakpoint. No
    // two share an axis except by accident, which is the whole point.
    let monthly: Vec<u32> = (1..=12).collect();
    let quarterly = vec![1u32, 4, 7, 10];
    let one = vec![1u32];
    add(&mut store, 1, curve("monthly", &monthly));
    add(&mut store, 2, curve("quarterly", &quarterly));
    add(&mut store, 3, curve("one", &one));
    store.flush().unwrap();

    let mut reader = store
        .build_static_reader(
            ListFilter::new().time_series_type(TimeSeriesType::PersistentTimeSeries),
        )
        .unwrap();
    assert_eq!(
        reader.time_series_type(),
        TimeSeriesType::PersistentTimeSeries
    );
    // A step function has no constant step, so there is no resolution to report.
    assert_eq!(reader.resolution(), None);
    // The public axis is the union: every instant at which *something* changes.
    // Here that is the monthly vector, which subsumes the other two.
    let axis: Vec<_> = reader.timestamps().collect();
    assert_eq!(axis, monthly.iter().map(|m| month(*m)).collect::<Vec<_>>());

    // Column order is deterministic; capture it once.
    let names: Vec<String> = column_names(&store, &reader);
    let vectors: Vec<&[u32]> = names
        .iter()
        .map(|n| match n.as_str() {
            "monthly" => monthly.as_slice(),
            "quarterly" => quarterly.as_slice(),
            "one" => one.as_slice(),
            other => panic!("unexpected column {other}"),
        })
        .collect();

    // Sweep every union instant and compare every column against the
    // independently computed hold-last reference. This is the assertion that
    // catches a `vector_ids` desync or a bad scatter-back in the HDF5 override,
    // both of which produce plausible wrong numbers rather than a failure.
    for at in axis {
        store.static_read(&mut reader, at).unwrap();
        let got: Vec<f64> = reader
            .groups()
            .iter()
            .flat_map(|g| g.values_to_vec::<f64>().unwrap())
            .collect();
        let want: Vec<f64> = vectors
            .iter()
            .map(|v| expected(v, at).expect("every column is defined from January on"))
            .collect();
        assert_eq!(got, want, "at {at}");
    }
}

#[test]
fn columns_sharing_a_vector_are_interned_and_still_read_correctly() {
    let mut store = create_store(None, true).unwrap();
    // Two on one axis, one on another: exercises both the interning and the
    // `(dataset, row)` bucketing in the backend override.
    let shared = vec![1u32, 4, 7, 10];
    let other = vec![1u32, 6];
    add(&mut store, 1, curve("a", &shared));
    add(&mut store, 2, curve("b", &shared));
    add(&mut store, 3, curve("c", &other));

    let mut reader = store
        .build_static_reader(
            ListFilter::new().time_series_type(TimeSeriesType::PersistentTimeSeries),
        )
        .unwrap();
    // Union of {1,4,7,10} and {1,6}.
    let axis: Vec<_> = reader.timestamps().collect();
    assert_eq!(
        axis,
        [1u32, 4, 6, 7, 10]
            .iter()
            .map(|m| month(*m))
            .collect::<Vec<_>>()
    );

    let names: Vec<String> = column_names(&store, &reader);
    for at in axis {
        store.static_read(&mut reader, at).unwrap();
        let got: Vec<f64> = reader
            .groups()
            .iter()
            .flat_map(|g| g.values_to_vec::<f64>().unwrap())
            .collect();
        for (name, value) in names.iter().zip(&got) {
            let months: &[u32] = if name == "c" { &other } else { &shared };
            assert_eq!(Some(*value), expected(months, at), "column {name} at {at}");
        }
    }
}

#[test]
fn vector_ids_stay_parallel_through_the_group_sort() {
    let mut store = create_store(None, true).unwrap();
    // Mixed dtypes and element shapes force several groups, and `build_groups`
    // sorts rows to make column order stable. The vector ids must ride along.
    let f64_curve = curve("z_f64", &[1, 7]);
    let i64_curve = {
        let timestamps = vec![month(1), month(3), month(5)];
        PersistentTimeSeries::new(
            timestamps,
            TypedArray::from_slice(vec![3], &[100i64, 300, 500]).unwrap(),
            "a_i64",
        )
        .unwrap()
    };
    let tuple_curve = {
        let timestamps = vec![month(2), month(9)];
        PersistentTimeSeries::new(
            timestamps,
            TypedArray::from_f64(vec![2, 2], &[1.0, 2.0, 3.0, 4.0]),
            "m_tuple",
        )
        .unwrap()
    };
    add(&mut store, 1, f64_curve);
    add(&mut store, 2, i64_curve);
    add(&mut store, 3, tuple_curve);

    let mut reader = store
        .build_static_reader(
            ListFilter::new().time_series_type(TimeSeriesType::PersistentTimeSeries),
        )
        .unwrap();
    // Sorted by element type, then element shape, then identity — note that
    // this puts the two f64 groups together and the i64 group last, regardless
    // of the names. Pinned exactly, because column order is the contract a
    // caller reads the buffers against.
    let layout: Vec<(String, Vec<String>)> = reader
        .groups()
        .iter()
        .map(|g| {
            let names = g
                .ids()
                .iter()
                .map(|id| store.get_metadata_by_id(*id).unwrap().unwrap().name)
                .collect();
            (
                format!("{:?}{:?}", g.dtype().as_str(), g.element_shape()),
                names,
            )
        })
        .collect();
    assert_eq!(
        layout,
        vec![
            ("\"f64\"[]".to_string(), vec!["z_f64".to_string()]),
            ("\"f64\"[2]".to_string(), vec!["m_tuple".to_string()]),
            ("\"i64\"[]".to_string(), vec!["a_i64".to_string()]),
        ]
    );

    // Every column is defined only from month 2 on (the tuple column starts
    // there), so sweep from there.
    for at in [month(2), month(3), month(5), month(9), month(12)] {
        store.static_read(&mut reader, at).unwrap();
        assert_eq!(
            reader.groups()[0].values_to_vec::<f64>().unwrap(),
            vec![expected(&[1, 7], at).unwrap()],
            "f64 column at {at}"
        );
        assert_eq!(
            reader.groups()[2].values_to_vec::<i64>().unwrap(),
            vec![expected(&[1, 3, 5], at).unwrap() as i64 * 10],
            "i64 column at {at}"
        );
        let tuple = reader.groups()[1].values_to_vec::<f64>().unwrap();
        let want = if at < month(9) {
            vec![1.0, 2.0]
        } else {
            vec![3.0, 4.0]
        };
        assert_eq!(tuple, want, "tuple column at {at}");
    }
}

#[test]
fn reading_before_a_column_s_first_breakpoint_names_that_column() {
    let mut store = create_store(None, true).unwrap();
    add(&mut store, 1, curve("early", &[1, 6]));
    let late = add(&mut store, 2, curve("late", &[9]));

    let mut reader = store
        .build_static_reader(
            ListFilter::new().time_series_type(TimeSeriesType::PersistentTimeSeries),
        )
        .unwrap();
    // The union starts in January, but `late` has nothing before September.
    let err = store
        .static_read(&mut reader, month(1))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&late.to_string()),
        "the error must name the column, by the id a caller can look it up with: {err}"
    );
    assert!(err.contains("before the first breakpoint"), "{err}");

    // From September on, every column resolves.
    store.static_read(&mut reader, month(9)).unwrap();
}

#[test]
fn a_reader_refuses_to_mix_persistent_with_non_sequential_columns() {
    let mut store = create_store(None, true).unwrap();
    let timestamps = vec![month(1), month(6)];
    add(&mut store, 1, curve("step", &[1, 6]));
    store
        .add_time_series(
            2,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::NonSequentialTimeSeries(
                NonSequentialTimeSeries::new(
                    timestamps,
                    TypedArray::from_f64(vec![2], &[1.0, 2.0]),
                    "events",
                )
                .unwrap(),
            ),
            Features::new(),
        )
        .unwrap();

    // Each reader sees only its own type — the filter is by type, so this is
    // really asserting that the two do not silently pool into one reader
    // despite sharing a storage cohort.
    let persistent = store
        .build_static_reader(
            ListFilter::new().time_series_type(TimeSeriesType::PersistentTimeSeries),
        )
        .unwrap();
    assert_eq!(
        persistent
            .groups()
            .iter()
            .map(|g| g.num_columns())
            .sum::<usize>(),
        1
    );
    let irregular = store
        .build_static_reader(
            ListFilter::new().time_series_type(TimeSeriesType::NonSequentialTimeSeries),
        )
        .unwrap();
    assert_eq!(
        irregular
            .groups()
            .iter()
            .map(|g| g.num_columns())
            .sum::<usize>(),
        1
    );
}

#[test]
fn a_persistent_reader_takes_no_resolution_filter() {
    let mut store = create_store(None, true).unwrap();
    add(&mut store, 1, curve("gas", &[1, 6]));
    let err = store
        .build_static_reader(
            ListFilter::new()
                .time_series_type(TimeSeriesType::PersistentTimeSeries)
                .resolution(Duration::hours(1)),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("no resolution filter"), "{err}");
}

#[test]
fn an_empty_persistent_selection_is_an_error_not_an_empty_reader() {
    let store = create_store(None, true).unwrap();
    let err = store
        .build_static_reader(
            ListFilter::new().time_series_type(TimeSeriesType::PersistentTimeSeries),
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no PersistentTimeSeries match the filter"),
        "{err}"
    );
}
