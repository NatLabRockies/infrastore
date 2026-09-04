//! Guards at the store's seams, each pinned to a failure a review found:
//!
//! * an array reaching the write boundary with bytes that disagree with its
//!   shape, or a zero-width element dimension, is refused rather than truncated
//!   or indexed past its end;
//! * a dense forecast whose content hash already belongs to a packed static
//!   column is still readable through the `ForecastReader`;
//! * a `ReadWindow` that names no `start` reads a set mixing zoneless and
//!   instant-bearing series, because it carries no bound to disagree about;
//! * a forecast removed after the reader cached its block is `NotFound`, not
//!   served from the cache;
//! * the `DeterministicSingleTimeSeries` removal guard is per forecast family
//!   (owner, name, resolution, features) *and* per array, so two owners'
//!   byte-identical sources cannot stand in for each other and a view copied
//!   into a family pins only a source it is actually a view of;
//! * a failed read leaves a reader wholly empty, not holding one timestamp's
//!   values in the groups it reached and another's in the groups it did not.
//!
//! Both backends run every case: the write boundary is shared, but the two
//! backends stored a malformed array differently (the on-disk one refused it in
//! one write path and copied a prefix in the other; the in-memory one kept it
//! and panicked on a sliced read).

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::types::array::Dtype;
use infrastore_core::{
    AddRequest, Deterministic, ListFilter, OwnerCategory, ReadWindow, SingleTimeSeries, Store,
    TimeReference, TimeSeriesData, TimeSeriesError, TimeSeriesId, TimeSeriesType, TransformPolicy,
    TypedArray, create_store,
};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
}

/// Run `body` against a fresh writable store on each backend.
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

fn sts(name: &str, data: TypedArray) -> SingleTimeSeries {
    SingleTimeSeries::new(t0(), Duration::hours(1), data, name)
}

fn request(owner: i64, data: TimeSeriesData) -> AddRequest {
    AddRequest::new(owner, "Generator", OwnerCategory::Component, data)
}

fn add(
    store: &mut Store,
    owner: i64,
    data: TimeSeriesData,
) -> Result<TimeSeriesId, TimeSeriesError> {
    store.add(request(owner, data))
}

fn hourly(len: usize) -> TypedArray {
    let vals: Vec<f64> = (0..len).map(|i| i as f64).collect();
    TypedArray::from_f64(vec![len], &vals)
}

/// The id of the one `DeterministicSingleTimeSeries` `owner` holds.
fn dst_of(store: &Store, owner: i64) -> TimeSeriesId {
    let mut filter = ListFilter::new();
    filter.owner_id = Some(owner);
    filter.time_series_type = Some(TimeSeriesType::DeterministicSingleTimeSeries);
    let rows = store.list_metadata(filter).unwrap();
    assert_eq!(rows.len(), 1, "owner {owner} holds one derived forecast");
    rows[0].id.unwrap()
}

fn is_invalid(err: &TimeSeriesError) -> bool {
    matches!(err, TimeSeriesError::InvalidParameter(_))
}

#[test]
fn an_array_whose_bytes_disagree_with_its_shape_is_refused() {
    each_backend(|store, backend| {
        let mut bytes = Vec::new();
        for v in [1.0f64, 2.0, 3.0, 4.0] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let oversize = TypedArray {
            dtype: Dtype::F64,
            shape: vec![2],
            bytes: bytes.clone(),
        };
        let undersize = TypedArray {
            dtype: Dtype::F64,
            shape: vec![8],
            bytes,
        };
        for (label, arr) in [("oversize", oversize), ("undersize", undersize)] {
            let err = add(
                store,
                1,
                TimeSeriesData::SingleTimeSeries(sts("bad", arr.clone())),
            )
            .unwrap_err();
            assert!(is_invalid(&err), "{backend}: {label} single add: {err}");
            let err = store
                .add_time_series_bulk(vec![request(
                    2,
                    TimeSeriesData::SingleTimeSeries(sts("bad_bulk", arr.clone())),
                )])
                .unwrap_err();
            assert!(is_invalid(&err), "{backend}: {label} bulk add: {err}");
            // The dense forecast path validated its geometry through the
            // constructor, which a struct literal bypasses.
            let det = Deterministic {
                data: arr,
                ..Deterministic::new(
                    t0(),
                    Duration::hours(1),
                    Duration::hours(2),
                    Duration::hours(1),
                    1,
                    TypedArray::from_f64(vec![2, 1], &[0.0, 0.0]),
                    "fc",
                )
                .unwrap()
            };
            let err = add(store, 3, TimeSeriesData::Deterministic(det)).unwrap_err();
            assert!(is_invalid(&err), "{backend}: {label} forecast add: {err}");
        }
        assert_eq!(
            store.num_distinct_arrays().unwrap(),
            0,
            "{backend}: nothing written"
        );
        assert!(store.list_metadata(ListFilter::new()).unwrap().is_empty());
    });
}

#[test]
fn a_zero_width_element_dimension_is_refused_but_an_empty_time_axis_is_not() {
    each_backend(|store, backend| {
        let hollow = TypedArray {
            dtype: Dtype::F64,
            shape: vec![24, 0],
            bytes: Vec::new(),
        };
        let err = add(
            store,
            1,
            TimeSeriesData::SingleTimeSeries(sts("hollow", hollow)),
        )
        .unwrap_err();
        assert!(is_invalid(&err), "{backend}: {err}");
        assert!(err.to_string().contains("zero-width"), "{backend}: {err}");

        // A rank-0 array is one scalar with no time axis: `length()` reads it
        // as empty, so the packed path would write nothing and hash the scalar.
        // The element-type validator refuses it for lacking the time axis.
        let scalar = TypedArray {
            dtype: Dtype::F64,
            shape: Vec::new(),
            bytes: 1.0f64.to_le_bytes().to_vec(),
        };
        let err = add(
            store,
            1,
            TimeSeriesData::SingleTimeSeries(sts("scalar", scalar)),
        )
        .unwrap_err();
        assert!(is_invalid(&err), "{backend}: {err}");
        assert!(err.to_string().contains("leading dims"), "{backend}: {err}");

        // A zero-window forecast is a stored fact too: the count axis is a
        // layout axis, not an element dimension.
        let none = Deterministic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            0,
            TypedArray::from_f64(vec![2, 0], &[]),
            "none",
        )
        .unwrap();
        let id = add(store, 3, TimeSeriesData::Deterministic(none))
            .unwrap_or_else(|e| panic!("{backend}: zero-window forecast: {e}"));
        let back = store.read_by_id(id, ReadWindow::full()).unwrap();
        assert_eq!(back.as_deterministic().unwrap().count, 0, "{backend}");

        // A series with no steps is a stored fact, not a malformed one.
        let empty = TypedArray::from_f64(vec![0], &[]);
        let id = add(
            store,
            1,
            TimeSeriesData::SingleTimeSeries(sts("empty", empty)),
        )
        .unwrap();
        let back = store.read_by_id(id, ReadWindow::full()).unwrap();
        assert_eq!(back.as_single().unwrap().length, 0, "{backend}");
    });
}

#[test]
fn a_forecast_sharing_a_packed_column_is_readable_by_the_forecast_reader() {
    each_backend(|store, backend| {
        // `[2, 3]` is both a two-step series of three-vectors and a
        // two-step-horizon forecast of three windows; the hash covers dtype,
        // shape and bytes, so the two share one array.
        let arr = TypedArray::from_f64(vec![2, 3], &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        add(
            store,
            1,
            TimeSeriesData::SingleTimeSeries(sts("vec", arr.clone())),
        )
        .unwrap();
        let det = Deterministic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            3,
            arr,
            "fc",
        )
        .unwrap();
        let id = add(store, 2, TimeSeriesData::Deterministic(det)).unwrap();
        assert_eq!(
            store.num_distinct_arrays().unwrap(),
            1,
            "{backend}: one shared array"
        );

        let mut filter = ListFilter::new();
        filter.time_series_type = Some(TimeSeriesType::Deterministic);
        filter.resolution = Some(Duration::hours(1).into());
        let mut reader = store.build_forecast_reader(filter).unwrap();
        for k in 0..3usize {
            store
                .forecast_read(&mut reader, t0() + Duration::hours(k as i64))
                .unwrap_or_else(|e| panic!("{backend}: window {k}: {e}"));
            let window = reader.entry_slot(0).window_to_vec::<f64>().unwrap();
            // Row-major `[H, count]`: window `k` is column `k`.
            assert_eq!(
                window,
                vec![k as f64, 3.0 + k as f64],
                "{backend}: window {k}"
            );
        }

        // The whole-array read and the range read see the same forecast.
        let whole = store.read_by_id(id, ReadWindow::full()).unwrap();
        assert_eq!(
            whole.as_deterministic().unwrap().data.shape,
            vec![2, 3],
            "{backend}"
        );
        let tail = store
            .read_by_id(
                id,
                ReadWindow::from(t0() + Duration::hours(1)).with_count(2),
            )
            .unwrap();
        assert_eq!(
            tail.as_deterministic().unwrap().data.to_f64_vec().unwrap(),
            vec![1.0, 2.0, 4.0, 5.0],
            "{backend}: windows 1..3"
        );
    });
}

#[test]
fn a_window_naming_no_start_reads_a_mixed_zoning_set() {
    each_backend(|store, backend| {
        let zoneless = sts("wall", hourly(4)).with_time_reference(TimeReference::Zoneless);
        let utc = sts("instant", hourly(4)).with_time_reference(TimeReference::Utc);
        let a = add(store, 1, TimeSeriesData::SingleTimeSeries(zoneless)).unwrap();
        let b = add(store, 2, TimeSeriesData::SingleTimeSeries(utc)).unwrap();

        // No bound is named, so each series is read on its own spelling.
        let got = store
            .read_by_ids(&[a, b], ReadWindow::full().with_len(2))
            .unwrap_or_else(|e| panic!("{backend}: {e}"));
        assert_eq!(got.len(), 2);
        for series in &got {
            assert_eq!(series.as_single().unwrap().length, 2, "{backend}");
        }

        // A named bound has one spelling, and the set cannot agree on it.
        let err = store
            .read_by_ids(&[a, b], ReadWindow::from(t0()).with_len(2))
            .unwrap_err();
        assert!(is_invalid(&err), "{backend}: {err}");
    });
}

#[test]
fn a_removed_forecast_is_not_served_from_the_reader_cache() {
    each_backend(|store, backend| {
        // Ten windows of a two-step horizon fit one cached block, so without
        // the guard the second read would never touch the backend.
        let vals: Vec<f64> = (0..20).map(|i| i as f64).collect();
        let det = Deterministic::new(
            t0(),
            Duration::hours(1),
            Duration::hours(2),
            Duration::hours(1),
            10,
            TypedArray::from_f64(vec![2, 10], &vals),
            "fc",
        )
        .unwrap();
        let id = add(store, 1, TimeSeriesData::Deterministic(det)).unwrap();
        let mut filter = ListFilter::new();
        filter.time_series_type = Some(TimeSeriesType::Deterministic);
        filter.resolution = Some(Duration::hours(1).into());
        let mut reader = store.build_forecast_reader(filter).unwrap();
        store.forecast_read(&mut reader, t0()).unwrap();
        assert_eq!(
            reader.entry_slot(0).window_to_vec::<f64>().unwrap(),
            vec![0.0, 10.0]
        );

        store.remove_by_ids(&[id]).unwrap();
        let err = store
            .forecast_read(&mut reader, t0() + Duration::hours(1))
            .unwrap_err();
        assert!(
            matches!(err, TimeSeriesError::NotFound),
            "{backend}: expected NotFound, got {err}"
        );
        assert!(
            reader.entry_slot(0).window().is_empty(),
            "{backend}: a failed read leaves no window behind"
        );
    });
}

#[test]
fn the_derived_forecast_source_guard_is_per_owner() {
    each_backend(|store, backend| {
        // Two owners, byte-identical sources: one array, two families.
        let s1 = add(
            store,
            1,
            TimeSeriesData::SingleTimeSeries(sts("load", hourly(8))),
        )
        .unwrap();
        let s2 = add(
            store,
            2,
            TimeSeriesData::SingleTimeSeries(sts("load", hourly(8))),
        )
        .unwrap();
        let outcome = store
            .transform_single_time_series(
                Duration::hours(2),
                Duration::hours(1),
                None,
                None,
                TransformPolicy::default(),
            )
            .unwrap();
        assert_eq!(outcome.transformed, 2, "{backend}");

        // Owner 2 drops its view; owner 1 keeps its own.
        store.remove_by_ids(&[dst_of(store, 2)]).unwrap();

        // Owner 1's source still backs owner 1's view, whoever else shares the
        // array; owner 2's source backs nothing and is free to go.
        let err = store.remove_by_ids(&[s1]).unwrap_err();
        assert!(is_invalid(&err), "{backend}: {err}");
        assert!(err.to_string().contains("owner 1"), "{backend}: {err}");
        assert_eq!(store.remove_by_ids(&[s2]).unwrap(), 1, "{backend}");

        // Removing the view and its source together passes in either order.
        assert_eq!(
            store.remove_by_ids(&[s1, dst_of(store, 1)]).unwrap(),
            2,
            "{backend}"
        );
    });
}

/// The guard pins a source only for a view that is actually a view *of it*.
///
/// The family alone is not enough. `copy_time_series` deliberately writes a
/// `DeterministicSingleTimeSeries` with no source at the destination, and that
/// copy can land in a family where an unrelated `SingleTimeSeries` lives — the
/// two are distinct identities, so the catalog holds both. Probing the family
/// alone made that unrelated source unremovable on behalf of a view over a
/// different array entirely.
#[test]
fn a_copied_view_does_not_pin_an_unrelated_source_in_its_family() {
    each_backend(|store, backend| {
        // Owner 1: a source and the view derived from it. Both name array A.
        let src = add(
            store,
            1,
            TimeSeriesData::SingleTimeSeries(sts("load", hourly(8))),
        )
        .unwrap();
        store
            .transform_single_time_series(
                Duration::hours(2),
                Duration::hours(1),
                None,
                None,
                TransformPolicy::default(),
            )
            .unwrap();

        // Owner 2: a source of its own over *different* values, so a different
        // array; then owner 1's view copied onto it under the same name. Owner
        // 2's family now holds an STS and a DST that have no relationship.
        let unrelated = add(
            store,
            2,
            TimeSeriesData::SingleTimeSeries(sts("load", hourly(7))),
        )
        .unwrap();
        store
            .copy_time_series(dst_of(store, 1), 2, "Generator", None)
            .unwrap();
        assert_ne!(
            store
                .get_metadata_by_id(unrelated)
                .unwrap()
                .unwrap()
                .data_hash,
            store.get_metadata_by_id(src).unwrap().unwrap().data_hash,
            "{backend}: the two sources must be different arrays for this case"
        );

        // Owner 2's source backs nothing: the view beside it is a view of owner
        // 1's array, and removing this row takes nothing away from it.
        assert_eq!(store.remove_by_ids(&[unrelated]).unwrap(), 1, "{backend}");
        // The copied view still reads, because it holds its array by hash.
        store
            .read_by_id(dst_of(store, 2), ReadWindow::full())
            .unwrap();
        // And owner 1's source is still pinned by the view that *is* over it.
        let err = store.remove_by_ids(&[src]).unwrap_err();
        assert!(is_invalid(&err), "{backend}: {err}");
    });
}

/// The same, for a failure *inside* the loop rather than before it.
///
/// Invalidating once up front only covers the timestamp that fails before any
/// group is touched. A group failing part way through is the other half: every
/// group before it has already been filled with the new timestamp's values and
/// `fill` sets `filled = true` as it goes, so an up-front invalidation is long
/// spent by the time the error is raised. The reader is emptied on the error
/// path too, so both cases end the same way.
#[test]
fn a_static_read_failing_mid_loop_empties_the_groups_it_already_filled() {
    each_backend(|store, backend| {
        add(
            store,
            1,
            TimeSeriesData::SingleTimeSeries(sts("load", hourly(8))),
        )
        .unwrap();
        let i64s: Vec<i64> = (0..8).collect();
        add(
            store,
            2,
            TimeSeriesData::SingleTimeSeries(sts(
                "count",
                TypedArray::from_slice(vec![8], &i64s).unwrap(),
            )),
        )
        .unwrap();

        let mut reader = store
            .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
            .unwrap();
        assert!(reader.groups().len() > 1, "{backend}");
        store.static_read(&mut reader, t0()).unwrap();

        // Pull the array out from under the *last* group, so the read fails
        // only after the earlier ones have been filled with this timestamp.
        // A reader holds hashes, not catalog rows, so it keeps pointing at the
        // array the removal reclaimed.
        let last = reader.groups().len() - 1;
        let victims: Vec<TimeSeriesId> = reader.groups()[last].ids().to_vec();
        assert_eq!(store.remove_by_ids(&victims).unwrap(), victims.len());

        let err = store.static_read(&mut reader, t0() + Duration::hours(1));
        assert!(err.is_err(), "{backend}: the reclaimed array must not read");
        assert!(
            reader.groups().iter().all(|g| g.values().is_empty()),
            "{backend}: the groups filled before the failure are emptied too"
        );
    });
}

/// A failed read leaves the whole reader empty, not half of one read and half
/// of the last.
///
/// `StaticGroup::fill` clears the group it is filling, but a read is one
/// operation over every group: a failure part way through left the groups
/// already filled holding the new timestamp's values while the rest held the
/// previous read's, all of it still labelled with the previous timestamp. An
/// off-grid timestamp is the sharper form — it fails before any group is
/// touched, so every group kept the last read intact and a caller that ignored
/// the error saw a full, plausible, wrong answer.
#[test]
fn a_failed_static_read_empties_the_whole_reader() {
    each_backend(|store, backend| {
        for owner in 1..=2 {
            add(
                store,
                owner,
                TimeSeriesData::SingleTimeSeries(sts("load", hourly(8))),
            )
            .unwrap();
        }
        // A second dtype, so the reader holds more than one group and the
        // all-or-nothing claim has something to be about.
        let i64s: Vec<i64> = (0..8).collect();
        add(
            store,
            3,
            TimeSeriesData::SingleTimeSeries(sts(
                "count",
                TypedArray::from_slice(vec![8], &i64s).unwrap(),
            )),
        )
        .unwrap();

        let mut reader = store
            .build_static_reader(ListFilter::new().resolution(Duration::hours(1)))
            .unwrap();
        assert!(
            reader.groups().len() > 1,
            "{backend}: two dtypes, two groups"
        );
        store.static_read(&mut reader, t0()).unwrap();
        assert!(
            reader.groups().iter().all(|g| !g.values().is_empty()),
            "{backend}: the first read fills every group"
        );

        // Half past the hour is not a point on an hourly grid.
        let off_grid = t0() + Duration::minutes(30);
        assert!(
            store.static_read(&mut reader, off_grid).is_err(),
            "{backend}"
        );
        assert!(
            reader.groups().iter().all(|g| g.values().is_empty()),
            "{backend}: a failed read leaves no group serving its old values"
        );

        // And the reader still works afterwards.
        store.static_read(&mut reader, t0()).unwrap();
        assert!(
            reader.groups().iter().all(|g| !g.values().is_empty()),
            "{backend}"
        );
    });
}
