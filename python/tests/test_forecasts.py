"""Round-trip tests for forecast types (Deterministic, Probabilistic, Scenarios).

Dense forecasts are written through the generic ``add_time_series`` by passing a
``Deterministic`` / ``Probabilistic`` / ``Scenarios`` object;
``DeterministicSingleTimeSeries`` is derived from a stored ``SingleTimeSeries``
via ``transform_single_time_series``.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from time_series_store import (
    Deterministic,
    InvalidParameterError,
    OwnerCategory,
    Probabilistic,
    Scenarios,
    SingleTimeSeries,
    TimeSeriesStore,
    TimeSeriesType,
)

OWNER_ID = 1
OWNER_TYPE = "Generator"
OWNER_CAT = OwnerCategory.Component

T0 = datetime(2024, 1, 1, tzinfo=timezone.utc)
RES_1H = timedelta(hours=1)
HORIZON_6H = timedelta(hours=6)   # H = 6 steps
INTERVAL_12H = timedelta(hours=12)  # 12-hour window spacing


# ---------------------------------------------------------------------------
# Deterministic
# ---------------------------------------------------------------------------


def test_deterministic_scalar_round_trip():
    """Write a Deterministic forecast [H=6, count=4] and read it back."""
    store = TimeSeriesStore.create(in_memory=True)
    H, C = 6, 4
    data = np.arange(H * C, dtype=np.float64).reshape(H, C)

    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Deterministic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, data, "det_scalar"),
    )

    assert key.time_series_type == TimeSeriesType.Deterministic

    got = store.get_time_series(key)
    assert isinstance(got, Deterministic)
    assert got.count == C
    assert got.horizon == "PT6H"
    assert got.interval == "PT12H"
    assert got.initial_timestamp == T0
    assert got.name == "det_scalar"
    np.testing.assert_array_equal(np.asarray(got.data), data)
    assert np.asarray(got.data).shape == (H, C)


def test_deterministic_multidim_element():
    """Deterministic with per-step element shape [H=4, count=3, E=2] round-trips."""
    store = TimeSeriesStore.create(in_memory=True)
    H, C, E = 4, 3, 2
    horizon = timedelta(hours=H)
    data = np.arange(H * C * E, dtype=np.float32).reshape(H, C, E)

    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Deterministic(T0, RES_1H, horizon, INTERVAL_12H, C, data, "det_multidim"),
    )

    got = store.get_time_series(key)
    assert isinstance(got, Deterministic)
    arr = np.asarray(got.data)
    assert arr.shape == (H, C, E)
    assert arr.dtype == np.float32
    np.testing.assert_array_equal(arr, data)


def test_deterministic_window_selection():
    """Select a middle sub-range of Deterministic windows via time_range."""
    store = TimeSeriesStore.create(in_memory=True)
    H, C = 6, 6  # 6 windows
    data = np.arange(H * C, dtype=np.float64).reshape(H, C)

    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Deterministic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, data, "det_window"),
    )

    # Select windows 2, 3, 4 (0-indexed): start = T0 + 2*interval, end = T0 + 5*interval
    w_start = T0 + 2 * INTERVAL_12H
    w_end = T0 + 5 * INTERVAL_12H
    got = store.get_time_series(key, time_range=(w_start, w_end))

    assert isinstance(got, Deterministic)
    assert got.count == 3
    assert got.initial_timestamp == w_start
    # Expected: columns 2, 3, 4 of data  -> shape [H, 3]
    expected = data[:, 2:5]
    np.testing.assert_array_equal(np.asarray(got.data), expected)


def test_deterministic_int64_dtype():
    """Deterministic with int64 data round-trips the dtype exactly."""
    store = TimeSeriesStore.create(in_memory=True)
    H, C = 4, 3
    horizon = timedelta(hours=H)
    data = np.arange(H * C, dtype=np.int64).reshape(H, C) * 100

    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Deterministic(T0, RES_1H, horizon, INTERVAL_12H, C, data, "det_int64"),
    )

    got = store.get_time_series(key)
    arr = np.asarray(got.data)
    assert arr.dtype == np.int64
    np.testing.assert_array_equal(arr, data)


# ---------------------------------------------------------------------------
# Probabilistic
# ---------------------------------------------------------------------------


def test_probabilistic_round_trip():
    """Write a Probabilistic forecast [P=3, H=6, count=4] and read it back."""
    store = TimeSeriesStore.create(in_memory=True)
    P, H, C = 3, 6, 4
    percentiles = [0.1, 0.5, 0.9]
    data = np.arange(P * H * C, dtype=np.float64).reshape(P, H, C)

    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Probabilistic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, percentiles, data, "prob_basic"),
    )

    assert key.time_series_type == TimeSeriesType.Probabilistic

    got = store.get_time_series(key)
    assert isinstance(got, Probabilistic)
    assert got.count == C
    assert got.horizon == "PT6H"
    assert got.interval == "PT12H"
    assert got.initial_timestamp == T0
    assert got.name == "prob_basic"
    assert got.percentiles == pytest.approx(percentiles)
    arr = np.asarray(got.data)
    assert arr.shape == (P, H, C)
    np.testing.assert_array_equal(arr, data)


def test_probabilistic_window_selection():
    """Select windows 1..3 (exclusive) from a Probabilistic forecast."""
    store = TimeSeriesStore.create(in_memory=True)
    P, H, C = 3, 6, 5
    percentiles = [0.25, 0.5, 0.75]
    data = np.arange(P * H * C, dtype=np.float64).reshape(P, H, C)

    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Probabilistic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, percentiles, data, "prob_window"),
    )

    # Windows 1 and 2 (0-indexed): start = T0 + 1*interval, end = T0 + 3*interval
    w_start = T0 + 1 * INTERVAL_12H
    w_end = T0 + 3 * INTERVAL_12H
    got = store.get_time_series(key, time_range=(w_start, w_end))

    assert isinstance(got, Probabilistic)
    assert got.count == 2
    assert got.initial_timestamp == w_start
    assert got.percentiles == pytest.approx(percentiles)
    # Expected: columns 1, 2 along axis 2 -> shape [P, H, 2]
    expected = data[:, :, 1:3]
    np.testing.assert_array_equal(np.asarray(got.data), expected)


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------


def test_scenarios_round_trip():
    """Write a Scenarios forecast [S=4, H=6, count=3] and read it back."""
    store = TimeSeriesStore.create(in_memory=True)
    S, H, C = 4, 6, 3
    data = np.arange(S * H * C, dtype=np.float64).reshape(S, H, C)

    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Scenarios(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, data, "scen_basic"),
    )

    assert key.time_series_type == TimeSeriesType.Scenarios

    got = store.get_time_series(key)
    assert isinstance(got, Scenarios)
    assert got.count == C
    assert got.scenario_count == S
    assert got.horizon == "PT6H"
    assert got.interval == "PT12H"
    assert got.initial_timestamp == T0
    assert got.name == "scen_basic"
    arr = np.asarray(got.data)
    assert arr.shape == (S, H, C)
    np.testing.assert_array_equal(arr, data)


def test_scenarios_window_selection():
    """Select windows 2..5 (exclusive) from a Scenarios forecast."""
    store = TimeSeriesStore.create(in_memory=True)
    S, H, C = 4, 6, 6
    data = np.arange(S * H * C, dtype=np.float64).reshape(S, H, C)

    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Scenarios(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, data, "scen_window"),
    )

    # Select windows 2, 3, 4 (0-indexed)
    w_start = T0 + 2 * INTERVAL_12H
    w_end = T0 + 5 * INTERVAL_12H
    got = store.get_time_series(key, time_range=(w_start, w_end))

    assert isinstance(got, Scenarios)
    assert got.count == 3
    assert got.scenario_count == S
    assert got.initial_timestamp == w_start
    # Expected: columns 2, 3, 4 along axis 2 -> shape [S, H, 3]
    expected = data[:, :, 2:5]
    np.testing.assert_array_equal(np.asarray(got.data), expected)


def test_scenarios_int64_dtype():
    """Scenarios with int64 data preserves dtype through round-trip."""
    store = TimeSeriesStore.create(in_memory=True)
    S, H, C = 2, 4, 3
    horizon = timedelta(hours=H)
    data = np.arange(S * H * C, dtype=np.int64).reshape(S, H, C) * 7

    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Scenarios(T0, RES_1H, horizon, INTERVAL_12H, C, data, "scen_int64"),
    )

    got = store.get_time_series(key)
    arr = np.asarray(got.data)
    assert arr.dtype == np.int64
    np.testing.assert_array_equal(arr, data)


# ---------------------------------------------------------------------------
# DeterministicSingleTimeSeries via transform_single_time_series
# ---------------------------------------------------------------------------


def test_transform_single_time_series_to_dst():
    """A stored SingleTimeSeries transforms into a DST, read back as Deterministic."""
    store = TimeSeriesStore.create(in_memory=True)
    # total_len=8, H=4 (horizon=4h, res=1h), interval=2h => interval_steps=2.
    # count = (8 - 4) / 2 + 1 = 3.
    horizon = timedelta(hours=4)
    interval = timedelta(hours=2)
    underlying = np.arange(8, dtype=np.float64)
    store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        SingleTimeSeries(T0, RES_1H, underlying, "dst_series"),
    )

    transformed = store.transform_single_time_series(horizon, interval)
    assert transformed == 1

    keys = store.get_time_series_keys(OWNER_ID, OWNER_CAT)
    dst_key = next(
        k for k in keys
        if k.time_series_type == TimeSeriesType.DeterministicSingleTimeSeries
    )
    got = store.get_time_series(dst_key)
    assert isinstance(got, Deterministic)
    assert got.count == 3
    assert got.name == "dst_series"
    arr = np.asarray(got.data)
    assert arr.shape == (4, 3)
    # Row-major [H, C]: out[s, w] = underlying[w*2 + s].
    expected = np.array([
        [0.0, 2.0, 4.0],
        [1.0, 3.0, 5.0],
        [2.0, 4.0, 6.0],
        [3.0, 5.0, 7.0],
    ])
    np.testing.assert_array_equal(arr, expected)


# ---------------------------------------------------------------------------
# Error cases
# ---------------------------------------------------------------------------


def test_misaligned_window_start_raises():
    """A time_range start not on a window boundary raises InvalidParameterError."""
    store = TimeSeriesStore.create(in_memory=True)
    H, C = 6, 4
    data = np.zeros((H, C), dtype=np.float64)
    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Deterministic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, data, "det_misalign"),
    )

    # 1 hour off — not a window boundary (interval=12h)
    bad_start = T0 + timedelta(hours=1)
    bad_end = T0 + 2 * INTERVAL_12H
    with pytest.raises(InvalidParameterError):
        store.get_time_series(key, time_range=(bad_start, bad_end))


def test_end_before_start_raises():
    """time_range with end < start raises InvalidParameterError."""
    store = TimeSeriesStore.create(in_memory=True)
    H, C = 6, 4
    data = np.zeros((H, C), dtype=np.float64)
    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Deterministic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, data, "det_backwards"),
    )

    with pytest.raises(InvalidParameterError):
        store.get_time_series(
            key,
            time_range=(T0 + 2 * INTERVAL_12H, T0),
        )


def test_start_past_last_window_raises():
    """A time_range whose aligned start is past the last window raises."""
    store = TimeSeriesStore.create(in_memory=True)
    H, C = 6, 4
    data = np.zeros((H, C), dtype=np.float64)
    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Deterministic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, data, "det_past"),
    )

    # Windows exist at indices 0..3; index C (one past the last) does not.
    past_start = T0 + C * INTERVAL_12H
    with pytest.raises(InvalidParameterError):
        store.get_time_series(key, time_range=(past_start, past_start + INTERVAL_12H))


def test_empty_window_range():
    """A time_range selecting zero windows returns count=0 array."""
    store = TimeSeriesStore.create(in_memory=True)
    H, C = 6, 4
    data = np.arange(H * C, dtype=np.float64).reshape(H, C)
    key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Deterministic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, data, "det_empty"),
    )

    # Aligned start but end == start selects no windows
    w_start = T0 + INTERVAL_12H
    w_end = T0 + INTERVAL_12H  # empty half-open [start, end)
    got = store.get_time_series(key, time_range=(w_start, w_end))

    assert isinstance(got, Deterministic)
    assert got.count == 0
    assert np.asarray(got.data).shape == (H, 0)


# ---------------------------------------------------------------------------
# get_forecast_parameters
# ---------------------------------------------------------------------------


def test_get_forecast_parameters():
    """A store with a forecast reports its horizon/interval/count/resolution."""
    store = TimeSeriesStore.create(in_memory=True)
    H, C = 6, 4
    data = np.arange(H * C, dtype=np.float64).reshape(H, C)
    store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Deterministic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, data, "det_params"),
    )

    params = store.get_forecast_parameters()
    # Periods are returned as canonical ISO-8601 duration strings.
    assert params["horizon"] == "PT6H"
    assert params["interval"] == "PT12H"
    assert params["count"] == C
    assert params["resolution"] == "PT1H"


def test_get_forecast_parameters_no_forecasts():
    """Without forecasts, every parameter is None."""
    store = TimeSeriesStore.create(in_memory=True)
    params = store.get_forecast_parameters()
    assert params == {
        "horizon": None,
        "interval": None,
        "count": None,
        "resolution": None,
    }


# ---------------------------------------------------------------------------
# repr smoke test
# ---------------------------------------------------------------------------


def test_repr_smoke():
    """__repr__ returns a non-empty string without raising."""
    store = TimeSeriesStore.create(in_memory=True)
    H, C = 6, 3

    det_data = np.zeros((H, C), dtype=np.float64)
    det_key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Deterministic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, det_data, "det_repr"),
    )
    det = store.get_time_series(det_key)
    assert "Deterministic" in repr(det)

    P = 2
    prob_data = np.zeros((P, H, C), dtype=np.float64)
    prob_key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Probabilistic(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, [0.1, 0.9], prob_data, "prob_repr"),
    )
    prob = store.get_time_series(prob_key)
    assert "Probabilistic" in repr(prob)

    S = 3
    scen_data = np.zeros((S, H, C), dtype=np.float64)
    scen_key = store.add_time_series(
        OWNER_ID, OWNER_TYPE, OWNER_CAT,
        Scenarios(T0, RES_1H, HORIZON_6H, INTERVAL_12H, C, scen_data, "scen_repr"),
    )
    scen = store.get_time_series(scen_key)
    assert "Scenarios" in repr(scen)
