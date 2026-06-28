//! Tests for the forecast read path in `Store::get_time_series`.
//!
//! All cases run against BOTH backends via [`for_each_backend`]: the in-memory
//! store, and a NetCDF store that is flushed, closed, and reopened read-only
//! (exercising the persisted format).
//!
//! Dense forecasts (`Deterministic` / `Probabilistic` / `Scenarios`) are written
//! through the generic `Store::add_time_series`; `DeterministicSingleTimeSeries`
//! is derived from a stored `SingleTimeSeries` via
//! `Store::transform_single_time_series`. Read results are returned as
//! `TimeSeriesData::{Deterministic,Probabilistic,Scenarios}` variants; DST is
//! synthesized into `Deterministic`.

use chrono::{Duration, TimeZone, Utc};
use time_series_store_core::{
    Deterministic, Dtype, Features, ForecastTimeSeriesKey, OwnerCategory, Period, Probabilistic,
    Scenarios, SingleTimeSeries, Store, TimeSeriesData, TimeSeriesKey, TimeSeriesType, TypedArray,
    create_store, open_store,
};

// Re-export slice_count_axis through a test-visible path.  The helper is
// `pub(crate)` in store.rs; expose it here by calling it via the public API
// indirectly, but for the unit tests (case 9) we test it directly using a
// thin wrapper below.
mod slice_axis {
    use time_series_store_core::TypedArray;

    // Use the test-crate access: we compile with `#[cfg(test)]` integration
    // tests which can see public items only.  `slice_count_axis` is
    // `pub(crate)` so we can't call it from an integration test directly.
    // Instead we expose the same logic inline here so we can test the
    // algorithm independently.
    pub fn slice_count_axis(arr: &TypedArray, axis: usize, w0: usize, w1: usize) -> TypedArray {
        assert!(axis < arr.shape.len());
        assert!(w0 <= w1);
        let axis_len = arr.shape[axis];
        assert!(w1 <= axis_len);

        let outer: usize = arr.shape[..axis].iter().product();
        let inner_bytes: usize = arr.shape[axis + 1..].iter().product::<usize>() * arr.dtype.size();
        let window_bytes = (w1 - w0) * inner_bytes;

        let mut out_bytes = Vec::with_capacity(outer * window_bytes);
        for o in 0..outer {
            let block_start = o * axis_len * inner_bytes;
            let src_start = block_start + w0 * inner_bytes;
            let src_end = block_start + w1 * inner_bytes;
            out_bytes.extend_from_slice(&arr.bytes[src_start..src_end]);
        }

        let mut new_shape = arr.shape.clone();
        new_shape[axis] = w1 - w0;

        TypedArray {
            dtype: arr.dtype,
            shape: new_shape,
            bytes: out_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// Two-backend harness (mirrors indexing.rs exactly)
// ---------------------------------------------------------------------------

fn for_each_backend<T>(populate: impl Fn(&mut Store) -> T, verify: impl Fn(&Store, &T, &str)) {
    // In-memory backend.
    {
        let mut store = create_store(None, true).unwrap();
        let state = populate(&mut store);
        verify(&store, &state, "memory");
    }
    // NetCDF backend: persist, reopen read-only, then read.
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.nc");
        let state = {
            let mut store = create_store(Some(path.as_path()), false).unwrap();
            let state = populate(&mut store);
            store.flush().unwrap();
            state
        };
        let store = open_store(path.as_path(), true).unwrap();
        verify(&store, &state, "netcdf");
    }
}

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
                    None,
                )
                .unwrap();
            store
                .transform_single_time_series(horizon, interval, None, None)
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
            None,
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
// Case 9: Direct unit tests of slice_count_axis on known small arrays
// ---------------------------------------------------------------------------

#[test]
fn slice_count_axis_axis0() {
    // Shape [4]: axis 0 = leading axis, equivalent to leading-axis slicing.
    // vals = [10, 20, 30, 40] (f64).
    let arr = f64_arr(vec![4], &[10.0, 20.0, 30.0, 40.0]);
    let sliced = slice_axis::slice_count_axis(&arr, 0, 1, 3);
    assert_eq!(sliced.shape, vec![2]);
    assert_eq!(sliced.to_f64_vec().unwrap(), vec![20.0, 30.0]);
}

#[test]
fn slice_count_axis_axis1_of_3d() {
    // Simulate Deterministic shape [H=2, C=4, E=1]: [2, 4, 1].
    // vals[s][w][e] = s*100 + w*10 + e
    let vals: Vec<f64> = (0..2_usize)
        .flat_map(|s| {
            (0..4_usize).flat_map(move |w| (0..1_usize).map(move |e| (s * 100 + w * 10 + e) as f64))
        })
        .collect();
    let arr = f64_arr(vec![2, 4, 1], &vals);

    // Select windows w=1..3 along axis 1.
    let sliced = slice_axis::slice_count_axis(&arr, 1, 1, 3);
    assert_eq!(sliced.shape, vec![2, 2, 1]);

    // Expected: s=0, w=1: [10.0], s=0, w=2: [20.0], s=1, w=1: [110.0], s=1, w=2: [120.0]
    let expected = vec![10.0, 20.0, 110.0, 120.0];
    assert_eq!(sliced.to_f64_vec().unwrap(), expected);
}

#[test]
fn slice_count_axis_axis2_of_4d() {
    // Simulate Probabilistic/Scenarios shape [P=2, H=2, C=3]: [2, 2, 3].
    // vals[p][s][w] = p*1000 + s*100 + w*10
    let vals: Vec<f64> = (0..2_usize)
        .flat_map(|p| {
            (0..2_usize)
                .flat_map(move |s| (0..3_usize).map(move |w| (p * 1000 + s * 100 + w * 10) as f64))
        })
        .collect();
    let arr = f64_arr(vec![2, 2, 3], &vals);

    // Select windows w=0..2 (first two) along axis 2.
    let sliced = slice_axis::slice_count_axis(&arr, 2, 0, 2);
    assert_eq!(sliced.shape, vec![2, 2, 2]);

    // p=0, s=0: [0, 10]; p=0, s=1: [100, 110]; p=1, s=0: [1000, 1010]; p=1, s=1: [1100, 1110]
    let expected = vec![0.0, 10.0, 100.0, 110.0, 1000.0, 1010.0, 1100.0, 1110.0];
    assert_eq!(sliced.to_f64_vec().unwrap(), expected);
}

#[test]
fn slice_count_axis_full_range_is_identity() {
    // Slicing the full range should return identical bytes.
    let vals: Vec<f64> = (0..12).map(|i| i as f64).collect();
    let arr = f64_arr(vec![3, 4], &vals);
    let sliced = slice_axis::slice_count_axis(&arr, 1, 0, 4);
    assert_eq!(sliced.shape, arr.shape);
    assert_eq!(sliced.bytes, arr.bytes);
}

#[test]
fn slice_count_axis_empty_range() {
    let arr = f64_arr(vec![2, 4], &[0.0; 8]);
    let sliced = slice_axis::slice_count_axis(&arr, 1, 2, 2);
    assert_eq!(sliced.shape, vec![2, 0]);
    assert!(sliced.bytes.is_empty());
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
// AbstractDeterministic resolution + matched_type
//
// These replace the bindings' former guess-and-retry fallback: the catalog
// resolves the family to one concrete key (whose `time_series_type` is the
// matched type), errors explicitly on ambiguity, and reports a genuine miss as
// `NotFound` rather than masking it.
// ---------------------------------------------------------------------------

use time_series_store_core::{RequestedType, TimeSeriesError};

// Underlying STS values long enough to derive a DST under (H=2, interval=1).
fn dst_source_vals() -> Vec<f64> {
    (0..8).map(|i| i as f64).collect()
}

#[test]
fn resolve_abstract_deterministic_matches_real_deterministic() {
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
                    RequestedType::AbstractDeterministic,
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
fn resolve_abstract_deterministic_matches_dst() {
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
                    RequestedType::AbstractDeterministic,
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
fn resolve_abstract_deterministic_not_found_is_not_masked() {
    let store = create_store(None, true).unwrap();
    let err = store
        .resolve_forecast_key(
            1,
            OwnerCategory::Component,
            "missing",
            Some(Period::Fixed(Duration::hours(1))),
            None,
            Features::new(),
            RequestedType::AbstractDeterministic,
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
            RequestedType::Concrete(TimeSeriesType::Deterministic),
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
            RequestedType::Concrete(TimeSeriesType::Deterministic),
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
            None,
        )
        .unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::InvalidParameter(_)),
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
            None,
        )
        .unwrap();
    let err = store
        .transform_single_time_series(horizon, interval, None, None)
        .unwrap_err();
    assert!(
        matches!(err, TimeSeriesError::InvalidParameter(_)),
        "deriving a DST over a Deterministic family should error, got {err:?}"
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
