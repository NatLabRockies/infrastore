//! Focused coverage for array indexing in `Store::get_time_series`.
//!
//! These tests target the three layers of index arithmetic a returned slice
//! depends on, all of which must agree:
//!   1. `store.rs` — turning a `time_range` into `start_idx..end_idx`;
//!   2. `netcdf.rs` `packed_extents` — `(time_range, col, element_shape)` -> extents;
//!   3. `memory.rs::get_slice` — `row_bytes` byte arithmetic.
//!
//! Every battery runs against BOTH backends via [`for_each_backend`]: the
//! in-memory store, and a NetCDF store that is flushed, closed, and reopened
//! read-only so the on-disk packed layout (not just in-memory state) is read
//! back. Because both backends are checked against the same expected values,
//! these tests also assert memory/NetCDF parity.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    Dtype, Features, NonSequentialTimeSeries, OwnerCategory, SingleTimeSeries, Store,
    TimeSeriesData, TimeSeriesKey, TypedArray,
};

mod common;
use common::for_each_backend;

fn add_single(store: &mut Store, owner: i64, s: SingleTimeSeries) -> TimeSeriesKey {
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(s),
            Features::new(),
            None,
        )
        .unwrap()
}

/// Read a `SingleTimeSeries` window, returning `(length, initial_timestamp, values)`.
fn sliced(
    store: &Store,
    key: &TimeSeriesKey,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> (usize, DateTime<Utc>, Vec<f64>) {
    let got = store
        .get_time_series(key.identity(), Some((start, end)))
        .unwrap();
    let single = got.as_single().unwrap();
    (
        single.length,
        single.initial_timestamp,
        single.data.to_f64_vec().unwrap(),
    )
}

fn single_6(initial: DateTime<Utc>) -> SingleTimeSeries {
    SingleTimeSeries::new(
        initial,
        Duration::hours(1),
        TypedArray::from_f64(vec![6], &[10.0, 20.0, 30.0, 40.0, 50.0, 60.0]),
        "load",
    )
}

/// Several distinct series sharing one packed dataset must each read back their
/// OWN values, both as a full read and a sub-range. Catches column-offset bugs
/// (`col..col+1` in `packed_extents`) and cross-column contamination.
#[test]
fn cross_contamination_across_packed_columns() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let n = 5usize;
    let len = 4usize;

    for_each_backend(
        |store| {
            let mut keys = Vec::new();
            for i in 0..n {
                let base = i as f64 * 1000.0;
                let vals: Vec<f64> = (0..len).map(|j| base + j as f64).collect();
                let s = SingleTimeSeries::new(
                    initial,
                    resolution,
                    TypedArray::from_f64(vec![len], &vals),
                    "load",
                );
                keys.push(add_single(store, i as i64, s));
            }
            keys
        },
        |store, keys, backend| {
            for (i, key) in keys.iter().enumerate() {
                let base = i as f64 * 1000.0;

                let full = store.get_time_series(key.identity(), None).unwrap();
                let expected: Vec<f64> = (0..len).map(|j| base + j as f64).collect();
                assert_eq!(
                    full.as_single().unwrap().data.to_f64_vec().unwrap(),
                    expected,
                    "{backend}: full read of column {i}"
                );

                // Sub-range hours 1..3 -> indices 1, 2 of this column only.
                let (slen, sinit, svals) = sliced(
                    store,
                    key,
                    initial + Duration::hours(1),
                    initial + Duration::hours(3),
                );
                assert_eq!(slen, 2, "{backend}: sub length, column {i}");
                assert_eq!(
                    sinit,
                    initial + Duration::hours(1),
                    "{backend}: sub initial, column {i}"
                );
                assert_eq!(
                    svals,
                    vec![base + 1.0, base + 2.0],
                    "{backend}: sub values, column {i}"
                );
            }
        },
    );
}

/// A multidimensional (per-step element shape) series stored alongside another
/// in the same dataset must slice correctly: time sub-range AND a non-zero
/// column AND element dims interact in `packed_extents`. This is the most
/// complex extent computation and was previously read only in full.
#[test]
fn multidim_slice_at_nonzero_column() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    // shape [4, 3]: 4 timesteps, 3 elements each.
    let a: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let b: Vec<f64> = (0..12).map(|i| 100.0 + i as f64).collect();

    for_each_backend(
        {
            let a = a.clone();
            let b = b.clone();
            move |store| {
                let ka = add_single(
                    store,
                    11,
                    SingleTimeSeries::new(
                        initial,
                        resolution,
                        TypedArray::from_f64(vec![4, 3], &a),
                        "load",
                    ),
                );
                let kb = add_single(
                    store,
                    12,
                    SingleTimeSeries::new(
                        initial,
                        resolution,
                        TypedArray::from_f64(vec![4, 3], &b),
                        "load",
                    ),
                );
                (ka, kb)
            }
        },
        |store, (ka, kb), backend| {
            // Full read of the second column preserves shape + element order.
            let full_b = store.get_time_series(kb.identity(), None).unwrap();
            let full_b = full_b.as_single().unwrap();
            assert_eq!(full_b.data.shape, vec![4, 3], "{backend}: full shape");
            assert_eq!(
                full_b.data.to_f64_vec().unwrap(),
                b,
                "{backend}: full b values"
            );

            // Time sub-range rows 1..3 of column b -> timesteps 1 and 2.
            let sub = store
                .get_time_series(
                    kb.identity(),
                    Some((initial + Duration::hours(1), initial + Duration::hours(3))),
                )
                .unwrap();
            let sub = sub.as_single().unwrap();
            assert_eq!(sub.data.shape, vec![2, 3], "{backend}: sub shape");
            assert_eq!(
                sub.data.to_f64_vec().unwrap(),
                vec![103.0, 104.0, 105.0, 106.0, 107.0, 108.0],
                "{backend}: sub b rows 1..3"
            );

            // Column a is untouched by reads of column b.
            let full_a = store.get_time_series(ka.identity(), None).unwrap();
            assert_eq!(
                full_a.as_single().unwrap().data.to_f64_vec().unwrap(),
                a,
                "{backend}: column a intact"
            );
        },
    );
}

/// Slicing must respect `dtype.size()` byte strides for every dtype, not just
/// F64. Builds arrays from raw little-endian bytes and checks the sliced byte
/// range is exactly `[2*size .. 5*size)`.
#[test]
fn slice_preserves_all_dtypes() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);

    let cases: Vec<(&str, TypedArray)> = vec![
        (
            "i64",
            TypedArray::new(
                Dtype::I64,
                vec![6],
                [1i64, 2, 3, 4, 5, 6]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
            )
            .unwrap(),
        ),
        (
            "i32",
            TypedArray::new(
                Dtype::I32,
                vec![6],
                [1i32, 2, 3, 4, 5, 6]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
            )
            .unwrap(),
        ),
        (
            "f32",
            TypedArray::new(
                Dtype::F32,
                vec![6],
                [1.5f32, 2.5, 3.5, 4.5, 5.5, 6.5]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
            )
            .unwrap(),
        ),
        (
            "u64",
            TypedArray::new(
                Dtype::U64,
                vec![6],
                [10u64, 20, 30, 40, 50, 60]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
            )
            .unwrap(),
        ),
        (
            "bool",
            TypedArray::new(Dtype::Bool, vec![6], vec![1u8, 0, 1, 0, 1, 0]).unwrap(),
        ),
    ];

    for (label, arr) in cases {
        for_each_backend(
            {
                let arr = arr.clone();
                move |store| {
                    add_single(
                        store,
                        13,
                        SingleTimeSeries::new(initial, resolution, arr.clone(), "load"),
                    )
                }
            },
            |store, key, backend| {
                // Full read preserves dtype, shape, and exact bytes.
                let full = store.get_time_series(key.identity(), None).unwrap();
                let full = full.as_single().unwrap();
                assert_eq!(full.data.dtype, arr.dtype, "{backend}/{label}: full dtype");
                assert_eq!(full.data.shape, arr.shape, "{backend}/{label}: full shape");
                assert_eq!(full.data.bytes, arr.bytes, "{backend}/{label}: full bytes");

                // Slice indices 2..5; byte offsets must respect dtype size.
                let sub = store
                    .get_time_series(
                        key.identity(),
                        Some((initial + Duration::hours(2), initial + Duration::hours(5))),
                    )
                    .unwrap();
                let sub = sub.as_single().unwrap();
                let sz = arr.dtype.size();
                assert_eq!(sub.data.dtype, arr.dtype, "{backend}/{label}: sub dtype");
                assert_eq!(sub.data.shape, vec![3], "{backend}/{label}: sub shape");
                assert_eq!(
                    sub.data.bytes,
                    arr.bytes[2 * sz..5 * sz].to_vec(),
                    "{backend}/{label}: sub bytes"
                );
            },
        );
    }
}

/// Pins the index-from-timestamp arithmetic for `SingleTimeSeries`:
/// `start` floors, `end` ceilings (half-open), and out-of-range windows clamp.
#[test]
fn single_slice_boundary_semantics() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let h = move |n: i64| initial + Duration::hours(n);

    for_each_backend(
        |store| add_single(store, 12, single_6(initial)),
        |store, key, backend| {
            // Aligned window [2, 5) -> 30, 40, 50.
            let (_, init, vals) = sliced(store, key, h(2), h(5));
            assert_eq!(vals, vec![30.0, 40.0, 50.0], "{backend}: aligned values");
            assert_eq!(init, h(2), "{backend}: aligned initial");

            // Unaligned start rounds DOWN (floor): 1.5h -> index 1.
            let (_, init, vals) = sliced(store, key, initial + Duration::minutes(90), h(5));
            assert_eq!(
                vals,
                vec![20.0, 30.0, 40.0, 50.0],
                "{backend}: unaligned start values"
            );
            assert_eq!(init, h(1), "{backend}: unaligned start initial");

            // End exactly on a sample boundary is EXCLUSIVE: [2, 3) -> 30.
            assert_eq!(
                sliced(store, key, h(2), h(3)).2,
                vec![30.0],
                "{backend}: end boundary exclusive"
            );

            // End one millisecond past the boundary pulls the next sample (ceil).
            assert_eq!(
                sliced(store, key, h(2), h(3) + Duration::milliseconds(1)).2,
                vec![30.0, 40.0],
                "{backend}: end ceil"
            );

            // Zero-width range -> empty.
            let (zlen, _, zvals) = sliced(store, key, h(2), h(2));
            assert_eq!(zlen, 0, "{backend}: zero-width length");
            assert!(zvals.is_empty(), "{backend}: zero-width values");

            // Window straddling the start clamps start to index 0.
            let (_, sinit, svals) = sliced(store, key, h(-2), h(2));
            assert_eq!(svals, vec![10.0, 20.0], "{backend}: straddle start values");
            assert_eq!(sinit, initial, "{backend}: straddle start initial");

            // Entirely before / after the series -> empty.
            assert_eq!(
                sliced(store, key, h(-5), h(-1)).0,
                0,
                "{backend}: before series"
            );
            assert_eq!(
                sliced(store, key, h(100), h(200)).0,
                0,
                "{backend}: after series"
            );

            // Wider than the series clamps to the full range.
            assert_eq!(
                sliced(store, key, h(-10), h(100)).2,
                vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0],
                "{backend}: wider than series"
            );
        },
    );
}

/// A length-1 series: full read, a window covering it, and a window before it.
#[test]
fn length_one_series_slicing() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

    for_each_backend(
        |store| {
            add_single(
                store,
                14,
                SingleTimeSeries::new(
                    initial,
                    Duration::hours(1),
                    TypedArray::from_f64(vec![1], &[42.0]),
                    "load",
                ),
            )
        },
        |store, key, backend| {
            let full = store.get_time_series(key.identity(), None).unwrap();
            assert_eq!(
                full.as_single().unwrap().data.to_f64_vec().unwrap(),
                vec![42.0],
                "{backend}: full"
            );

            assert_eq!(
                sliced(store, key, initial, initial + Duration::hours(1)).2,
                vec![42.0],
                "{backend}: covering window"
            );
            assert_eq!(
                sliced(store, key, initial - Duration::hours(2), initial).0,
                0,
                "{backend}: window before series"
            );
        },
    );
}

/// Regression: a far-future `end` (here `initial + i64::MAX` nanoseconds)
/// must clamp to the series length in the ceiling-division arithmetic in
/// `get_time_series` without overflow. Expect the full series, no panic.
#[test]
fn far_future_end_does_not_overflow() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

    for_each_backend(
        |store| add_single(store, 15, single_6(initial)),
        |store, key, backend| {
            let end = initial + Duration::nanoseconds(i64::MAX);
            let got = store
                .get_time_series(key.identity(), Some((initial, end)))
                .unwrap();
            assert_eq!(
                got.as_single().unwrap().data.to_f64_vec().unwrap(),
                vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0],
                "{backend}: far-future end returns full series"
            );
        },
    );
}

/// `NonSequentialTimeSeries` windows are half-open `[start, end)` — `start`
/// matching a timestamp is inclusive, `end` matching one is exclusive,
/// consistent with `SingleTimeSeries`. Empty and out-of-range windows return
/// an empty series.
#[test]
fn non_sequential_boundary_semantics() {
    let t0 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let timestamps = vec![
        t0,
        t0 + Duration::hours(1),
        t0 + Duration::hours(2),
        t0 + Duration::hours(3),
    ];

    for_each_backend(
        {
            let timestamps = timestamps.clone();
            move |store| {
                let series = NonSequentialTimeSeries::new(
                    timestamps.clone(),
                    TypedArray::from_f64(vec![4], &[1.0, 2.0, 3.0, 4.0]),
                    "events",
                )
                .unwrap();
                store
                    .add_time_series(
                        999,
                        "Generator",
                        OwnerCategory::Component,
                        TimeSeriesData::NonSequentialTimeSeries(series),
                        Features::new(),
                        None,
                    )
                    .unwrap()
            }
        },
        |store, key, backend| {
            let ns = |s: DateTime<Utc>, e: DateTime<Utc>| {
                let got = store.get_time_series(key.identity(), Some((s, e))).unwrap();
                let series = got.as_non_sequential().unwrap();
                (series.timestamps.clone(), series.data.to_f64_vec().unwrap())
            };

            // start inclusive, end exclusive: [1h, 3h) -> indices 1, 2.
            let (ts, vals) = ns(t0 + Duration::hours(1), t0 + Duration::hours(3));
            assert_eq!(ts, timestamps[1..3], "{backend}: boundary timestamps");
            assert_eq!(vals, vec![2.0, 3.0], "{backend}: boundary values");

            // Empty window strictly between two timestamps.
            let (ts, _) = ns(t0 + Duration::minutes(90), t0 + Duration::minutes(105));
            assert!(ts.is_empty(), "{backend}: between-timestamps empty");

            // Windows entirely before / after all timestamps.
            assert!(
                ns(t0 - Duration::hours(2), t0 - Duration::hours(1))
                    .0
                    .is_empty(),
                "{backend}: before all"
            );
            assert!(
                ns(t0 + Duration::hours(10), t0 + Duration::hours(20))
                    .0
                    .is_empty(),
                "{backend}: after all"
            );
        },
    );
}
