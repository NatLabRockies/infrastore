//! Tests for the forecast read path in `Store::get_time_series`.
//!
//! All cases run against BOTH backends via [`for_each_backend`]: the in-memory
//! store, and an HDF5 store that is flushed, closed, and reopened read-only
//! (exercising the persisted format).
//!
//! Dense forecasts (`Deterministic` / `Probabilistic` / `Scenarios`) are written
//! through the generic `Store::add_time_series`; `DeterministicSingleTimeSeries`
//! is derived from a stored `SingleTimeSeries` via
//! `Store::transform_single_time_series`. Read results are returned as
//! `TimeSeriesData::{Deterministic,Probabilistic,Scenarios}` variants; DST is
//! synthesized into `Deterministic`.

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    Deterministic, Dtype, Features, ForecastTimeSeriesKey, ListFilter, OwnerCategory, Period,
    Probabilistic, Scenarios, SingleTimeSeries, Store, TimeSeriesData, TimeSeriesKey,
    TimeSeriesType, TypedArray, create_store, open_store,
};

mod common;
use common::for_each_backend;

// ---------------------------------------------------------------------------
// Helper: add a forecast and return the key.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn add_forecast(
    store: &mut Store,
    owner: i64,
    name: &str,
    ts_type: TimeSeriesType,
    initial: chrono::DateTime<chrono::Utc>,
    resolution: Duration,
    horizon: Duration,
    interval: Duration,
    count: usize,
    data: TypedArray,
    percentiles: Option<Vec<f64>>,
) -> TimeSeriesKey {
    let data = match ts_type {
        TimeSeriesType::Deterministic => TimeSeriesData::Deterministic(
            Deterministic::new(initial, resolution, horizon, interval, count, data, name).unwrap(),
        ),
        TimeSeriesType::Probabilistic => TimeSeriesData::Probabilistic(
            Probabilistic::new(
                initial,
                resolution,
                horizon,
                interval,
                count,
                percentiles.expect("Probabilistic requires percentiles"),
                data,
                name,
            )
            .unwrap(),
        ),
        TimeSeriesType::Scenarios => {
            let scenario_count = data.shape[0];
            TimeSeriesData::Scenarios(
                Scenarios::new(
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    scenario_count,
                    data,
                    name,
                )
                .unwrap(),
            )
        }
        TimeSeriesType::DeterministicSingleTimeSeries => {
            // DST is not added directly: store the underlying SingleTimeSeries,
            // then derive the DST via `transform_single_time_series`.
            store
                .add_time_series(
                    owner,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                        initial, resolution, data, name,
                    )),
                    Features::new(),
                )
                .unwrap();
            store
                .transform_single_time_series(horizon, interval, None, None, Default::default())
                .unwrap();
            return TimeSeriesKey::Forecast(ForecastTimeSeriesKey::new(
                owner,
                OwnerCategory::Component,
                TimeSeriesType::DeterministicSingleTimeSeries,
                name.to_string(),
                resolution,
                Features::new(),
                initial,
                horizon,
                interval,
                count,
            ));
        }
        other => panic!("add_forecast helper: unsupported type {other:?}"),
    };
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            data,
            Features::new(),
        )
        .unwrap()
}

// Convenience: build f64 TypedArray.
fn f64_arr(shape: Vec<usize>, vals: &[f64]) -> TypedArray {
    TypedArray::from_f64(shape, vals)
}

// Convenience: build i64 TypedArray.
fn i64_arr(shape: Vec<usize>, vals: &[i64]) -> TypedArray {
    let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
    TypedArray::new(Dtype::I64, shape, bytes).unwrap()
}

// Decode i64 bytes from a TypedArray.
fn to_i64_vec(arr: &TypedArray) -> Vec<i64> {
    assert_eq!(arr.dtype, Dtype::I64);
    arr.bytes
        .chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

// ---------------------------------------------------------------------------
// Case 1: Deterministic round-trip, scalar [H, C]
// ---------------------------------------------------------------------------

#[test]
fn deterministic_scalar_roundtrip() {
    // H=4, C=3, scalar => shape [4, 3], values row-major.
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(4);
    let interval = Duration::hours(6);
    let count = 3usize;
    let vals: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let data = f64_arr(vec![4, 3], &vals);

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    1,
                    "load",
                    TimeSeriesType::Deterministic,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    None,
                )
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.count, count, "{backend}: count");
            assert_eq!(det.horizon, horizon, "{backend}: horizon");
            assert_eq!(det.interval, interval, "{backend}: interval");
            assert_eq!(det.initial_timestamp, initial, "{backend}: initial");
            assert_eq!(det.resolution, resolution, "{backend}: resolution");
            assert_eq!(det.data.shape, vec![4, 3], "{backend}: shape");
            assert_eq!(det.data.to_f64_vec().unwrap(), vals, "{backend}: values");
        },
    );
}

// ---------------------------------------------------------------------------
// Case 2: Deterministic multidim [H, C, k] – element dims preserved
// ---------------------------------------------------------------------------

#[test]
fn deterministic_multidim_element_shape() {
    // H=2, C=2, E=[3] => shape [2, 2, 3], 12 elements.
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(4);
    let count = 2usize;
    let vals: Vec<f64> = (0..12).map(|i| i as f64 * 10.0).collect();
    let data = f64_arr(vec![2, 2, 3], &vals);

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    2,
                    "cost",
                    TimeSeriesType::Deterministic,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    None,
                )
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.data.shape, vec![2, 2, 3], "{backend}: shape");
            assert_eq!(det.data.to_f64_vec().unwrap(), vals, "{backend}: values");
            assert_eq!(det.count, 2, "{backend}: count");
        },
    );
}

// ---------------------------------------------------------------------------
// Case 3: Probabilistic round-trip [P, H, C]
// ---------------------------------------------------------------------------

#[test]
fn probabilistic_roundtrip() {
    // P=3, H=2, C=2 => shape [3, 2, 2], 12 elements.
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(4);
    let count = 2usize;
    let percentiles = vec![0.1, 0.5, 0.9];
    let vals: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let data = f64_arr(vec![3, 2, 2], &vals);

    for_each_backend(
        {
            let data = data.clone();
            let percentiles = percentiles.clone();
            move |store| {
                add_forecast(
                    store,
                    3,
                    "prob_load",
                    TimeSeriesType::Probabilistic,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    Some(percentiles.clone()),
                )
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let prob = got.as_probabilistic().unwrap();
            assert_eq!(prob.percentiles, percentiles, "{backend}: percentiles");
            assert_eq!(prob.count, count, "{backend}: count");
            assert_eq!(prob.data.shape, vec![3, 2, 2], "{backend}: shape");
            // Each percentile slice: prob.data is [P, H, C].
            // Percentile 0 = vals[0..4], percentile 1 = vals[4..8], etc.
            assert_eq!(prob.data.to_f64_vec().unwrap(), vals, "{backend}: values");
        },
    );
}

// ---------------------------------------------------------------------------
// Case 4: Scenarios round-trip [S, H, C]
// ---------------------------------------------------------------------------

#[test]
fn scenarios_roundtrip() {
    // S=4, H=3, C=2 => shape [4, 3, 2], 24 elements.
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(3);
    let interval = Duration::hours(6);
    let count = 2usize;
    let scenario_count = 4usize;
    let vals: Vec<f64> = (0..24).map(|i| i as f64).collect();
    let data = f64_arr(vec![4, 3, 2], &vals);

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    4,
                    "scenarios_load",
                    TimeSeriesType::Scenarios,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    None,
                )
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let scen = got.as_scenarios().unwrap();
            assert_eq!(
                scen.scenario_count, scenario_count,
                "{backend}: scenario_count"
            );
            assert_eq!(scen.count, count, "{backend}: count");
            assert_eq!(scen.data.shape, vec![4, 3, 2], "{backend}: shape");
            assert_eq!(scen.data.to_f64_vec().unwrap(), vals, "{backend}: values");
        },
    );
}

// ---------------------------------------------------------------------------
// Case 5: Window selection by (start, end) – Det, Prob, Scen all slice right
// ---------------------------------------------------------------------------

#[test]
fn window_selection_deterministic() {
    // C=5, H=2, interval=6h. Windows at t0, t0+6h, t0+12h, t0+18h, t0+24h.
    // Select windows 1..3 (k=1 and k=2).
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(6);
    let count = 5usize;
    // shape [2, 5]: row 0 = [w0_s0, w1_s0, w2_s0, w3_s0, w4_s0], etc.
    // Use distinct values per (step, window) for easy verification.
    // val(s, w) = s*100 + w
    let vals: Vec<f64> = (0..2_usize)
        .flat_map(|s| (0..5_usize).map(move |w| (s * 100 + w) as f64))
        .collect();
    let data = f64_arr(vec![2, 5], &vals);

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    5,
                    "det_win",
                    TimeSeriesType::Deterministic,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    None,
                )
            }
        },
        |store, key, backend| {
            // Select windows k=1 and k=2: start = t0+6h, end = t0+18h (exclusive).
            let start = initial + Duration::hours(6);
            let end = initial + Duration::hours(18);
            let got = store
                .get_time_series(key.identity(), Some((start, end)))
                .unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.count, 2, "{backend}: selected count");
            assert_eq!(det.initial_timestamp, start, "{backend}: initial_timestamp");
            // Selected shape: [2, 2] — windows 1 and 2 of the original 5.
            assert_eq!(det.data.shape, vec![2, 2], "{backend}: shape");
            // Expected: col 1 and col 2 of each step row.
            // step 0: [1.0, 2.0], step 1: [101.0, 102.0]
            let expected = vec![1.0, 2.0, 101.0, 102.0];
            assert_eq!(
                det.data.to_f64_vec().unwrap(),
                expected,
                "{backend}: values"
            );
        },
    );
}

#[test]
fn window_selection_probabilistic() {
    // P=2, H=2, C=4, interval=4h. Select windows 1..3 (k=1,2).
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(4);
    let count = 4usize;
    let percentiles = vec![0.25, 0.75];
    // shape [2, 2, 4]: val(p, s, w) = p*1000 + s*100 + w
    let vals: Vec<f64> = (0..2_usize)
        .flat_map(|p| {
            (0..2_usize)
                .flat_map(move |s| (0..4_usize).map(move |w| (p * 1000 + s * 100 + w) as f64))
        })
        .collect();
    let data = f64_arr(vec![2, 2, 4], &vals);

    for_each_backend(
        {
            let data = data.clone();
            let percentiles = percentiles.clone();
            move |store| {
                add_forecast(
                    store,
                    51,
                    "prob_win",
                    TimeSeriesType::Probabilistic,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    Some(percentiles.clone()),
                )
            }
        },
        |store, key, backend| {
            // Select windows k=1,2: start = t0+4h, end = t0+12h (exclusive).
            let start = initial + Duration::hours(4);
            let end = initial + Duration::hours(12);
            let got = store
                .get_time_series(key.identity(), Some((start, end)))
                .unwrap();
            let prob = got.as_probabilistic().unwrap();
            assert_eq!(prob.count, 2, "{backend}: count");
            assert_eq!(prob.initial_timestamp, start, "{backend}: initial");
            assert_eq!(prob.data.shape, vec![2, 2, 2], "{backend}: shape");
            // For p=0, s=0: windows 1,2 => [1.0, 2.0]
            // For p=0, s=1: windows 1,2 => [101.0, 102.0]
            // For p=1, s=0: windows 1,2 => [1001.0, 1002.0]
            // For p=1, s=1: windows 1,2 => [1101.0, 1102.0]
            let expected = vec![1.0, 2.0, 101.0, 102.0, 1001.0, 1002.0, 1101.0, 1102.0];
            assert_eq!(
                prob.data.to_f64_vec().unwrap(),
                expected,
                "{backend}: values"
            );
        },
    );
}

#[test]
fn window_selection_scenarios() {
    // S=3, H=2, C=4, interval=4h. Select windows 2..4 (k=2,3).
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(4);
    let count = 4usize;
    // shape [3, 2, 4]: val(sc, s, w) = sc*1000 + s*100 + w
    let vals: Vec<f64> = (0..3_usize)
        .flat_map(|sc| {
            (0..2_usize)
                .flat_map(move |s| (0..4_usize).map(move |w| (sc * 1000 + s * 100 + w) as f64))
        })
        .collect();
    let data = f64_arr(vec![3, 2, 4], &vals);

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    52,
                    "scen_win",
                    TimeSeriesType::Scenarios,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    None,
                )
            }
        },
        |store, key, backend| {
            // Select windows k=2,3: start = t0+8h, end = t0+18h (exclusive).
            let start = initial + Duration::hours(8);
            let end = initial + Duration::hours(18);
            let got = store
                .get_time_series(key.identity(), Some((start, end)))
                .unwrap();
            let scen = got.as_scenarios().unwrap();
            assert_eq!(scen.count, 2, "{backend}: count");
            assert_eq!(scen.initial_timestamp, start, "{backend}: initial");
            assert_eq!(scen.data.shape, vec![3, 2, 2], "{backend}: shape");
            // sc=0, s=0: w=2,3 => [2.0, 3.0]; sc=0, s=1: [102.0, 103.0]
            // sc=1, s=0: [1002.0, 1003.0]; sc=1, s=1: [1102.0, 1103.0]
            // sc=2, s=0: [2002.0, 2003.0]; sc=2, s=1: [2102.0, 2103.0]
            let expected = vec![
                2.0, 3.0, 102.0, 103.0, 1002.0, 1003.0, 1102.0, 1103.0, 2002.0, 2003.0, 2102.0,
                2103.0,
            ];
            assert_eq!(
                scen.data.to_f64_vec().unwrap(),
                expected,
                "{backend}: values"
            );
        },
    );
}

// ---------------------------------------------------------------------------
// Case 6: Error cases – misaligned start, end < start, empty window range
// ---------------------------------------------------------------------------

#[test]
fn window_selection_error_cases() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(4);
    let count = 4usize;
    let vals: Vec<f64> = vec![0.0; 8]; // [2, 4]
    let data = f64_arr(vec![2, 4], &vals);

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    6,
                    "errfcast",
                    TimeSeriesType::Deterministic,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    None,
                )
            }
        },
        |store, key, backend| {
            // end < start => InvalidParameter
            let start = initial + Duration::hours(4);
            let end = initial;
            let err = store
                .get_time_series(key.identity(), Some((start, end)))
                .unwrap_err();
            assert!(
                err.to_string().contains("end < start"),
                "{backend}: end<start: {err}"
            );

            // Misaligned start (not on interval boundary) => InvalidParameter
            let misaligned = initial + Duration::hours(3); // interval=4h, not aligned
            let far_end = initial + Duration::hours(20);
            let err = store
                .get_time_series(key.identity(), Some((misaligned, far_end)))
                .unwrap_err();
            assert!(
                err.to_string().contains("window boundary"),
                "{backend}: misaligned: {err}"
            );

            // Empty selection (aligned start with end == start) => count == 0.
            let aligned_start = initial + Duration::hours(4);
            let got = store
                .get_time_series(key.identity(), Some((aligned_start, aligned_start)))
                .unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.count, 0, "{backend}: empty count");
            assert_eq!(det.data.shape, vec![2, 0], "{backend}: empty shape");
        },
    );
}

// ---------------------------------------------------------------------------
// Case 7: DeterministicSingleTimeSeries synthesis – overlapping windows
// ---------------------------------------------------------------------------

#[test]
fn dst_synthesis_overlapping_windows() {
    // Underlying STS-like array: total_len=8, shape [8].
    // H=4 steps (horizon=4h, resolution=1h), interval=2h (interval_steps=2).
    // count=3 windows: k=0 covers [0..4), k=1 covers [2..6), k=2 covers [4..8).
    // => (count-1)*interval_steps + H = 2*2+4 = 8 = total_len. OK.
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(4);
    let interval = Duration::hours(2);
    let count = 3usize;
    let underlying: Vec<f64> = (0..8).map(|i| i as f64).collect();
    let data = f64_arr(vec![8], &underlying);

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    100,
                    "dst_series",
                    TimeSeriesType::DeterministicSingleTimeSeries,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    None,
                )
            }
        },
        |store, key, backend| {
            // Full read => synthesized Deterministic, shape [4, 3].
            let got = store.get_time_series(key.identity(), None).unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.count, 3, "{backend}: count");
            assert_eq!(det.data.shape, vec![4, 3], "{backend}: shape");
            // Row-major [H, C]: out[s, w] = underlying[w*2 + s].
            // Row 0 (s=0): [0.0, 2.0, 4.0]
            // Row 1 (s=1): [1.0, 3.0, 5.0]
            // Row 2 (s=2): [2.0, 4.0, 6.0]
            // Row 3 (s=3): [3.0, 5.0, 7.0]
            let expected = vec![0.0, 2.0, 4.0, 1.0, 3.0, 5.0, 2.0, 4.0, 6.0, 3.0, 5.0, 7.0];
            assert_eq!(
                det.data.to_f64_vec().unwrap(),
                expected,
                "{backend}: synthesized values"
            );

            // Window selection: select window k=1 only.
            let start = initial + interval;
            let end = initial + interval + interval; // exclusive
            let got2 = store
                .get_time_series(key.identity(), Some((start, end)))
                .unwrap();
            let det2 = got2.as_deterministic().unwrap();
            assert_eq!(det2.count, 1, "{backend}: selected count");
            assert_eq!(det2.initial_timestamp, start, "{backend}: selected initial");
            assert_eq!(det2.data.shape, vec![4, 1], "{backend}: selected shape");
            // Window k=1 covers [2..6): [2.0, 3.0, 4.0, 5.0]
            let expected2 = vec![2.0, 3.0, 4.0, 5.0];
            assert_eq!(
                det2.data.to_f64_vec().unwrap(),
                expected2,
                "{backend}: selected window values"
            );
        },
    );
}

// ---------------------------------------------------------------------------
// Case 8: Non-f64 dtype (i64) – exact bytes survive window slicing
// ---------------------------------------------------------------------------

#[test]
fn deterministic_i64_dtype_preserved() {
    // H=2, C=3, shape [2, 3], i64.
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(4);
    let count = 3usize;
    let vals: Vec<i64> = vec![100, 200, 300, 400, 500, 600];
    let data = i64_arr(vec![2, 3], &vals);
    let original_bytes = data.bytes.clone();

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    101,
                    "i64_fcast",
                    TimeSeriesType::Deterministic,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    None,
                )
            }
        },
        |store, key, backend| {
            // Full read preserves dtype and exact bytes.
            let got = store.get_time_series(key.identity(), None).unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.data.dtype, Dtype::I64, "{backend}: dtype");
            assert_eq!(det.data.shape, vec![2, 3], "{backend}: shape");
            assert_eq!(det.data.bytes, original_bytes, "{backend}: full bytes");

            // Select window k=1 (start = t0+4h, end = t0+8h).
            let start = initial + Duration::hours(4);
            let end = initial + Duration::hours(8);
            let got2 = store
                .get_time_series(key.identity(), Some((start, end)))
                .unwrap();
            let det2 = got2.as_deterministic().unwrap();
            assert_eq!(det2.data.dtype, Dtype::I64, "{backend}: sliced dtype");
            assert_eq!(det2.data.shape, vec![2, 1], "{backend}: sliced shape");
            // Window k=1 => col 1 of each row: val[0][1]=200, val[1][1]=500.
            let expected = vec![200i64, 500];
            assert_eq!(to_i64_vec(&det2.data), expected, "{backend}: sliced values");
        },
    );
}

// ---------------------------------------------------------------------------
// Case 10: get_forecast_parameters returns real values when forecasts exist
// ---------------------------------------------------------------------------

#[test]
fn get_forecast_parameters_real() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(4);
    let interval = Duration::hours(6);
    let count = 3usize;
    let data = f64_arr(vec![4, 3], &[0.0; 12]);

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    102,
                    "fp_series",
                    TimeSeriesType::Deterministic,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    None,
                )
            }
        },
        |store, _key, backend| {
            let params = store.get_forecast_parameters(None, None).unwrap();
            assert_eq!(
                params.horizon,
                Some(Period::Fixed(horizon)),
                "{backend}: horizon"
            );
            assert_eq!(
                params.interval,
                Some(Period::Fixed(interval)),
                "{backend}: interval"
            );
            assert_eq!(params.count, Some(count), "{backend}: count");
            assert_eq!(
                params.resolution,
                Some(Period::Fixed(resolution)),
                "{backend}: resolution"
            );
        },
    );
}

#[test]
fn get_forecast_parameters_empty_when_no_forecasts() {
    let store = create_store(None, true).unwrap();
    let params = store.get_forecast_parameters(None, None).unwrap();
    assert!(params.horizon.is_none());
    assert!(params.interval.is_none());
    assert!(params.count.is_none());
    assert!(params.resolution.is_none());
}

// ---------------------------------------------------------------------------
// Case 11: DST with multidim element shape [E] preserved through synthesis
// ---------------------------------------------------------------------------

#[test]
fn dst_synthesis_multidim_element_shape() {
    // Underlying shape [6, 2] (total_len=6, E=[2]).
    // H=3, interval_steps=2, count=2.
    // Window k=0: rows 0..3; k=1: rows 2..5 (overlap!).
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(3);
    let interval = Duration::hours(2);
    let count = 2usize;
    // underlying[i][e] = i*10 + e
    let vals: Vec<f64> = (0..6_usize)
        .flat_map(|i| (0..2_usize).map(move |e| (i * 10 + e) as f64))
        .collect();
    let data = f64_arr(vec![6, 2], &vals);

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    103,
                    "dst_md_series",
                    TimeSeriesType::DeterministicSingleTimeSeries,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    count,
                    data.clone(),
                    None,
                )
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let det = got.as_deterministic().unwrap();
            // Expected output shape: [H=3, C=2, E=2] = [3, 2, 2], 12 elements.
            assert_eq!(det.data.shape, vec![3, 2, 2], "{backend}: shape");
            // Row-major [s, w, e]: out[s][w][e] = underlying[w*2+s][e]
            // s=0, w=0: underlying[0] = [0, 1]; s=0, w=1: underlying[2] = [20, 21]
            // s=1, w=0: underlying[1] = [10, 11]; s=1, w=1: underlying[3] = [30, 31]
            // s=2, w=0: underlying[2] = [20, 21]; s=2, w=1: underlying[4] = [40, 41]
            let expected = vec![
                0.0, 1.0, 20.0, 21.0, // s=0
                10.0, 11.0, 30.0, 31.0, // s=1
                20.0, 21.0, 40.0, 41.0, // s=2
            ];
            assert_eq!(
                det.data.to_f64_vec().unwrap(),
                expected,
                "{backend}: synthesized multidim values"
            );
        },
    );
}

// ---------------------------------------------------------------------------
// Deterministic resolution + matched type
//
// A `Deterministic` request resolves to one concrete key (whose
// `time_series_type` is the stored form that matched), errors explicitly on
// ambiguity, and reports a genuine miss as `NotFound` rather than masking it.
// ---------------------------------------------------------------------------

use infrastore_core::TimeSeriesError;

// Underlying STS values long enough to derive a DST under (H=2, interval=1).
fn dst_source_vals() -> Vec<f64> {
    (0..8).map(|i| i as f64).collect()
}

#[test]
fn resolve_deterministic_matches_real_deterministic() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(4);
    let interval = Duration::hours(6);
    let vals: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let data = f64_arr(vec![4, 3], &vals);

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    1,
                    "load",
                    TimeSeriesType::Deterministic,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    3,
                    data.clone(),
                    None,
                )
            }
        },
        |store, _key, backend| {
            let resolved = store
                .resolve_forecast_key(
                    1,
                    OwnerCategory::Component,
                    "load",
                    Some(Period::Fixed(resolution)),
                    None,
                    Features::new(),
                    TimeSeriesType::Deterministic,
                )
                .unwrap();
            assert_eq!(
                resolved.time_series_type(),
                TimeSeriesType::Deterministic,
                "{backend}: matched type is the concrete Deterministic"
            );
            assert!(
                store
                    .get_time_series(resolved.identity(), None)
                    .unwrap()
                    .as_deterministic()
                    .is_some(),
                "{backend}: reads back as Deterministic"
            );
        },
    );
}

#[test]
fn resolve_deterministic_matches_dst() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(1);
    let data = f64_arr(vec![8], &dst_source_vals());

    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                add_forecast(
                    store,
                    7,
                    "gen",
                    TimeSeriesType::DeterministicSingleTimeSeries,
                    initial,
                    resolution,
                    horizon,
                    interval,
                    0,
                    data.clone(),
                    None,
                )
            }
        },
        |store, _key, backend| {
            let resolved = store
                .resolve_forecast_key(
                    7,
                    OwnerCategory::Component,
                    "gen",
                    Some(Period::Fixed(resolution)),
                    None,
                    Features::new(),
                    TimeSeriesType::Deterministic,
                )
                .unwrap();
            assert_eq!(
                resolved.time_series_type(),
                TimeSeriesType::DeterministicSingleTimeSeries,
                "{backend}: matched type is the concrete DST"
            );
            assert!(
                store
                    .get_time_series(resolved.identity(), None)
                    .unwrap()
                    .as_deterministic()
                    .is_some(),
                "{backend}: DST reads back as Deterministic"
            );
        },
    );
}

#[test]
fn resolve_deterministic_not_found_is_not_masked() {
    let store = create_store(None, true).unwrap();
    let err = store
        .resolve_forecast_key(
            1,
            OwnerCategory::Component,
            "missing",
            Some(Period::Fixed(Duration::hours(1))),
            None,
            Features::new(),
            TimeSeriesType::Deterministic,
        )
        .unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::NotFound),
        "expected NotFound, got {err:?}"
    );
}

#[test]
fn resolve_deterministic_ambiguous_by_interval_errors() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let mut store = create_store(None, true).unwrap();

    // Two Deterministic forecasts of one variable at the same resolution but
    // different intervals (e.g. day-ahead vs intra-day). Interval is part of the
    // identity, so both coexist as distinct series.
    add_forecast(
        &mut store,
        3,
        "dup",
        TimeSeriesType::Deterministic,
        initial,
        resolution,
        horizon,
        Duration::hours(1),
        2,
        f64_arr(vec![2, 2], &[0.0, 1.0, 2.0, 3.0]),
        None,
    );
    add_forecast(
        &mut store,
        3,
        "dup",
        TimeSeriesType::Deterministic,
        initial,
        resolution,
        horizon,
        Duration::hours(6),
        2,
        f64_arr(vec![2, 2], &[10.0, 11.0, 12.0, 13.0]),
        None,
    );

    // Resolving without an interval is ambiguous: two candidates differ only by
    // interval.
    let err = store
        .resolve_forecast_key(
            3,
            OwnerCategory::Component,
            "dup",
            Some(Period::Fixed(resolution)),
            None,
            Features::new(),
            TimeSeriesType::Deterministic,
        )
        .unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::InvalidParameter(_)),
        "ambiguous interval should error, got {err:?}"
    );

    // Specifying the interval disambiguates.
    let d = store
        .resolve_forecast_key(
            3,
            OwnerCategory::Component,
            "dup",
            Some(Period::Fixed(resolution)),
            Some(Period::Fixed(Duration::hours(6))),
            Features::new(),
            TimeSeriesType::Deterministic,
        )
        .unwrap();
    assert_eq!(d.time_series_type(), TimeSeriesType::Deterministic);
    assert_eq!(d.interval(), Some(Period::Fixed(Duration::hours(6))));
}

#[test]
fn deterministic_and_dst_are_mutually_exclusive() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(1);

    // Adding a Deterministic when a DST view of the same family exists is
    // rejected: a DST is a synthetic view of a SingleTimeSeries and the two may
    // never coexist.
    let mut store = create_store(None, true).unwrap();
    add_forecast(
        &mut store,
        3,
        "load",
        TimeSeriesType::DeterministicSingleTimeSeries,
        initial,
        resolution,
        horizon,
        interval,
        0,
        f64_arr(vec![8], &dst_source_vals()),
        None,
    );
    let err = store
        .add_time_series(
            3,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::Deterministic(
                Deterministic::new(
                    initial,
                    resolution,
                    horizon,
                    interval,
                    2,
                    f64_arr(vec![2, 2], &[0.0, 1.0, 2.0, 3.0]),
                    "load",
                )
                .unwrap(),
            ),
            Features::new(),
        )
        .unwrap_err();
    assert!(
        matches!(&err, TimeSeriesError::InvalidParameter(msg)
            if msg.contains("cannot add Deterministic")),
        "adding Deterministic over a DST family should error, got {err:?}"
    );

    // The reverse: transforming a SingleTimeSeries into a DST is rejected when a
    // Deterministic of the same family already exists.
    let mut store = create_store(None, true).unwrap();
    add_forecast(
        &mut store,
        5,
        "load",
        TimeSeriesType::Deterministic,
        initial,
        resolution,
        horizon,
        interval,
        2,
        f64_arr(vec![2, 2], &[0.0, 1.0, 2.0, 3.0]),
        None,
    );
    store
        .add_time_series(
            5,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                resolution,
                f64_arr(vec![8], &dst_source_vals()),
                "load",
            )),
            Features::new(),
        )
        .unwrap();
    let err = store
        .transform_single_time_series(horizon, interval, None, None, Default::default())
        .unwrap_err();
    assert!(
        matches!(&err, TimeSeriesError::InvalidParameter(msg)
            if msg.contains("cannot derive DeterministicSingleTimeSeries")),
        "deriving a DST over a Deterministic family should error, got {err:?}"
    );
}

/// The `owner_category` and `resolution` arguments select which SingleTimeSeries
/// are transformed. Both are applied as SQL predicates, so this pins that the
/// narrowing still matches (and only matches) the intended rows, and that the
/// transform is idempotent: a second identical call derives nothing new.
#[test]
fn transform_honors_owner_category_and_resolution_filters() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let hourly = Duration::hours(1);
    let daily = Duration::days(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(1);

    let mut store = create_store(None, true).unwrap();
    let vals = f64_arr(vec![8], &dst_source_vals());
    let add_sts = |store: &mut Store, owner: i64, cat: OwnerCategory, res: Duration| {
        store
            .add_time_series(
                owner,
                "Generator",
                cat,
                TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                    initial,
                    res,
                    vals.clone(),
                    "load",
                )),
                Features::new(),
            )
            .unwrap();
    };
    // Three sources: an hourly component, a daily component, and an hourly
    // supplemental attribute. Only the first matches the filters below.
    add_sts(&mut store, 1, OwnerCategory::Component, hourly);
    add_sts(&mut store, 2, OwnerCategory::Component, daily);
    add_sts(&mut store, 3, OwnerCategory::SupplementalAttribute, hourly);

    let n = store
        .transform_single_time_series(
            horizon,
            interval,
            Some(OwnerCategory::Component),
            Some(hourly.into()),
            Default::default(),
        )
        .unwrap()
        .transformed;
    assert_eq!(
        n, 1,
        "only the hourly component should transform; the daily component and the \
         supplemental attribute are excluded by the filters"
    );

    // Idempotent: the one derived DST is recognized and not re-derived.
    let again = store
        .transform_single_time_series(
            horizon,
            interval,
            Some(OwnerCategory::Component),
            Some(hourly.into()),
            Default::default(),
        )
        .unwrap()
        .transformed;
    assert_eq!(
        again, 0,
        "re-running the same transform should derive nothing"
    );

    // Widening to the other category picks up the attribute, and leaves the
    // already-transformed component alone.
    let attrs = store
        .transform_single_time_series(
            horizon,
            interval,
            Some(OwnerCategory::SupplementalAttribute),
            Some(hourly.into()),
            Default::default(),
        )
        .unwrap()
        .transformed;
    assert_eq!(
        attrs, 1,
        "the supplemental attribute should transform on its own pass"
    );

    // The daily source was never touched by any of the above.
    let daily_keys = store
        .get_time_series_keys(2, OwnerCategory::Component)
        .unwrap();
    assert!(
        daily_keys
            .iter()
            .all(|k| k.time_series_type() != TimeSeriesType::DeterministicSingleTimeSeries),
        "the daily component must not have been transformed, got {daily_keys:?}"
    );
}

/// Re-transforming at the same interval with a *different* horizon must error,
/// not silently skip: the DST identity does not include the horizon, so the
/// requested view cannot coexist with the existing one, and skipping would
/// report success while the old horizon kept serving reads. Same horizon stays
/// idempotent, and a different interval is a legitimately distinct view.
#[test]
fn transform_rejects_horizon_change_at_same_interval() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let interval = Duration::hours(1);

    let mut store = create_store(None, true).unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                resolution,
                f64_arr(vec![8], &dst_source_vals()),
                "load",
            )),
            Features::new(),
        )
        .unwrap();

    assert_eq!(
        store
            .transform_single_time_series(
                Duration::hours(2),
                interval,
                None,
                None,
                Default::default()
            )
            .unwrap()
            .transformed,
        1
    );
    // Same horizon + interval: idempotent no-op.
    assert_eq!(
        store
            .transform_single_time_series(
                Duration::hours(2),
                interval,
                None,
                None,
                Default::default()
            )
            .unwrap()
            .transformed,
        0
    );
    // Different horizon at the same interval: hard error.
    let err = store
        .transform_single_time_series(Duration::hours(3), interval, None, None, Default::default())
        .unwrap_err();
    assert!(
        matches!(&err, TimeSeriesError::InvalidParameter(msg)
            if msg.contains("horizon PT2H already exists (requested PT3H)")),
        "expected a horizon-mismatch error, got {err:?}"
    );
    // Different interval: a distinct view, derived alongside the first.
    assert_eq!(
        store
            .transform_single_time_series(
                Duration::hours(3),
                Duration::hours(2),
                None,
                None,
                Default::default()
            )
            .unwrap()
            .transformed,
        1
    );
}

#[test]
fn count_array_references_counts_sts_and_dst() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(1);
    let mut store = create_store(None, true).unwrap();

    // Transforming an STS leaves an STS and a DST sharing one underlying array.
    let dst_key = add_forecast(
        &mut store,
        5,
        "ref",
        TimeSeriesType::DeterministicSingleTimeSeries,
        initial,
        resolution,
        horizon,
        interval,
        0,
        f64_arr(vec![8], &dst_source_vals()),
        None,
    );
    let meta = store.get_metadata(dst_key.identity()).unwrap();
    let (sts, dst) = store.count_array_references(&meta.data_hash).unwrap();
    assert_eq!(
        (sts, dst),
        (1, 1),
        "one STS and one DST reference the array"
    );
}

// ---------------------------------------------------------------------------
// Calendar (`Period::Months`) forecasts
//
// Nothing else exercises a calendar period as a forecast horizon or interval —
// only static resolutions. A `Months` grid is *not* a fixed span, so every
// window-index computation goes down the calendar-aware branch of
// `Period::{add_to, steps_between, floor_steps}`.
// ---------------------------------------------------------------------------

/// A Deterministic on a monthly grid: resolution P1M, horizon P3M (H = 3),
/// interval P1M, 4 windows. Initial timestamp is the 15th so a naive
/// month-arithmetic implementation that normalizes to the 1st would be caught.
fn monthly_det(initial: chrono::DateTime<chrono::Utc>, count: usize) -> Deterministic {
    // vals[h][w] = h * 100 + w
    let vals: Vec<f64> = (0..3_usize)
        .flat_map(|h| (0..count).map(move |w| (h * 100 + w) as f64))
        .collect();
    Deterministic::new(
        initial,
        Period::Months(1),
        Period::Months(3),
        Period::Months(1),
        count,
        f64_arr(vec![3, count], &vals),
        "monthly_forecast",
    )
    .unwrap()
}

#[test]
fn monthly_deterministic_round_trips_on_both_backends() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
    for_each_backend(
        move |store| {
            store
                .add_time_series(
                    9,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::Deterministic(monthly_det(initial, 4)),
                    Features::new(),
                )
                .unwrap()
        },
        move |store, key, backend| {
            // The calendar periods survive the ISO-8601 encoding as `Months`,
            // never collapsing into an equivalent-looking `Fixed` span.
            assert_eq!(key.resolution(), Some(Period::Months(1)), "{backend}");
            assert_eq!(key.interval(), Some(Period::Months(1)), "{backend}");

            let got = store.get_time_series(key.identity(), None).unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.resolution, Period::Months(1), "{backend}");
            assert_eq!(det.horizon, Period::Months(3), "{backend}");
            assert_eq!(det.interval, Period::Months(1), "{backend}");
            assert_eq!(det.count, 4, "{backend}");
            assert_eq!(det.initial_timestamp, initial, "{backend}");
            assert_eq!(det.data.shape, vec![3, 4], "{backend}");

            let params = store.get_forecast_parameters(None, None).unwrap();
            assert_eq!(params.horizon, Some(Period::Months(3)), "{backend}");
            assert_eq!(params.interval, Some(Period::Months(1)), "{backend}");
        },
    );
}

#[test]
fn monthly_deterministic_window_selection_at_calendar_boundaries() {
    // Windows start at 2024-01-15, 02-15, 03-15, 04-15 — spans of 31, 29
    // (leap February), and 31 days. A fixed-span implementation cannot land on
    // all four.
    let initial = Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
    let w = |m: u32| Utc.with_ymd_and_hms(2024, m, 15, 0, 0, 0).unwrap();

    for_each_backend(
        move |store| {
            store
                .add_time_series(
                    9,
                    "Generator",
                    OwnerCategory::Component,
                    TimeSeriesData::Deterministic(monthly_det(initial, 4)),
                    Features::new(),
                )
                .unwrap()
        },
        move |store, key, backend| {
            // Select windows 1..3 (Feb 15 and Mar 15).
            let got = store
                .get_time_series(key.identity(), Some((w(2), w(4))))
                .unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.count, 2, "{backend}: two windows selected");
            assert_eq!(
                det.initial_timestamp,
                w(2),
                "{backend}: sliced initial is the first selected window"
            );
            // vals[h][w] for w in {1, 2}: h*100 + w.
            assert_eq!(
                det.data.to_f64_vec().unwrap(),
                vec![1.0, 2.0, 101.0, 102.0, 201.0, 202.0],
                "{backend}"
            );

            // The last window on its own boundary.
            let got = store
                .get_time_series(key.identity(), Some((w(4), w(5))))
                .unwrap();
            assert_eq!(got.as_deterministic().unwrap().count, 1, "{backend}");

            // Off-grid start: the 20th is not a calendar step from the 15th.
            let off = Utc.with_ymd_and_hms(2024, 2, 20, 0, 0, 0).unwrap();
            let err = store
                .get_time_series(key.identity(), Some((off, w(4))))
                .unwrap_err();
            assert!(
                matches!(err, infrastore_core::TimeSeriesError::InvalidParameter(_)),
                "{backend}: off-grid monthly start must be rejected, got {err:?}"
            );

            // A start past the last window is rejected, not silently empty.
            let past = Utc.with_ymd_and_hms(2024, 5, 15, 0, 0, 0).unwrap();
            assert!(
                store
                    .get_time_series(key.identity(), Some((past, past + Duration::days(31))))
                    .is_err(),
                "{backend}: start past the last window must be rejected"
            );
        },
    );
}

#[test]
fn monthly_deterministic_end_of_month_initial_timestamp() {
    // 2024-01-31 + 1 calendar month is 2024-02-29 (clamped), and + 2 is
    // 2024-03-31. `steps_between` verifies the exact landing, so the clamped
    // window boundary must be addressable and the "unclamped" 03-29 must not be.
    let initial = Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap();
    let mut store = create_store(None, true).unwrap();
    let key = store
        .add_time_series(
            9,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::Deterministic(monthly_det(initial, 3)),
            Features::new(),
        )
        .unwrap();

    let feb29 = Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap();
    let apr = Utc.with_ymd_and_hms(2024, 4, 30, 0, 0, 0).unwrap();
    let got = store
        .get_time_series(key.identity(), Some((feb29, apr)))
        .unwrap();
    let det = got.as_deterministic().unwrap();
    assert_eq!(det.initial_timestamp, feb29, "clamped boundary is window 1");
    assert_eq!(det.count, 2);

    // 2024-03-29 is not on the grid (window 2 is 03-31).
    let mar29 = Utc.with_ymd_and_hms(2024, 3, 29, 0, 0, 0).unwrap();
    assert!(
        store
            .get_time_series(key.identity(), Some((mar29, apr)))
            .is_err(),
        "an unclamped day-of-month must not be treated as a window boundary"
    );
}

#[test]
fn transform_single_time_series_on_a_monthly_grid() {
    // A monthly SingleTimeSeries transformed into a DST at horizon P3M,
    // interval P1M. `H = 3`, `interval_steps = 1`, so window k covers source
    // months k..k+3 and the last valid window is `length - H`.
    let initial = Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
    let values: Vec<f64> = (0..12).map(|i| 100.0 + i as f64).collect();
    let mut store = create_store(None, true).unwrap();
    store
        .add_time_series(
            4,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                Period::Months(1),
                f64_arr(vec![12], &values),
                "monthly_load",
            )),
            Features::new(),
        )
        .unwrap();

    let n = store
        .transform_single_time_series(
            Period::Months(3),
            Period::Months(1),
            None,
            None,
            Default::default(),
        )
        .unwrap()
        .transformed;
    assert_eq!(n, 1, "one series transformed");

    let dst_keys = store
        .list_keys(
            ListFilter::new().time_series_type(TimeSeriesType::DeterministicSingleTimeSeries),
        )
        .unwrap();
    assert_eq!(dst_keys.len(), 1);
    assert_eq!(dst_keys[0].resolution(), Some(Period::Months(1)));
    assert_eq!(dst_keys[0].interval(), Some(Period::Months(1)));

    // Reads back as a Deterministic view (storage-level view, by design).
    let got = store.get_time_series(dst_keys[0].identity(), None).unwrap();
    let det = got.as_deterministic().unwrap();
    assert_eq!(det.resolution, Period::Months(1));
    assert_eq!(det.horizon, Period::Months(3));
    assert_eq!(det.interval, Period::Months(1));
    // 12 source months, H = 3 -> windows 0..=9, i.e. 10 windows.
    assert_eq!(det.count, 10);
    assert_eq!(det.data.shape, vec![3, 10]);
    // Window k, horizon step h -> source index k + h.
    let vals = det.data.to_f64_vec().unwrap();
    for h in 0..3_usize {
        for k in 0..10_usize {
            assert_eq!(
                vals[h * 10 + k],
                values[k + h],
                "window {k}, horizon step {h}"
            );
        }
    }

    // Window selection on the calendar grid: March 15 is window 2.
    let mar = Utc.with_ymd_and_hms(2024, 3, 15, 0, 0, 0).unwrap();
    let apr = Utc.with_ymd_and_hms(2024, 4, 15, 0, 0, 0).unwrap();
    let got = store
        .get_time_series(dst_keys[0].identity(), Some((mar, apr)))
        .unwrap();
    let det = got.as_deterministic().unwrap();
    assert_eq!(det.count, 1);
    assert_eq!(det.initial_timestamp, mar);
    assert_eq!(
        det.data.to_f64_vec().unwrap(),
        vec![values[2], values[3], values[4]]
    );
}

#[test]
fn forecast_reader_sweeps_a_monthly_grid() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("monthly_forecast.h5");
    {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        store
            .add_time_series(
                9,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::Deterministic(monthly_det(initial, 4)),
                Features::new(),
            )
            .unwrap();
        store.flush().unwrap();
    }
    let store = open_store(path.as_path(), true).unwrap();
    let mut reader = store
        .build_forecast_reader(
            ListFilter::new()
                .time_series_type(TimeSeriesType::Deterministic)
                .resolution(Period::Months(1)),
        )
        .unwrap();
    assert_eq!(reader.resolution(), Period::Months(1));
    assert_eq!(reader.interval(), Period::Months(1));
    assert_eq!(reader.count(), 4);

    // The reader's own timeline is calendar-aware.
    let expected: Vec<_> = (1..=4)
        .map(|m| Utc.with_ymd_and_hms(2024, m, 15, 0, 0, 0).unwrap())
        .collect();
    assert_eq!(reader.timestamps().collect::<Vec<_>>(), expected);

    // Sweep every window; slot values must equal vals[h][k] = h*100 + k.
    for (k, at) in expected.iter().enumerate() {
        store.forecast_read(&mut reader, *at).unwrap();
        assert_eq!(reader.window_index(*at).unwrap(), k);
        let slot = reader.entry_slot(0);
        assert_eq!(slot.window_shape(), &[3], "window {k} shape");
        let got = slot.window_to_vec::<f64>().unwrap();
        let want: Vec<f64> = (0..3).map(|h| (h * 100 + k) as f64).collect();
        assert_eq!(got, want, "window {k}");
    }

    // Off-grid (mid-month) is a hard error, not a rounded read.
    assert!(
        store
            .forecast_read(
                &mut reader,
                Utc.with_ymd_and_hms(2024, 2, 20, 0, 0, 0).unwrap()
            )
            .is_err()
    );
}

#[test]
fn monthly_and_fixed_periods_never_over_match() {
    // A `Fixed` 30-day resolution and a `Months(1)` resolution are distinct
    // catalog keys even though their spans coincide for some months. This is
    // what keeps a monthly series from being served to a fixed-span query.
    let initial = Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
    let mut store = create_store(None, true).unwrap();
    for (res, name) in [
        (Period::Months(1), "monthly"),
        (Period::fixed(Duration::days(30)), "thirty_day"),
    ] {
        store
            .add_time_series(
                1,
                "Generator",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                    initial,
                    res,
                    f64_arr(vec![4], &[1.0, 2.0, 3.0, 4.0]),
                    name,
                )),
                Features::new(),
            )
            .unwrap();
    }

    let monthly = store
        .list_keys(ListFilter::new().resolution(Period::Months(1)))
        .unwrap();
    assert_eq!(monthly.len(), 1);
    assert_eq!(monthly[0].name(), "monthly");

    let fixed = store
        .list_keys(ListFilter::new().resolution(Period::fixed(Duration::days(30))))
        .unwrap();
    assert_eq!(fixed.len(), 1);
    assert_eq!(fixed[0].name(), "thirty_day");

    let mut resolutions = store.get_resolutions(None).unwrap();
    resolutions.sort_by_key(|p| p.to_iso8601());
    assert_eq!(
        resolutions,
        vec![Period::Months(1), Period::fixed(Duration::days(30))]
    );
}

#[test]
fn non_positive_forecast_periods_are_rejected_through_the_add_path() {
    // `validate_positive_periods` runs inside the forecast constructors, so a
    // zero or negative resolution/horizon/interval can never reach the store.
    // This pins that the *only* way in is blocked, in both period kinds.
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let data = f64_arr(vec![2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);

    let zero = Period::fixed(Duration::zero());
    let neg = Period::fixed(Duration::hours(-1));
    let ok = Period::fixed(Duration::hours(1));
    let ok_h = Period::fixed(Duration::hours(2));

    for (res, hor, iv) in [
        (zero, ok_h, ok),
        (neg, ok_h, ok),
        (ok, zero, ok),
        (ok, neg, ok),
        (ok, ok_h, zero),
        (ok, ok_h, neg),
        (Period::Months(0), Period::Months(3), Period::Months(1)),
        (Period::Months(-1), Period::Months(3), Period::Months(1)),
        (Period::Months(1), Period::Months(0), Period::Months(1)),
        (Period::Months(1), Period::Months(3), Period::Months(0)),
    ] {
        assert!(
            Deterministic::new(initial, res, hor, iv, 3, data.clone(), "bad").is_err(),
            "Deterministic accepted non-positive periods {res:?}/{hor:?}/{iv:?}"
        );
    }

    // `transform_single_time_series` is the other forecast-creating entry
    // point; it must reject a non-positive horizon/interval too.
    let mut store = create_store(None, true).unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                Duration::hours(1),
                f64_arr(vec![8], &dst_source_vals()),
                "load",
            )),
            Features::new(),
        )
        .unwrap();
    assert!(
        store
            .transform_single_time_series(
                Duration::zero(),
                Duration::hours(1),
                None,
                None,
                Default::default()
            )
            .is_err(),
        "a zero horizon must be rejected"
    );
    assert!(
        store
            .transform_single_time_series(
                Duration::hours(2),
                Duration::zero(),
                None,
                None,
                Default::default()
            )
            .is_err(),
        "a zero interval must be rejected"
    );
    assert!(
        store
            .transform_single_time_series(
                Duration::hours(-2),
                Duration::hours(1),
                None,
                None,
                Default::default()
            )
            .is_err(),
        "a negative horizon must be rejected"
    );
}

// ---------------------------------------------------------------------------
// A `Deterministic` catalog *filter* (list/has/aggregate queries) spanning both
// storage forms, and zero-interval single-window forecasts.
// ---------------------------------------------------------------------------

/// A `Deterministic` `ListFilter` matches a stored `Deterministic` and a stored
/// `DeterministicSingleTimeSeries`, and nothing else — across the listing,
/// existence, distinct-interval, and owner-id query paths.
#[test]
fn deterministic_filter_matches_both_storage_forms() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(2);
    let interval = Duration::hours(1);

    for_each_backend(
        |store| {
            // Owner 1: a real Deterministic. Owner 2: a DST derived from an
            // STS. Owner 3: a Probabilistic that must NOT match the family.
            add_forecast(
                store,
                1,
                "load",
                TimeSeriesType::Deterministic,
                initial,
                resolution,
                horizon,
                interval,
                3,
                f64_arr(vec![2, 3], &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]),
                None,
            );
            add_forecast(
                store,
                2,
                "wind",
                TimeSeriesType::DeterministicSingleTimeSeries,
                initial,
                resolution,
                horizon,
                interval,
                7,
                f64_arr(vec![8], &dst_source_vals()),
                None,
            );
            add_forecast(
                store,
                3,
                "prob",
                TimeSeriesType::Probabilistic,
                initial,
                resolution,
                horizon,
                interval,
                3,
                f64_arr(
                    vec![2, 2, 3],
                    &(0..12).map(|i| i as f64).collect::<Vec<_>>(),
                ),
                Some(vec![0.25, 0.75]),
            );
        },
        |store, _, backend| {
            let family = ListFilter::new().time_series_type(TimeSeriesType::Deterministic);
            let keys = store.list_keys(family.clone()).unwrap();
            assert_eq!(keys.len(), 2, "{backend}: family list matches Det + DST");
            assert!(
                store.has_any_time_series(family).unwrap(),
                "{backend}: family existence probe"
            );
            let mut ids = store
                .list_owner_ids(
                    OwnerCategory::Component,
                    Some(TimeSeriesType::Deterministic),
                    None,
                )
                .unwrap();
            ids.sort();
            assert_eq!(ids, vec![1, 2], "{backend}: family owner ids");
            let intervals = store
                .get_intervals(Some(TimeSeriesType::Deterministic))
                .unwrap();
            assert_eq!(
                intervals,
                vec![Period::from(interval)],
                "{backend}: family intervals"
            );
        },
    );
}

/// A single-window forecast (`count == 1`) may carry a zero interval: it round
/// trips, and a `time_range` anchored at `initial` selects its only window.
#[test]
fn zero_interval_single_window_forecast_round_trips() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(4);
    let vals: Vec<f64> = (0..4).map(|i| i as f64).collect();

    for_each_backend(
        |store| {
            add_forecast(
                store,
                1,
                "load",
                TimeSeriesType::Deterministic,
                initial,
                resolution,
                horizon,
                Duration::zero(),
                1,
                f64_arr(vec![4, 1], &vals),
                None,
            )
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.count, 1, "{backend}: count");
            assert!(det.interval.is_zero(), "{backend}: zero interval preserved");
            assert_eq!(det.data.to_f64_vec().unwrap(), vals, "{backend}: values");

            // The only valid windowed read starts at `initial`.
            let sliced = store
                .get_time_series(key.identity(), Some((initial, initial + horizon)))
                .unwrap();
            assert_eq!(
                sliced.as_deterministic().unwrap().count,
                1,
                "{backend}: range read selects the single window"
            );
            assert!(
                store
                    .get_time_series(
                        key.identity(),
                        Some((initial + Duration::hours(1), initial + horizon)),
                    )
                    .is_err(),
                "{backend}: an off-initial start is rejected"
            );
        },
    );
}

/// A transform that derives exactly one window (`interval == horizon`) records
/// the requested interval verbatim — the same encoding a directly-added
/// single-window forecast uses, so a client that looks the view up by its
/// horizon finds it. It stays idempotent under the same arguments and reads
/// back.
#[test]
fn single_window_transform_stores_the_requested_interval_and_stays_idempotent() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(8);
    let mut store = create_store(None, true).unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                resolution,
                f64_arr(vec![8], &dst_source_vals()),
                "load",
            )),
            Features::new(),
        )
        .unwrap();

    // horizon spans the whole series => one window.
    assert_eq!(
        store
            .transform_single_time_series(horizon, horizon, None, None, Default::default())
            .unwrap()
            .transformed,
        1
    );
    let key = store
        .list_keys(
            ListFilter::new().time_series_type(TimeSeriesType::DeterministicSingleTimeSeries),
        )
        .unwrap()
        .pop()
        .unwrap();
    let got = store.get_time_series(key.identity(), None).unwrap();
    let det = got.as_deterministic().unwrap();
    assert_eq!(det.count, 1);
    assert_eq!(
        det.interval,
        Period::from(horizon),
        "the requested interval is stored verbatim, not collapsed to zero"
    );
    assert_eq!(det.data.to_f64_vec().unwrap(), dst_source_vals());

    // Re-running with the same arguments derives nothing new.
    assert_eq!(
        store
            .transform_single_time_series(horizon, horizon, None, None, Default::default())
            .unwrap()
            .transformed,
        0
    );
}

/// A single window derived at an interval *smaller* than the horizon keeps
/// that interval too: the transform never rewrites the requested interval.
#[test]
fn single_window_transform_at_a_smaller_interval_keeps_it() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(8); // spans the whole series => count == 1
    let interval = Duration::hours(1);
    let mut store = create_store(None, true).unwrap();
    store
        .add_time_series(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                resolution,
                f64_arr(vec![8], &dst_source_vals()),
                "load",
            )),
            Features::new(),
        )
        .unwrap();
    assert_eq!(
        store
            .transform_single_time_series(horizon, interval, None, None, Default::default())
            .unwrap()
            .transformed,
        1
    );
    let key = store
        .list_keys(
            ListFilter::new().time_series_type(TimeSeriesType::DeterministicSingleTimeSeries),
        )
        .unwrap()
        .pop()
        .unwrap();
    let got = store.get_time_series(key.identity(), None).unwrap();
    let det = got.as_deterministic().unwrap();
    assert_eq!(det.count, 1);
    assert_eq!(det.interval, Period::from(interval));
}

// ---------------------------------------------------------------------------
// TransformPolicy: the rules InfrastructureSystems.jl opts into.
//
// Every test above runs with `TransformPolicy::default()` — the permissive
// behavior. These cover the opted-in rules, which are what moved out of the
// InfrastructureSystems.jl per-series validation loop and into the core.
// ---------------------------------------------------------------------------

/// Add one `SingleTimeSeries` of `len` hourly points starting at `initial`.
fn add_hourly_sts(
    store: &mut Store,
    owner: i64,
    name: &str,
    initial: chrono::DateTime<Utc>,
    resolution: Duration,
    len: usize,
) {
    let vals: Vec<f64> = (0..len).map(|i| i as f64).collect();
    store
        .add_time_series(
            owner,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                resolution,
                f64_arr(vec![len], &vals),
                name,
            )),
            Features::new(),
        )
        .unwrap();
}

fn is_policy() -> infrastore_core::TransformPolicy {
    infrastore_core::TransformPolicy {
        dry_run: false,
        normalize_single_window: true,
        require_uniform_forecast_grid: true,
    }
}

/// Under `normalize_single_window` a single-window request is stored as the
/// zero interval instead of verbatim — the encoding IS looks views up by. The
/// case is reported either way.
#[test]
fn normalize_single_window_stores_the_zero_interval() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let horizon = Duration::hours(8);
    let mut store = create_store(None, true).unwrap();
    add_hourly_sts(&mut store, 1, "load", initial, Duration::hours(1), 8);

    let outcome = store
        .transform_single_time_series(horizon, horizon, None, None, is_policy())
        .unwrap();
    assert_eq!(outcome.transformed, 1);
    assert_eq!(outcome.sources, 1);
    assert!(
        outcome.interval_normalized,
        "the horizon spans the series, so this is the single-window case"
    );
    assert_eq!(
        outcome.interval,
        Period::zero(),
        "IS stores the single-window interval as zero, not verbatim"
    );

    let key = store
        .list_keys(
            ListFilter::new().time_series_type(TimeSeriesType::DeterministicSingleTimeSeries),
        )
        .unwrap()
        .pop()
        .unwrap();
    let det = store
        .get_time_series(key.identity(), None)
        .unwrap()
        .as_deterministic()
        .unwrap()
        .clone();
    assert_eq!(det.count, 1);
    assert_eq!(det.interval, Period::zero());
}

/// An interval longer than the horizon would leave gaps between windows. The
/// permissive policy derives it anyway (the historical core behavior); IS's
/// policy is not what rejects it — the check is unconditional.
#[test]
fn an_interval_longer_than_the_horizon_is_rejected() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let mut store = create_store(None, true).unwrap();
    add_hourly_sts(&mut store, 1, "load", initial, Duration::hours(1), 24);

    let err = store
        .transform_single_time_series(
            Duration::hours(4),
            Duration::hours(6),
            None,
            None,
            is_policy(),
        )
        .unwrap_err();
    assert!(
        matches!(&err, infrastore_core::TimeSeriesError::InvalidParameter(m)
            if m.contains("longer than the horizon")),
        "got {err:?}"
    );
}

/// `require_uniform_forecast_grid` rejects a transform whose derived grid
/// disagrees with a forecast already stored at the same (resolution, interval).
/// This is the check that was `check_params_compatibility` in IS.
#[test]
fn uniform_grid_policy_rejects_a_count_mismatch_with_a_stored_forecast() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let horizon = Duration::hours(4);
    let interval = Duration::hours(1);
    let mut store = create_store(None, true).unwrap();

    // A real Deterministic with 2 windows.
    add_forecast(
        &mut store,
        1,
        "existing",
        TimeSeriesType::Deterministic,
        initial,
        resolution,
        horizon,
        interval,
        2,
        f64_arr(vec![4, 2], &[0.0; 8]),
        None,
    );
    // An STS long enough to derive many more than 2 windows.
    add_hourly_sts(&mut store, 2, "load", initial, resolution, 24);

    let err = store
        .transform_single_time_series(horizon, interval, None, None, is_policy())
        .unwrap_err();
    assert!(
        matches!(&err, infrastore_core::TimeSeriesError::InvalidParameter(m)
            if m.contains("does not match the stored forecast count")),
        "got {err:?}"
    );

    // The permissive policy allows exactly this — it is a client rule, not a
    // storage invariant.
    store
        .transform_single_time_series(horizon, interval, None, None, Default::default())
        .unwrap();
}

/// Two resolutions that derive different window counts would produce two
/// forecast grids. IS forbids that; the default policy allows it.
#[test]
fn uniform_grid_policy_rejects_resolutions_that_derive_different_counts() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let mut store = create_store(None, true).unwrap();
    // 24 hourly points and 24 two-hourly points: same horizon, different counts.
    add_hourly_sts(&mut store, 1, "hourly", initial, Duration::hours(1), 24);
    add_hourly_sts(&mut store, 2, "two_hourly", initial, Duration::hours(2), 24);

    let err = store
        .transform_single_time_series(
            Duration::hours(4),
            Duration::hours(2),
            None,
            None,
            is_policy(),
        )
        .unwrap_err();
    assert!(
        matches!(&err, infrastore_core::TimeSeriesError::InvalidParameter(m)
            if m.contains("different window")),
        "got {err:?}"
    );

    let outcome = store
        .transform_single_time_series(
            Duration::hours(4),
            Duration::hours(2),
            None,
            None,
            Default::default(),
        )
        .unwrap();
    assert_eq!(
        outcome.transformed, 2,
        "the permissive policy derives each resolution on its own grid"
    );
}

/// Series at one resolution that disagree on `(initial_timestamp, length)` are
/// rejected before anything is written — the grid check that replaced IS's
/// per-series count/initial-timestamp loop. This one is unconditional: it is a
/// property of the static data, not a client rule.
#[test]
fn a_divergent_static_grid_is_rejected() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let mut store = create_store(None, true).unwrap();
    add_hourly_sts(&mut store, 1, "a", initial, resolution, 24);
    add_hourly_sts(&mut store, 2, "b", initial, resolution, 12);

    let err = store
        .transform_single_time_series(
            Duration::hours(4),
            Duration::hours(1),
            None,
            None,
            is_policy(),
        )
        .unwrap_err();
    assert!(
        matches!(&err, infrastore_core::TimeSeriesError::IntegrityError(m)
            if m.contains("more than one")),
        "got {err:?}"
    );
    assert_eq!(
        store
            .list_keys(
                ListFilter::new().time_series_type(TimeSeriesType::DeterministicSingleTimeSeries)
            )
            .unwrap()
            .len(),
        0,
        "nothing is written when the grid check fails"
    );
}

/// An empty store reports zero sources rather than erroring, so the caller can
/// tell "nothing to do" from "something was wrong".
#[test]
fn a_store_with_no_single_time_series_reports_zero_sources() {
    let mut store = create_store(None, true).unwrap();
    let outcome = store
        .transform_single_time_series(
            Duration::hours(4),
            Duration::hours(1),
            None,
            None,
            is_policy(),
        )
        .unwrap();
    assert_eq!(outcome.sources, 0);
    assert_eq!(outcome.transformed, 0);
    assert!(!outcome.interval_normalized);
}

/// The grid check is scoped to the transform's owner category, so a
/// supplemental attribute on a different grid does not fail a component-only
/// transform.
#[test]
fn the_grid_check_is_scoped_to_the_owner_category() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let mut store = create_store(None, true).unwrap();
    add_hourly_sts(&mut store, 1, "component", initial, resolution, 24);
    let vals: Vec<f64> = (0..12).map(|i| i as f64).collect();
    store
        .add_time_series(
            9,
            "Outage",
            OwnerCategory::SupplementalAttribute,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                initial,
                resolution,
                f64_arr(vec![12], &vals),
                "attr",
            )),
            Features::new(),
        )
        .unwrap();

    let outcome = store
        .transform_single_time_series(
            Duration::hours(4),
            Duration::hours(1),
            Some(OwnerCategory::Component),
            None,
            is_policy(),
        )
        .unwrap();
    assert_eq!(
        outcome.transformed, 1,
        "the attribute's divergent grid is out of scope for a component transform"
    );
}

/// A dry run answers "would this transform succeed?" — every check runs, the
/// verdict is reported, and nothing is written.
#[test]
fn a_dry_run_validates_without_writing() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let mut store = create_store(None, true).unwrap();
    add_hourly_sts(&mut store, 1, "load", initial, Duration::hours(1), 24);

    let policy = infrastore_core::TransformPolicy {
        dry_run: true,
        ..is_policy()
    };
    let outcome = store
        .transform_single_time_series(Duration::hours(4), Duration::hours(1), None, None, policy)
        .unwrap();
    assert_eq!(
        outcome.transformed, 1,
        "reports the count a committing run would produce"
    );
    assert_eq!(outcome.sources, 1);
    assert_eq!(
        store
            .list_keys(
                ListFilter::new().time_series_type(TimeSeriesType::DeterministicSingleTimeSeries)
            )
            .unwrap()
            .len(),
        0,
        "a dry run writes nothing"
    );

    // A failing transform still fails as a dry run, before any write.
    let err = store
        .transform_single_time_series(Duration::hours(4), Duration::hours(6), None, None, policy)
        .unwrap_err();
    assert!(matches!(
        err,
        infrastore_core::TimeSeriesError::InvalidParameter(_)
    ));

    // And the committing run still works afterwards.
    assert_eq!(
        store
            .transform_single_time_series(
                Duration::hours(4),
                Duration::hours(1),
                None,
                None,
                is_policy(),
            )
            .unwrap()
            .transformed,
        1
    );
}
