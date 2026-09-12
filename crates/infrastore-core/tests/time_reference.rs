//! How a series' timestamps were *spelled*, end to end.
//!
//! The store records instants; a [`TimeReference`] records what those instants
//! were written as. These tests hold the line the whole feature rests on: a
//! spelling the store accepts is a spelling it can hand back, and everything
//! that would let a spelling change *which* instants a series contains is
//! refused rather than guessed at.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, Deterministic, ListFilter, NonSequentialTimeSeries, OwnerCategory, Period,
    SingleTimeSeries, Store, TimeRange, TimeReference, TimeSeriesData, TimeSeriesError,
    TimeSeriesType, TypedArray,
};

mod common;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
}

fn sts(name: &str, length: usize) -> SingleTimeSeries {
    let values: Vec<f64> = (0..length).map(|i| i as f64).collect();
    SingleTimeSeries::new(
        t0(),
        Duration::hours(1),
        TypedArray::from_f64(vec![length], &values),
        name,
    )
}

fn add(
    store: &mut infrastore_core::Store,
    owner: i64,
    data: TimeSeriesData,
) -> infrastore_core::TimeSeriesId {
    store
        .add(AddRequest::new(
            owner,
            "Generator",
            OwnerCategory::Component,
            data,
        ))
        .unwrap()
}

/// Every spelling survives the catalog, a listing, and a reopen.
///
/// The array is untouched by all of this — two series holding equal values pool
/// into the same dataset and share a `data_hash` whatever their references say,
/// because they are the same numbers. Only the label differs.
#[test]
fn every_spelling_round_trips_through_the_catalog() {
    let references = [
        TimeReference::Utc,
        TimeReference::Zoneless,
        TimeReference::FixedOffset(-420),
        TimeReference::FixedOffset(0),
        TimeReference::Zone("America/Denver".into()),
        TimeReference::Zone("UTC".into()),
    ];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let mut hashes = Vec::new();
    {
        let mut store = Store::create(Some(path.as_path()), false).unwrap();
        for (i, reference) in references.iter().enumerate() {
            let key = add(
                &mut store,
                i as i64 + 1,
                TimeSeriesData::SingleTimeSeries(
                    sts("load", 4).with_time_reference(reference.clone()),
                ),
            );
            hashes.push(store.get_metadata_by_id(key).unwrap().unwrap().data_hash);
        }
        store.flush().unwrap();
    }
    let store = Store::open(path.as_path(), true).unwrap();
    for (i, reference) in references.iter().enumerate() {
        // Ids are stable across a reopen, and they were minted in owner order.
        let key = store
            .list_metadata(ListFilter::new().owner_id(i as i64 + 1))
            .unwrap()[0]
            .id
            .expect("a stored row carries its id");
        let data = store
            .read_by_id(key, infrastore_core::ReadWindow::full())
            .unwrap();
        assert_eq!(
            data.time_reference(),
            Some(reference),
            "the series' own reference"
        );
        assert_eq!(
            store
                .get_metadata_by_id(key)
                .unwrap()
                .unwrap()
                .time_reference
                .as_ref(),
            Some(reference),
            "the catalog row's reference"
        );
        let listed = store
            .list_metadata(ListFilter::new().owner_id(i as i64 + 1))
            .unwrap();
        assert_eq!(
            listed[0].time_reference.as_ref(),
            Some(reference),
            "the listing's reference"
        );
    }
    // Identical values, identical array -- the reference reaches neither the
    // hash domain nor the dataset layout.
    assert!(
        hashes.windows(2).all(|w| w[0] == w[1]),
        "equal values must share one array whatever their spelling"
    );
}

/// A reference is outside identity, so two series differing only in it are a
/// duplicate — the rule `units` already states, with no new mechanism.
#[test]
fn two_series_differing_only_in_their_reference_are_a_duplicate() {
    let mut store = Store::create(None, true).unwrap();
    add(
        &mut store,
        1,
        TimeSeriesData::SingleTimeSeries(sts("load", 4).with_time_reference(TimeReference::Utc)),
    );
    let err = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(
                sts("load", 4).with_time_reference(TimeReference::Zoneless),
            ),
        ))
        .unwrap_err();
    assert!(matches!(err, TimeSeriesError::DuplicateTimeSeries), "{err}");
}

/// Shape validation runs on the write path, so the native Rust caller — which
/// has no binding to catch a hand-built reference — is covered too.
#[test]
fn a_malformed_zone_name_is_refused_at_the_door() {
    let mut store = Store::create(None, true).unwrap();
    for bad in ["utc", "zoneless", "-07:00", "", "America/Den ver"] {
        let err = store
            .add(AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(
                    sts("load", 4).with_time_reference(TimeReference::Zone(bad.into())),
                ),
            ))
            .unwrap_err();
        assert!(
            matches!(err, TimeSeriesError::InvalidParameter(_)),
            "{bad:?} should be refused as a zone name, got {err}"
        );
    }
    // Existence is audited elsewhere, never gated here: a shape-valid name this
    // build has never heard of is stored, because gating would refuse
    // legitimate data whenever IANA moves ahead of our release.
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(
                sts("load", 4).with_time_reference(TimeReference::Zone("America/Dever".into())),
            ),
        ))
        .expect("a shape-valid zone name is stored without a database to check it against");
}

/// A bound has to be spelled the way the series is, and a mismatch is a
/// category error rather than a rounding one.
#[test]
fn a_query_bound_must_match_the_series_spelling() {
    let mut store = Store::create(None, true).unwrap();
    let zoned = add(
        &mut store,
        1,
        TimeSeriesData::SingleTimeSeries(
            sts("load", 8).with_time_reference(TimeReference::Zone("America/Denver".into())),
        ),
    );
    let wall = add(
        &mut store,
        2,
        TimeSeriesData::SingleTimeSeries(
            sts("load", 8).with_time_reference(TimeReference::Zoneless),
        ),
    );
    let unspecified = add(
        &mut store,
        3,
        TimeSeriesData::SingleTimeSeries(sts("load", 8)),
    );

    let window = (t0() + Duration::hours(2), t0() + Duration::hours(4));
    let zoned_bound = TimeRange::new(window.0, window.1);
    let wall_bound = TimeRange::zoneless(window.0, window.1);

    // An aware bound need not match the series' own offset: slicing is instant
    // arithmetic, and any offset names the same instant.
    for key in [zoned, unspecified] {
        let sliced = store
            .read_by_ids_range(&[key], zoned_bound)
            .map(|mut v| v.remove(0))
            .unwrap();
        assert_eq!(sliced.as_single().unwrap().length, 2);
    }
    let sliced = store
        .read_by_ids_range(&[wall], wall_bound)
        .map(|mut v| v.remove(0))
        .unwrap();
    assert_eq!(sliced.as_single().unwrap().length, 2);

    // The two mismatches, in both directions. `None` groups with the zoned
    // variants -- it is not a floating third case.
    for (key, bound, label) in [
        (zoned, wall_bound, "wall clock vs instants"),
        (unspecified, wall_bound, "wall clock vs unspecified"),
        (wall, zoned_bound, "instant vs wall clocks"),
    ] {
        let err = store
            .read_by_ids_range(&[key], bound)
            .map(|mut v| v.remove(0))
            .unwrap_err();
        assert!(
            matches!(err, TimeSeriesError::InvalidParameter(_)),
            "{label}: expected a refusal, got {err}"
        );
    }
}

/// One bound, or one shared timestamp axis, cannot serve both coherence
/// groups — and the refusal names them.
#[test]
fn a_selection_cannot_span_both_coherence_groups() {
    let mut store = Store::create(None, true).unwrap();
    let zoned = add(
        &mut store,
        1,
        TimeSeriesData::SingleTimeSeries(sts("load", 8).with_time_reference(TimeReference::Utc)),
    );
    let wall = add(
        &mut store,
        2,
        TimeSeriesData::SingleTimeSeries(
            sts("load", 8).with_time_reference(TimeReference::Zoneless),
        ),
    );

    // Unranged, there is nothing for the two groups to disagree about: each
    // series carries its own spelling back.
    assert_eq!(
        store
            .read_by_ids(&[zoned, wall], infrastore_core::ReadWindow::full())
            .unwrap()
            .len(),
        2
    );

    // Ranged, no single bound is valid for both.
    let err = store
        .read_by_ids_range(
            &[zoned, wall],
            TimeRange::new(t0(), t0() + Duration::hours(4)),
        )
        .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("zoneless"), "{message}");
    assert!(
        message.contains("ListFilter::zoneless"),
        "the remedy is named: {message}"
    );

    // A reader materializes one axis, so a mixed cohort is refused at *build*
    // time, where the error can say which series disagree.
    let err = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap_err();
    assert!(err.to_string().contains("zoneless"), "{err}");

    // The constructive half: a coherent selection is buildable, not just
    // describable.
    for (zoneless, expected) in [(true, 1), (false, 1)] {
        let filter = ListFilter::new()
            .resolution(Duration::hours(1))
            .zoneless(zoneless);
        let reader = store.build_static_reader(filter).unwrap();
        assert_eq!(reader.groups()[0].num_columns(), expected);
        assert_eq!(
            reader
                .time_reference()
                .map(TimeReference::as_storage_string),
            Some(if zoneless { "zoneless" } else { "utc" }.to_string()),
        );
    }
}

/// `Some(false)` selects everything that accepts a zoned bound — the three
/// zoned spellings *and* the rows that left the reference unset.
///
/// That second group is why this is a binary predicate rather than a match on a
/// specific spelling: an exact-match filter cannot name the rows that declared
/// nothing, and here they are a coherence group rather than an oversight.
#[test]
fn the_zoneless_filter_puts_unset_rows_with_the_zoned_ones() {
    let mut store = Store::create(None, true).unwrap();
    for (owner, reference) in [
        (1, Some(TimeReference::Utc)),
        (2, Some(TimeReference::Zone("America/Denver".into()))),
        (3, Some(TimeReference::FixedOffset(-420))),
        (4, Some(TimeReference::Zoneless)),
        (5, None),
    ] {
        let mut series = sts("load", 4);
        series.time_reference = reference;
        add(&mut store, owner, TimeSeriesData::SingleTimeSeries(series));
    }
    let owners = |zoneless: bool| {
        let mut ids: Vec<i64> = store
            .list_metadata(ListFilter::new().zoneless(zoneless))
            .unwrap()
            .iter()
            .map(|m| m.owner_id)
            .collect();
        ids.sort_unstable();
        ids
    };
    assert_eq!(owners(true), vec![4]);
    assert_eq!(
        owners(false),
        vec![1, 2, 3, 5],
        "the unset row groups with the zoned ones"
    );
}

/// Mixing the three zoned spellings in one reader is fine — all three name
/// instants, and an axis of instants is what a reader materializes. The axis is
/// reported as the spelling that is true of all of them.
#[test]
fn a_reader_over_mixed_zoned_spellings_reports_the_shared_truth() {
    let mut store = Store::create(None, true).unwrap();
    for (owner, reference) in [
        (1, TimeReference::Utc),
        (2, TimeReference::Zone("America/Denver".into())),
        (3, TimeReference::FixedOffset(-420)),
    ] {
        add(
            &mut store,
            owner,
            TimeSeriesData::SingleTimeSeries(sts("load", 4).with_time_reference(reference)),
        );
    }
    let reader = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    assert_eq!(reader.groups()[0].num_columns(), 3);
    assert_eq!(reader.time_reference(), Some(&TimeReference::Utc));

    // A cohort that agrees exactly reports what it agrees on.
    let mut store = Store::create(None, true).unwrap();
    for owner in 1..=2 {
        add(
            &mut store,
            owner,
            TimeSeriesData::SingleTimeSeries(
                sts("load", 4).with_time_reference(TimeReference::Zone("America/Denver".into())),
            ),
        );
    }
    let reader = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap();
    assert_eq!(
        reader.time_reference(),
        Some(&TimeReference::Zone("America/Denver".into()))
    );
}

/// A reference is a spelling, not a grid. A calendar period steps on the
/// stored UTC calendar and the reference does not redirect it.
///
/// Spelled here at a **fixed offset**, which is where this is still storable. A
/// fixed offset has no DST to drift against, so the only disagreement is at a
/// month boundary, and that is warned about rather than refused. The same period
/// on a *named zone* is refused outright — see
/// `a_calendar_scale_period_on_a_named_zone_is_refused`.
#[test]
fn a_calendar_period_on_an_offset_series_still_steps_on_the_utc_calendar() {
    let mut store = Store::create(None, true).unwrap();
    let months = Period::months(1);
    let series = SingleTimeSeries::new(
        t0(),
        months,
        TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
        "load",
    )
    .with_time_reference(TimeReference::FixedOffset(-420));
    let key = add(&mut store, 1, TimeSeriesData::SingleTimeSeries(series));

    // The grid is the stored UTC calendar: the reference does not redirect it,
    // because that would let a spelling decide which instants the series holds.
    let sliced = store
        .read_by_ids_range(
            &[key],
            TimeRange::new(
                months.add_to(t0(), 1).unwrap(),
                months.add_to(t0(), 2).unwrap(),
            ),
        )
        .map(|mut v| v.remove(0))
        .unwrap();
    assert_eq!(
        sliced.as_single().unwrap().initial_timestamp,
        Utc.with_ymd_and_hms(2030, 2, 1, 0, 0, 0).unwrap()
    );
}

/// Irregular series and dense forecasts carry the reference too — it rides on
/// `TimeSeriesData`, so every variant gets it for free.
#[test]
fn every_series_type_carries_its_reference() {
    let mut store = Store::create(None, true).unwrap();
    let stamps = vec![t0(), t0() + Duration::hours(3), t0() + Duration::hours(7)];
    let irregular = NonSequentialTimeSeries::new(
        stamps.clone(),
        TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]),
        "events",
    )
    .unwrap()
    .with_time_reference(TimeReference::Zoneless);
    let key = add(
        &mut store,
        1,
        TimeSeriesData::NonSequentialTimeSeries(irregular),
    );
    let read = store
        .read_by_id(key, infrastore_core::ReadWindow::full())
        .unwrap();
    assert_eq!(read.time_reference(), Some(&TimeReference::Zoneless));
    assert_eq!(read.as_non_sequential().unwrap().timestamps, stamps);

    let forecast = Deterministic::new(
        t0(),
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        3,
        TypedArray::from_f64(vec![2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
        "forecast",
    )
    .unwrap()
    .with_time_reference(TimeReference::FixedOffset(330));
    let key = add(&mut store, 2, TimeSeriesData::Deterministic(forecast));
    assert_eq!(
        store
            .read_by_id(key, infrastore_core::ReadWindow::full())
            .unwrap()
            .time_reference(),
        Some(&TimeReference::FixedOffset(330))
    );
    assert_eq!(
        store
            .get_metadata_by_id(key)
            .unwrap()
            .unwrap()
            .time_reference,
        Some(TimeReference::FixedOffset(330))
    );

    // A step function's breakpoints are spelled the same way, and the reference
    // gates its query bounds like every other type's.
    let step = infrastore_core::PersistentTimeSeries::new(
        stamps.clone(),
        TypedArray::from_f64(vec![3], &[7.0, 8.0, 9.0]),
        "gas_price",
    )
    .unwrap()
    .with_time_reference(TimeReference::Zoneless);
    let key = add(&mut store, 3, TimeSeriesData::PersistentTimeSeries(step));
    let read = store
        .read_by_id(key, infrastore_core::ReadWindow::full())
        .unwrap();
    assert_eq!(read.time_reference(), Some(&TimeReference::Zoneless));
    assert_eq!(read.as_persistent().unwrap().timestamps, stamps);
    // A zoned bound against a zoneless series is refused, not coerced --
    // the rule is the type-independent one in `TimeRange::check_against`.
    assert!(
        store
            .read_by_ids_range(
                &[key],
                infrastore_core::TimeRange::new(t0(), t0() + Duration::hours(8))
            )
            .is_err()
    );
}

/// A mixed cohort is refused for either reader — and the refusal names the one
/// the caller actually asked for.
///
/// Both readers share the coherence check, so its message has to name the
/// reader being built. A caller who built a *forecast* reader and is told a
/// `StaticReader` failed is pointed at an API they never invoked, and goes
/// looking in the wrong place for a filter they did not write.
#[test]
fn the_cohort_refusal_names_the_reader_that_was_asked_for() {
    let mut store = Store::create(None, true).unwrap();

    let det = |name: &str, reference: TimeReference| {
        TimeSeriesData::Deterministic(
            Deterministic::new(
                t0(),
                Duration::hours(1),
                Duration::hours(2),
                Duration::hours(1),
                3,
                TypedArray::from_f64(vec![2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
                name,
            )
            .unwrap()
            .with_time_reference(reference),
        )
    };
    add(&mut store, 1, det("fc", TimeReference::Utc));
    add(&mut store, 2, det("fc", TimeReference::Zoneless));

    let err = store
        .build_forecast_reader(
            ListFilter::new()
                .resolution(Duration::hours(1))
                .time_series_type(TimeSeriesType::Deterministic),
        )
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("ForecastReader requires one spelling"),
        "the failing API is named: {message}"
    );
    assert!(
        !message.contains("StaticReader"),
        "and the one that did not fail is not: {message}"
    );

    // The static path still names itself.
    let mut store = Store::create(None, true).unwrap();
    add(
        &mut store,
        1,
        TimeSeriesData::SingleTimeSeries(sts("load", 8).with_time_reference(TimeReference::Utc)),
    );
    add(
        &mut store,
        2,
        TimeSeriesData::SingleTimeSeries(
            sts("load", 8).with_time_reference(TimeReference::Zoneless),
        ),
    );
    let err = store
        .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("StaticReader requires one spelling"),
        "{err}"
    );
}

/// A calendar-scale period on a named zone is refused; a sub-daily one is not.
///
/// This is the line the whole "a reference is a spelling, not a grid" rule rests
/// on, and it is not drawn at "is there a zone" — it is drawn at whether an
/// instant grid and a local clock can part company. Below a day they cannot:
/// DST moves the offset, not the length of an hour, so an hourly Denver grid
/// *is* the local clock. At a day and above they do, silently and permanently.
#[test]
fn a_calendar_scale_period_on_a_named_zone_is_refused() {
    let denver = || TimeReference::Zone("America/Denver".to_string());
    let values = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);

    let build = |period: Period, reference: TimeReference| {
        SingleTimeSeries::new(t0(), period, values.clone(), "load").with_time_reference(reference)
    };

    // Sub-daily on a named zone: legal, and must stay so. Refusing it would push
    // callers onto a fixed offset, which is silently wrong for half the year.
    for period in [
        Period::Fixed(Duration::minutes(15)),
        Period::Fixed(Duration::hours(1)),
        Period::Fixed(Duration::hours(6)),
    ] {
        let mut store = Store::create(None, true).unwrap();
        store
            .add(AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(build(period, denver())),
            ))
            .unwrap_or_else(|e| panic!("{period:?} on a named zone must be storable: {e}"));
    }

    // A day or more on a named zone: refused, naming both remedies.
    for period in [
        Period::Fixed(Duration::days(1)),
        Period::Fixed(Duration::days(7)),
        Period::Months(1),
        Period::Months(12),
    ] {
        let mut store = Store::create(None, true).unwrap();
        let err = store
            .add(AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(build(period, denver())),
            ))
            .unwrap_err();
        let message = err.to_string();
        assert!(
            matches!(err, TimeSeriesError::InvalidParameter(_)),
            "{period:?} should be an InvalidParameter: {message}"
        );
        assert!(message.contains("from_timestamps"), "{message}");
        assert!(message.contains("NonSequentialTimeSeries"), "{message}");
    }

    // The spellings that cannot drift are untouched, calendar period or not. A
    // fixed offset has no DST; `Utc` and `Zoneless` step the calendar they are
    // already on.
    for reference in [
        TimeReference::Utc,
        TimeReference::Zoneless,
        TimeReference::FixedOffset(-420),
    ] {
        let mut store = Store::create(None, true).unwrap();
        store
            .add(AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(build(Period::Months(1), reference.clone())),
            ))
            .unwrap_or_else(|e| panic!("{reference:?} with P1M must be storable: {e}"));
    }
}

/// A forecast's *stepping* periods are covered by the same rule -- but its
/// `horizon` is not, because a horizon is a window length that is only ever
/// divided (`H = horizon / resolution`), never added to an instant. A day-ahead
/// forecast is `horizon = P1D`, which must stay legal in any zone.
#[test]
fn a_forecast_on_a_named_zone_is_refused_for_a_calendar_scale_period() {
    let mut store = Store::create(None, true).unwrap();
    let forecast = Deterministic::new(
        t0(),
        Period::Fixed(Duration::hours(12)),
        Period::Fixed(Duration::days(1)), // horizon: a window length, exempt
        Period::Fixed(Duration::days(1)), // interval: a grid step, refused
        2,
        TypedArray::from_f64(vec![2, 2], &[1.0, 2.0, 3.0, 4.0]),
        "day_ahead",
    )
    .unwrap()
    .with_time_reference(TimeReference::Zone("America/Denver".to_string()));
    let err = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::Deterministic(forecast),
        ))
        .unwrap_err();
    assert!(err.to_string().contains("cannot be combined"), "{err}");
}

/// `from_timestamps` is the constructive half of that refusal: it takes the
/// timeline the caller has and proves a period rather than asserting one.
#[test]
fn from_timestamps_compacts_an_hourly_local_grid_and_refuses_a_daily_one() {
    let ms = |h: i64| t0() + Duration::hours(h);
    let values = TypedArray::from_f64(vec![4], &[1.0, 2.0, 3.0, 4.0]);

    // Denver local hours across fall-back are a uniform instant grid.
    let hourly = [ms(0), ms(1), ms(2), ms(3)];
    let series = SingleTimeSeries::from_timestamps(&hourly, values.clone(), "load").unwrap();
    assert_eq!(series.resolution, Period::Fixed(Duration::hours(1)));
    assert_eq!(series.timestamps().collect::<Vec<_>>(), hourly);

    // Denver local midnights across the same transition are not: the third step
    // is 25 hours.
    let daily = [ms(0), ms(24), ms(48), ms(73)];
    let err = SingleTimeSeries::from_timestamps(&daily, values.clone(), "peak").unwrap_err();
    assert!(err.contains("index 3"), "{err}");
    assert!(err.contains("NonSequentialTimeSeries"), "{err}");

    // And a series built this way is storable even where asserting the period
    // would have been refused -- because the instants are now proven, not
    // claimed.
    let mut store = Store::create(None, true).unwrap();
    let proven = SingleTimeSeries::from_timestamps(&hourly, values, "load")
        .unwrap()
        .with_time_reference(TimeReference::Zone("America/Denver".to_string()));
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(proven),
        ))
        .unwrap();
}

/// A `P1D` horizon on a named zone is the canonical day-ahead forecast and must
/// stay storable: a horizon is divided, never stepped.
#[test]
fn a_calendar_scale_horizon_is_exempt_from_the_refusal() {
    let mut store = Store::create(None, true).unwrap();
    let day_ahead = Deterministic::new(
        t0(),
        Period::Fixed(Duration::hours(1)),  // resolution: sub-daily
        Period::Fixed(Duration::days(1)),   // horizon: P1D, a window length
        Period::Fixed(Duration::hours(12)), // interval: sub-daily
        2,
        TypedArray::from_f64(vec![24, 2], &[0.0; 48]),
        "day_ahead",
    )
    .unwrap()
    .with_time_reference(TimeReference::Zone("America/Denver".to_string()));
    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::Deterministic(day_ahead),
        ))
        .expect("a P1D horizon is a window length, not a grid step");
}
