"""Coverage parity for the Python binding.

Two gaps this file closes:

1. **Dtype / value breadth.** ``test_dtype_round_trip`` covered int64, int32,
   float32 and uint64 statics; ``bool`` was missing entirely, no forecast was
   ever stored at a non-float64 dtype, no static carried a multidimensional
   element shape (the Julia suite tests ``(4, 2, 3)``), and no non-finite value
   went through numpy.
2. **Untested ``Store`` methods.** Fourteen methods were declared in
   ``infrastore.pyi`` and exercised by no test. Each is called at least once here,
   with its result checked against an independently computed expectation rather
   than just "it didn't raise".

Assertions use the concrete exception classes; a bare ``pytest.raises(Exception)``
would also pass on a ``TypeError`` from a mis-specified call.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import (
    Deterministic,
    DuplicateTimeSeriesError,
    IntegrityError,
    InvalidParameterError,
    NonSequentialTimeSeries,
    NotFoundError,
    OwnerCategory,
    Probabilistic,
    Scenarios,
    SingleTimeSeries,
    Store,
    TimeSeriesType,
)

OWNER_TYPE = "Generator"
OWNER_CAT = OwnerCategory.Component
T0 = datetime(2024, 1, 1, tzinfo=timezone.utc)
RES_1H = timedelta(hours=1)


def _sts(name: str, data: np.ndarray, initial: datetime = T0) -> SingleTimeSeries:
    return SingleTimeSeries(initial, RES_1H, data, name)


def _add(store: Store, owner: int, ts, **kwargs):
    return store.add_time_series(owner, OWNER_TYPE, OWNER_CAT, ts, **kwargs)


# ---------------------------------------------------------------------------
# Dtype and value breadth
# ---------------------------------------------------------------------------


ALL_DTYPES = [np.float64, np.float32, np.int64, np.int32, np.uint64, np.bool_]


def test_every_dtype_round_trips_as_a_static_series():
    """All six supported dtypes, including bool, keep their dtype and values."""
    store = Store.create(in_memory=True)
    for i, dtype in enumerate(ALL_DTYPES):
        if dtype is np.bool_:
            values = np.array([True, False, True], dtype=dtype)
        else:
            values = np.array([1, 2, 3], dtype=dtype)
        key = _add(store, i + 1, _sts(f"ts_{np.dtype(dtype).name}", values))
        arr = np.asarray(store.read_by_id(key).data)
        assert arr.dtype == np.dtype(dtype), np.dtype(dtype).name
        np.testing.assert_array_equal(arr, values)


def test_every_dtype_round_trips_through_disk(tmp_path):
    """The same matrix, but persisted and reopened rather than in memory."""
    path = tmp_path / "dtypes.h5"
    store = Store.create(path=str(path), in_memory=False)
    keys = {}
    expected = {}
    for i, dtype in enumerate(ALL_DTYPES):
        name = np.dtype(dtype).name
        if dtype is np.bool_:
            values = np.array([True, False, True], dtype=dtype)
        else:
            values = np.array([1, 2, 3], dtype=dtype)
        keys[name] = _add(store, i + 1, _sts(f"ts_{name}", values))
        expected[name] = values
    store.flush()
    store.close()

    reopened = Store.open(str(path), read_only=True)
    for name, key in keys.items():
        arr = np.asarray(reopened.read_by_id(key).data)
        assert arr.dtype == expected[name].dtype, name
        np.testing.assert_array_equal(arr, expected[name], err_msg=name)
    assert reopened.verify_integrity() == {"ok": True, "errors": []}


def test_bool_static_series_keeps_true_and_false():
    """bool is one byte per element; a widened or truncated round trip shows up
    as changed values, not a changed dtype."""
    store = Store.create(in_memory=True)
    values = np.array([True, True, False, True, False, False], dtype=np.bool_)
    key = _add(store, 1, _sts("outage", values))
    arr = np.asarray(store.read_by_id(key).data)
    assert arr.dtype == np.bool_
    np.testing.assert_array_equal(arr, values)
    # A time slice of a bool series is still bool.
    sliced = store.read_by_ids_range([key], (T0, T0 + timedelta(hours=3)))[0]
    sliced_arr = np.asarray(sliced.data)
    assert sliced_arr.dtype == np.bool_
    np.testing.assert_array_equal(sliced_arr, values[:3])


def test_float32_static_round_trip_is_exact():
    """float32 was only reachable through one forecast test."""
    store = Store.create(in_memory=True)
    values = np.array([1.5, -2.25, 3.125], dtype=np.float32)
    key = _add(store, 1, _sts("f32", values))
    arr = np.asarray(store.read_by_id(key).data)
    assert arr.dtype == np.float32
    # Exact equality: these values are all representable in float32.
    np.testing.assert_array_equal(arr, values)


def test_multidimensional_element_shape_on_a_static_series():
    """A per-step tuple (element shape) round trips. Only ``element_shape == []``
    was asserted before."""
    store = Store.create(in_memory=True)
    # 4 timesteps, each a (2, 3) block.
    values = np.arange(4 * 2 * 3, dtype=np.float64).reshape(4, 2, 3)
    key = _add(store, 1, _sts("curve", values))

    got = store.read_by_id(key)
    assert got.length == 4
    arr = np.asarray(got.data)
    assert arr.shape == (4, 2, 3)
    np.testing.assert_array_equal(arr, values)

    meta = store.get_metadata_by_id(key)
    assert meta["element_shape"] == [2, 3]

    # A time slice keeps the element shape and slices only the time axis.
    sliced = np.asarray(
        store.read_by_ids_range([key], (T0 + RES_1H, T0 + 3 * RES_1H))[0].data
    )
    assert sliced.shape == (2, 2, 3)
    np.testing.assert_array_equal(sliced, values[1:3])


def test_multidimensional_element_shape_at_a_non_default_dtype():
    store = Store.create(in_memory=True)
    values = np.arange(3 * 4, dtype=np.int32).reshape(3, 4)
    key = _add(store, 1, _sts("int_curve", values))
    arr = np.asarray(store.read_by_id(key).data)
    assert arr.dtype == np.int32
    assert arr.shape == (3, 4)
    np.testing.assert_array_equal(arr, values)


def test_non_finite_values_round_trip_through_numpy():
    """NaN, ±Inf and -0.0 survive. Compared through ``.tobytes()`` because
    ``NaN != NaN`` and ``-0.0 == 0.0`` would both hide a corrupted round trip."""
    store = Store.create(in_memory=True)
    values = np.array(
        [np.nan, np.inf, -np.inf, -0.0, 0.0, np.finfo(np.float64).max],
        dtype=np.float64,
    )
    key = _add(store, 1, _sts("nonfinite", values))
    arr = np.asarray(store.read_by_id(key).data)
    assert arr.tobytes() == values.tobytes()
    assert np.isnan(arr[0])
    assert arr[1] == np.inf and arr[2] == -np.inf
    assert np.signbit(arr[3]) and arr[3] == 0.0

    # float32 too.
    values32 = np.array([np.nan, np.inf, -np.inf, -0.0], dtype=np.float32)
    key32 = _add(store, 2, _sts("nonfinite32", values32))
    arr32 = np.asarray(store.read_by_id(key32).data)
    assert arr32.dtype == np.float32
    assert arr32.tobytes() == values32.tobytes()


def test_nan_bit_patterns_deduplicate_to_one_array():
    """Two arrays differing only in NaN payload are one stored array: the core
    canonicalizes NaN before hashing."""
    store = Store.create(in_memory=True)
    quiet = np.array([1.0, np.nan, 3.0], dtype=np.float64)
    alt = quiet.copy()
    # Inject a different NaN bit pattern at index 1.
    alt.view(np.uint64)[1] = np.uint64(0x7FF8000000000001)
    assert alt.tobytes() != quiet.tobytes()
    assert np.isnan(alt[1])

    _add(store, 1, _sts("nan", quiet))
    _add(store, 2, _sts("nan", alt))
    assert store.num_distinct_arrays() == 1


def test_non_float64_forecast_dtypes():
    """One non-float64 dtype on each forecast type."""
    store = Store.create(in_memory=True)
    H, C = 2, 3
    horizon, interval = timedelta(hours=2), timedelta(hours=1)

    det_data = np.arange(H * C, dtype=np.int64).reshape(H, C)
    det_key = _add(
        store,
        1,
        Deterministic(T0, RES_1H, horizon, interval, C, det_data, "det_i64"),
    )
    det = store.read_by_id(det_key)
    det_arr = np.asarray(det.data)
    assert det_arr.dtype == np.int64
    np.testing.assert_array_equal(det_arr, det_data)

    prob_data = np.arange(2 * H * C, dtype=np.float32).reshape(2, H, C)
    prob_key = _add(
        store,
        2,
        Probabilistic(T0, RES_1H, horizon, interval, C, [0.1, 0.9], prob_data, "prob_f32"),
    )
    prob = store.read_by_id(prob_key)
    prob_arr = np.asarray(prob.data)
    assert prob_arr.dtype == np.float32
    np.testing.assert_array_equal(prob_arr, prob_data)
    assert prob.percentiles == [0.1, 0.9]

    scen_data = np.arange(3 * H * C, dtype=np.int32).reshape(3, H, C)
    scen_key = _add(
        store,
        3,
        # `scenario_count` is inferred from the leading dimension.
        Scenarios(T0, RES_1H, horizon, interval, C, scen_data, "scen_i32"),
    )
    scen = store.read_by_id(scen_key)
    scen_arr = np.asarray(scen.data)
    assert scen_arr.dtype == np.int32
    np.testing.assert_array_equal(scen_arr, scen_data)


def test_forecast_persists_and_reopens(tmp_path):
    """Only statics were ever persisted from Python; forecasts stayed in memory."""
    path = tmp_path / "forecasts.h5"
    H, C = 2, 3
    horizon, interval = timedelta(hours=2), timedelta(hours=1)
    det_data = np.arange(H * C, dtype=np.float64).reshape(H, C)
    prob_data = np.arange(2 * H * C, dtype=np.float64).reshape(2, H, C)
    scen_data = np.arange(3 * H * C, dtype=np.float64).reshape(3, H, C)

    store = Store.create(path=str(path), in_memory=False)
    det_key = _add(
        store, 1, Deterministic(T0, RES_1H, horizon, interval, C, det_data, "det")
    )
    prob_key = _add(
        store,
        2,
        Probabilistic(T0, RES_1H, horizon, interval, C, [0.5, 0.95], prob_data, "prob"),
    )
    scen_key = _add(
        store, 3, Scenarios(T0, RES_1H, horizon, interval, C, scen_data, "scen")
    )
    store.flush()
    store.close()

    reopened = Store.open(str(path), read_only=True)
    det = reopened.read_by_id(det_key)
    assert isinstance(det, Deterministic)
    assert det.count == C
    assert det.horizon == "PT2H"
    assert det.interval == "PT1H"
    np.testing.assert_array_equal(np.asarray(det.data), det_data)

    prob = reopened.read_by_id(prob_key)
    assert isinstance(prob, Probabilistic)
    assert prob.percentiles == [0.5, 0.95]
    np.testing.assert_array_equal(np.asarray(prob.data), prob_data)

    scen = reopened.read_by_id(scen_key)
    assert isinstance(scen, Scenarios)
    assert scen.scenario_count == 3
    np.testing.assert_array_equal(np.asarray(scen.data), scen_data)

    assert reopened.verify_integrity() == {"ok": True, "errors": []}

    # A window read off disk selects the right window.
    windowed = reopened.read_by_ids_range([det_key], (T0 + RES_1H, T0 + 2 * RES_1H))[0]
    assert windowed.count == 1
    np.testing.assert_array_equal(
        np.asarray(windowed.data), det_data[:, 1:2]
    )


def test_non_finite_forecast_round_trips_through_disk(tmp_path):
    path = tmp_path / "nonfinite_forecast.h5"
    data = np.array([[np.nan, np.inf], [-np.inf, -0.0]], dtype=np.float64)
    store = Store.create(path=str(path), in_memory=False)
    key = _add(
        store,
        1,
        Deterministic(
            T0, RES_1H, timedelta(hours=2), timedelta(hours=1), 2, data, "det_nan"
        ),
    )
    store.flush()
    store.close()

    reopened = Store.open(str(path), read_only=True)
    arr = np.asarray(reopened.read_by_id(key).data)
    assert arr.tobytes() == data.tobytes()


# ---------------------------------------------------------------------------
# Untested Store methods
# ---------------------------------------------------------------------------


def test_has_time_series():
    store = Store.create(in_memory=True)
    key = _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))
    assert store.association_exists(key) is True

    store.remove_by_ids([key])
    assert store.association_exists(key) is False


def test_has_any_time_series():
    store = Store.create(in_memory=True)
    _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))

    assert store.has_any_time_series(owner_id=1, owner_category=OWNER_CAT) is True
    assert store.has_any_time_series(owner_id=2, owner_category=OWNER_CAT) is False
    assert (
        store.has_any_time_series(
            owner_id=1, owner_category=OwnerCategory.SupplementalAttribute
        )
        is False
    )

    # A type restriction narrows the probe.
    assert (
        store.has_any_time_series(
            owner_id=1, time_series_type=TimeSeriesType.SingleTimeSeries
        )
        is True
    )
    assert (
        store.has_any_time_series(
            owner_id=1, time_series_type=TimeSeriesType.Deterministic
        )
        is False
    )

    # No filters: any association at all.
    assert store.has_any_time_series() is True

    # The features filter is a subset match.
    _add(store, 3, _sts("wind", np.arange(4, dtype=np.float64)), features={"s": "hi"})
    assert store.has_any_time_series(owner_id=3, features={"s": "hi"}) is True
    assert store.has_any_time_series(owner_id=3, features={"s": "lo"}) is False


def test_remove_time_series_bulk():
    store = Store.create(in_memory=True)
    k1 = _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))
    k2 = _add(store, 2, _sts("load", np.arange(4, dtype=np.float64) + 10))
    k3 = _add(store, 3, _sts("load", np.arange(4, dtype=np.float64) + 20))

    assert store.remove_by_ids([k1, k2]) == 2
    assert store.association_exists(k3) is True
    assert len(store.list_metadata()) == 1

    # An empty batch removes nothing and is not an error.
    assert store.remove_by_ids([]) == 0


def test_remove_time_series_bulk_is_all_or_nothing():
    store = Store.create(in_memory=True)
    k1 = _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))
    k2 = _add(store, 2, _sts("load", np.arange(4, dtype=np.float64) + 10))
    store.remove_by_ids([k2])  # k2 now matches nothing

    with pytest.raises(NotFoundError):
        store.remove_by_ids([k1, k2])
    # k1 survived: nothing in the failed batch was removed.
    assert store.association_exists(k1) is True


def test_counts_by_type():
    store = Store.create(in_memory=True)
    _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))
    _add(store, 2, _sts("load", np.arange(4, dtype=np.float64) + 1))
    _add(
        store,
        3,
        Deterministic(
            T0,
            RES_1H,
            timedelta(hours=2),
            timedelta(hours=1),
            3,
            np.arange(6, dtype=np.float64).reshape(2, 3),
            "det",
        ),
    )

    counts = store.counts_by_type()
    assert counts == {"SingleTimeSeries": 2, "Deterministic": 1}
    assert sum(counts.values()) == len(store.list_metadata())

    assert Store.create(in_memory=True).counts_by_type() == {}


def test_list_owner_ids():
    store = Store.create(in_memory=True)
    for owner in (5, 3, 9):
        _add(store, owner, _sts("load", np.arange(4, dtype=np.float64) + owner))
    # An attribute-owned series must not appear under the component category.
    store.add_time_series(
        77,
        "GeographicInfo",
        OwnerCategory.SupplementalAttribute,
        _sts("meta", np.arange(4, dtype=np.float64)),
    )

    assert sorted(store.list_owner_ids(OwnerCategory.Component)) == [3, 5, 9]
    assert store.list_owner_ids(OwnerCategory.SupplementalAttribute) == [77]

    # Scoped by type and resolution.
    assert sorted(
        store.list_owner_ids(
            OwnerCategory.Component, time_series_type=TimeSeriesType.SingleTimeSeries
        )
    ) == [3, 5, 9]
    assert (
        store.list_owner_ids(
            OwnerCategory.Component, time_series_type=TimeSeriesType.Deterministic
        )
        == []
    )
    assert sorted(store.list_owner_ids(OwnerCategory.Component, resolution=RES_1H)) == [
        3,
        5,
        9,
    ]
    assert (
        store.list_owner_ids(OwnerCategory.Component, resolution=timedelta(minutes=5))
        == []
    )


def test_static_summary():
    store = Store.create(in_memory=True)
    # Two owners share one name/shape/grid -> one grouped row with count 2.
    _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))
    _add(store, 2, _sts("load", np.arange(4, dtype=np.float64) + 1))
    # A different name is its own group.
    _add(store, 1, _sts("voltage", np.arange(4, dtype=np.float64) + 2))

    rows = store.static_summary()
    by_name = {(r["name"], r["owner_type"]): r for r in rows}
    assert by_name[("load", OWNER_TYPE)]["count"] == 2
    assert by_name[("voltage", OWNER_TYPE)]["count"] == 1
    for row in rows:
        assert row["resolution"] == "PT1H"
        assert row["time_step_count"] == 4
    # The counts add up to the association total.
    assert sum(r["count"] for r in rows) == len(store.list_metadata())

    assert Store.create(in_memory=True).static_summary() == []


def test_forecast_summary():
    store = Store.create(in_memory=True)
    H, C = 2, 3
    data = np.arange(H * C, dtype=np.float64).reshape(H, C)
    for owner in (1, 2):
        _add(
            store,
            owner,
            Deterministic(
                T0, RES_1H, timedelta(hours=2), timedelta(hours=1), C, data + owner, "det"
            ),
        )

    rows = store.forecast_summary()
    assert len(rows) == 1, rows
    row = rows[0]
    assert row["count"] == 2
    assert row["name"] == "det"
    assert row["resolution"] == "PT1H"
    assert row["horizon"] == "PT2H"
    assert row["interval"] == "PT1H"

    # A store with only statics has no forecast rows.
    statics = Store.create(in_memory=True)
    _add(statics, 1, _sts("load", np.arange(4, dtype=np.float64)))
    assert statics.forecast_summary() == []


def test_check_static_consistency():
    store = Store.create(in_memory=True)
    _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))
    _add(store, 2, _sts("load", np.arange(4, dtype=np.float64) + 1))

    rows = store.check_static_consistency()
    assert len(rows) == 1
    assert rows[0]["resolution"] == "PT1H"
    assert rows[0]["length"] == 4
    # Timestamps come back as RFC3339 strings in these summary rows, not
    # `datetime`s (unlike `SingleTimeSeries.initial_timestamp`).
    assert rows[0]["initial_timestamp"] == "2024-01-01T00:00:00+00:00"

    # Scoping to the one present resolution gives the same answer.
    assert store.check_static_consistency(resolution=RES_1H) == rows
    assert Store.create(in_memory=True).check_static_consistency() == []


def test_check_static_consistency_raises_on_a_divergent_grid():
    """The one path that raises ``IntegrityError``: two series at the same
    resolution disagreeing about the grid."""
    store = Store.create(in_memory=True)
    _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))
    # Same resolution, different length -> the grids diverge.
    _add(store, 2, _sts("other", np.arange(8, dtype=np.float64)))

    with pytest.raises(IntegrityError):
        store.check_static_consistency()


def test_check_static_consistency_raises_on_a_divergent_initial_timestamp():
    store = Store.create(in_memory=True)
    _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))
    _add(
        store,
        2,
        _sts("other", np.arange(4, dtype=np.float64), initial=T0 + timedelta(days=1)),
    )

    with pytest.raises(IntegrityError):
        store.check_static_consistency()


def test_check_static_consistency_separates_resolutions():
    """Two resolutions each internally consistent is not a divergence."""
    store = Store.create(in_memory=True)
    _add(store, 1, _sts("hourly", np.arange(4, dtype=np.float64)))
    store.add_time_series(
        2,
        OWNER_TYPE,
        OWNER_CAT,
        SingleTimeSeries(
            T0, timedelta(minutes=5), np.arange(8, dtype=np.float64), "five_minute"
        ),
    )

    rows = {r["resolution"]: r for r in store.check_static_consistency()}
    assert set(rows) == {"PT1H", "PT5M"}
    assert rows["PT1H"]["length"] == 4
    assert rows["PT5M"]["length"] == 8


def _resolve_one(store, owner_id, name, ts_type, **filters):
    """The single row matching these attributes.

    `resolve_metadata` used to be a `Store` method; it is now a listing plus an
    exactly-one check, which is what every binding does since the resolver came
    out. The filter reads its type through `TimeSeriesType::accepts`, so asking
    for `Deterministic` still spans a stored `DeterministicSingleTimeSeries` --
    that rule lives in the filter, not in a separate entry point.
    """
    rows = store.list_metadata(
        owner_id=owner_id,
        owner_category=OwnerCategory.Component,
        name=name,
        time_series_type=ts_type,
        **filters,
    )
    if not rows:
        raise NotFoundError(f"no series named {name}")
    if len(rows) != 1:
        raise InvalidParameterError(f"{len(rows)} rows match {name}; narrow the filter")
    return rows[0]


def test_a_filter_resolves_to_one_row_and_its_id():
    store = Store.create(in_memory=True)
    C = 3
    data = np.arange(2 * C, dtype=np.float64).reshape(2, C)
    key = _add(
        store,
        1,
        Deterministic(T0, RES_1H, timedelta(hours=2), timedelta(hours=1), C, data, "det"),
    )

    # Concrete type. The row comes back whole; the id off it is what the
    # id-addressed read and removal take.
    resolved = _resolve_one(
        store, 1, "det", TimeSeriesType.Deterministic, resolution=RES_1H
    )
    assert resolved["name"] == "det"
    assert resolved["id"] == key
    assert np.asarray(store.read_by_id(resolved["id"]).data).shape == (2, C)

    # A name that matches nothing is a miss, not a silent None.
    with pytest.raises(NotFoundError):
        _resolve_one(
            store, 1, "absent", TimeSeriesType.Deterministic, resolution=RES_1H
        )


def test_the_deterministic_family_filter_finds_a_transformed_dst():
    store = Store.create(in_memory=True)
    _add(store, 1, _sts("load", np.arange(8, dtype=np.float64)))
    assert store.transform_single_time_series(timedelta(hours=2), timedelta(hours=1)) == 1

    # Asking for `Deterministic` finds the derived forecast: the transform is a
    # storage detail. The row still reports the concrete stored type.
    resolved = _resolve_one(
        store, 1, "load", TimeSeriesType.Deterministic, resolution=RES_1H
    )
    assert resolved["time_series_type"] == "DeterministicSingleTimeSeries"


def _both_forecast_types() -> Store:
    """One explicitly stored ``Deterministic`` (owner 1) and one derived
    ``DeterministicSingleTimeSeries`` (owner 2), both at PT1H/PT1H."""
    store = Store.create(in_memory=True)
    data = np.arange(2 * 3, dtype=np.float64).reshape(2, 3)
    _add(
        store,
        1,
        Deterministic(T0, RES_1H, timedelta(hours=2), timedelta(hours=1), 3, data, "det"),
    )
    _add(store, 2, _sts("load", np.arange(8, dtype=np.float64)))
    assert store.transform_single_time_series(timedelta(hours=2), timedelta(hours=1)) == 1
    return store


def test_the_deterministic_filter_matches_both_stored_forms():
    """A `Deterministic` filter selects both storage forms in one catalog query,
    while `DeterministicSingleTimeSeries` narrows to the derived ones."""
    store = _both_forecast_types()

    rows = store.list_metadata(time_series_type=TimeSeriesType.Deterministic)
    # Both forms match, and each row still reports the type it is stored as, so
    # a caller auditing which forecasts are synthetic can still tell.
    assert {(r["owner_id"], r["time_series_type"]) for r in rows} == {
        (1, "Deterministic"),
        (2, "DeterministicSingleTimeSeries"),
    }
    assert [
        r["owner_id"]
        for r in store.list_metadata(
            time_series_type=TimeSeriesType.DeterministicSingleTimeSeries
        )
    ] == [2]

    # The same value threads through every other filter-taking surface.
    det = TimeSeriesType.Deterministic
    assert len(store.list_metadata(time_series_type=det)) == 2
    assert len(store.list_metadata(time_series_type=det)) == 2
    assert store.list_names(time_series_type=det) == ["det", "load"]
    assert store.list_owner_types(time_series_type=det) == [OWNER_TYPE]
    assert sorted(store.list_owner_ids(OWNER_CAT, time_series_type=det)) == [1, 2]
    assert store.get_resolutions(det) == ["PT1H"]
    assert store.get_intervals(det) == ["PT1H"]


def test_has_any_time_series_spans_both_deterministic_forms():
    """The probe a per-owner hot loop makes: one call covers both storage forms."""
    store = _both_forecast_types()

    for owner in (1, 2):
        assert store.has_any_time_series(
            owner_id=owner, time_series_type=TimeSeriesType.Deterministic
        )
    # Narrowing to the derived form excludes the densely stored one.
    assert not store.has_any_time_series(
        owner_id=1, time_series_type=TimeSeriesType.DeterministicSingleTimeSeries
    )
    assert not store.has_any_time_series(
        owner_id=3, time_series_type=TimeSeriesType.Deterministic
    )


def test_a_deterministic_reader_covers_a_derived_forecast():
    store = _both_forecast_types()
    reader = store.build_forecast_reader(TimeSeriesType.Deterministic, RES_1H, owner_id=2)
    assert len(reader.entries()) == 1

    store.forecast_read(reader, T0 + timedelta(hours=1))
    # The DST's window at T0+1h is two steps of the underlying series.
    np.testing.assert_array_equal(reader.entry_values(0), np.array([1.0, 2.0]))


def test_remove_by_filter_spans_both_deterministic_forms():
    store = _both_forecast_types()
    det = TimeSeriesType.Deterministic
    assert store.remove_by_filter(time_series_type=det) == 2
    assert not store.list_metadata(time_series_type=det)
    # Removing the derived view leaves the SingleTimeSeries it viewed in place.
    assert len(store.list_metadata(time_series_type=TimeSeriesType.SingleTimeSeries)) == 1


def test_every_time_series_type_name_is_accepted():
    """Every member of the enum is reachable by its name.

    The binding holds the accepted names in a list; this is what makes a new
    variant show up as a failing test rather than a name the filters quietly
    refuse.
    """
    store = _both_forecast_types()
    for name in (n for n in dir(TimeSeriesType) if not n.startswith("_")):
        member = getattr(TimeSeriesType, name)
        assert store.list_metadata(time_series_type=name) == (
            store.list_metadata(time_series_type=member)
        ), name


def test_an_unusable_requested_type_is_rejected():
    store = _both_forecast_types()
    # A `TimeSeriesType` or one of its own names, and nothing else. The name is
    # what the docstrings and the type stub have always shown, and it is what a
    # metadata row reports, so both spellings select the same rows.
    assert store.list_metadata(time_series_type="Deterministic") == (
        store.list_metadata(time_series_type=TimeSeriesType.Deterministic)
    )
    # A string that is not a type name says so, inside the exception hierarchy,
    # and names what would have worked. There is in particular no family
    # sentinel to reach for.
    with pytest.raises(InvalidParameterError, match="abstract_deterministic"):
        store.list_metadata(time_series_type="abstract_deterministic")
    with pytest.raises(InvalidParameterError, match="Deterministic"):
        store.list_metadata(time_series_type="deterministic")  # case-sensitive
    # Neither a type nor a name.
    with pytest.raises(TypeError):
        store.has_any_time_series(time_series_type=5)


def test_copy_time_series():
    store = Store.create(in_memory=True)
    values = np.arange(4, dtype=np.float64) + 7
    src = _add(store, 1, _sts("load", values))

    copy = store.copy_time_series(src, 2, OWNER_TYPE, new_name="load_copy")
    assert store.get_metadata_by_id(copy)['owner_id'] == 2
    assert store.get_metadata_by_id(copy)['name'] == "load_copy"

    # No array was duplicated, and both keys read the same values.
    assert store.num_distinct_arrays() == 1
    for key in (src, copy):
        np.testing.assert_array_equal(np.asarray(store.read_by_id(key).data), values)
    assert store.get_metadata_by_id(copy)["data_hash"] == store.get_metadata_by_id(src)["data_hash"]

    # Copying onto the same identity twice collides.
    with pytest.raises(DuplicateTimeSeriesError):
        store.copy_time_series(src, 2, OWNER_TYPE, new_name="load_copy")

    # Without a new name, the copy keeps the source's name.
    same_name = store.copy_time_series(src, 3, OWNER_TYPE)
    assert store.get_metadata_by_id(same_name)['name'] == "load"


def test_compact(tmp_path):
    """``compact`` rewrites the file so a delete actually reclaims space, and
    leaves the surviving series readable."""
    path = tmp_path / "compact.h5"
    store = Store.create(path=str(path), in_memory=False)
    keep_values = np.arange(4, dtype=np.float64)
    keep = _add(store, 1, _sts("keep", keep_values))
    # Big enough that dropping it moves the file size past HDF5 metadata noise.
    H, C = 48, 400
    bulk = np.arange(H * C, dtype=np.float64).reshape(H, C)
    drop = _add(
        store,
        2,
        Deterministic(
            T0, RES_1H, timedelta(hours=H), timedelta(hours=1), C, bulk, "drop"
        ),
    )
    store.flush()

    store.remove_by_ids([drop])
    store.flush()
    # HDF5 cannot hand the freed space back in place: the file only shrinks
    # once compact rewrites it.
    before = path.stat().st_size
    report = store.compact()
    after = path.stat().st_size

    assert set(report) == {
        "slots_reclaimed",
        "datasets_dropped",
        "feature_sets_reclaimed",
        "timestamp_sets_reclaimed",
        "bytes_reclaimed",
    }
    assert after < before
    assert report["bytes_reclaimed"] == before - after > 0

    np.testing.assert_array_equal(
        np.asarray(store.read_by_id(keep).data), keep_values
    )
    assert store.verify_integrity() == {"ok": True, "errors": []}

    # An in-memory store can be compacted too; it has no file to shrink.
    mem = Store.create(in_memory=True)
    _add(mem, 1, _sts("load", np.arange(4, dtype=np.float64)))
    assert mem.compact()["bytes_reclaimed"] == 0


def test_count_array_references_and_num_distinct_arrays():
    store = Store.create(in_memory=True)
    values = np.arange(8, dtype=np.float64)
    key = _add(store, 1, _sts("load", values))
    assert store.num_distinct_arrays() == 1

    data_hash = store.get_metadata_by_id(key)["data_hash"]
    assert store.count_array_references(data_hash) == {"sts": 1, "dst": 0}

    # A DST derived from the STS shares the array.
    assert store.transform_single_time_series(timedelta(hours=2), timedelta(hours=1)) == 1
    assert store.count_array_references(data_hash) == {"sts": 1, "dst": 1}
    assert store.num_distinct_arrays() == 1

    # A second owner with identical values shares the array; distinct values do not.
    _add(store, 2, _sts("load", values))
    assert store.count_array_references(data_hash)["sts"] == 2
    assert store.num_distinct_arrays() == 1
    _add(store, 3, _sts("load", values + 1))
    assert store.num_distinct_arrays() == 2

    # A hash that is not stored counts zero rather than raising.
    assert store.count_array_references("00" * 32) == {"sts": 0, "dst": 0}


def test_get_array_by_hash():
    store = Store.create(in_memory=True)
    values = np.arange(2 * 3, dtype=np.int32).reshape(2, 3)
    key = _add(store, 1, _sts("curve", values))

    data_hash = store.get_metadata_by_id(key)["data_hash"]
    arr = store.get_array_by_hash(data_hash)
    assert arr.dtype == np.int32
    assert arr.shape == (2, 3)
    np.testing.assert_array_equal(arr, values)

    # An unknown hash raises rather than returning an empty array.
    with pytest.raises(Exception) as exc:
        store.get_array_by_hash("00" * 32)
    assert exc.type is not AssertionError


def test_one_listing_groups_by_the_underlying_array():
    store = Store.create(in_memory=True)
    shared = np.arange(4, dtype=np.float64)
    k1 = _add(store, 1, _sts("load", shared))
    k2 = _add(store, 2, _sts("load", shared))
    k3 = _add(store, 3, _sts("load", shared + 100))

    # One listing carries the content hash beside the id, so grouping by array
    # is a `groupby` over its rows rather than a second projection of the same
    # query -- which is what `list_array_groups` was.
    rows = store.list_metadata()
    by_hash = {}
    for r in rows:
        by_hash.setdefault(r["data_hash"], []).append(r)
    assert len(by_hash) == 2, rows
    by_size = sorted(by_hash.values(), key=len)
    assert len(by_size[0]) == 1
    assert len(by_size[1]) == 2

    # The two-row group is the shared array, and its hash matches the metadata.
    shared_group = by_size[1]
    assert {r["owner_id"] for r in shared_group} == {1, 2}
    assert shared_group[0]["data_hash"] == store.get_metadata_by_id(k1)["data_hash"]
    assert shared_group[1]["data_hash"] == store.get_metadata_by_id(k2)["data_hash"]
    assert by_size[0][0]["owner_id"] == 3
    assert by_size[0][0]["data_hash"] == store.get_metadata_by_id(k3)["data_hash"]

    # Every row carries the id its write handed back, and agrees with the row a
    # by-id fetch returns.
    assert sorted(r["id"] for r in rows) == sorted([k1, k2, k3])
    for r in rows:
        assert store.get_metadata_by_id(r["id"]) == r

    # Filters apply.
    filtered = store.list_metadata(owner_id=3)
    assert len(filtered) == 1
    assert filtered[0]["data_hash"] == store.get_metadata_by_id(k3)["data_hash"]
    assert filtered[0]["id"] == k3
    assert store.list_metadata(name="absent") == []


def test_time_series_counts_detailed():
    store = Store.create(in_memory=True)
    _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))
    _add(store, 2, _sts("load", np.arange(4, dtype=np.float64) + 1))
    store.add_time_series(
        77,
        "GeographicInfo",
        OwnerCategory.SupplementalAttribute,
        _sts("meta", np.arange(4, dtype=np.float64) + 2),
    )

    detailed = store.time_series_counts_detailed()
    assert isinstance(detailed, dict)
    # Every reported count is a non-negative int, and the owner counts match the
    # independently-computed distinct-owner sets.
    for value in detailed.values():
        assert isinstance(value, int)
        assert value >= 0
    values = set(detailed.values())
    assert len(store.list_owner_ids(OwnerCategory.Component)) == 2
    assert len(store.list_owner_ids(OwnerCategory.SupplementalAttribute)) == 1
    assert 2 in values and 1 in values


def test_verify_integrity_reports_ok_for_a_healthy_store(tmp_path):
    path = tmp_path / "healthy.h5"
    store = Store.create(path=str(path), in_memory=False)
    _add(store, 1, _sts("load", np.arange(4, dtype=np.float64)))
    store.flush()
    assert store.verify_integrity() == {"ok": True, "errors": []}


# ---------------------------------------------------------------------------
# NonSequentialTimeSeries breadth
# ---------------------------------------------------------------------------


def test_non_sequential_at_every_dtype():
    store = Store.create(in_memory=True)
    timestamps = [T0, T0 + timedelta(minutes=7), T0 + timedelta(days=3)]
    for i, dtype in enumerate(ALL_DTYPES):
        if dtype is np.bool_:
            values = np.array([True, False, True], dtype=dtype)
        else:
            values = np.array([1, 2, 3], dtype=dtype)
        name = f"events_{np.dtype(dtype).name}"
        key = store.add_time_series(
            i + 1,
            OWNER_TYPE,
            OWNER_CAT,
            NonSequentialTimeSeries(timestamps, values, name),
        )
        got = store.read_by_id(key)
        assert got.timestamps == timestamps
        arr = np.asarray(got.data)
        assert arr.dtype == np.dtype(dtype), name
        np.testing.assert_array_equal(arr, values)


def test_non_sequential_rejects_bad_timestamps():
    with pytest.raises(InvalidParameterError):
        NonSequentialTimeSeries(
            [T0, T0], np.array([1.0, 2.0], dtype=np.float64), "duplicate"
        )
    with pytest.raises(InvalidParameterError):
        NonSequentialTimeSeries(
            [T0 + timedelta(hours=1), T0],
            np.array([1.0, 2.0], dtype=np.float64),
            "decreasing",
        )
    with pytest.raises(InvalidParameterError):
        NonSequentialTimeSeries(
            [T0], np.array([1.0, 2.0], dtype=np.float64), "count_mismatch"
        )


# ---------------------------------------------------------------------------
# Timestamp and period precision (Phase 4.1)
# ---------------------------------------------------------------------------


def test_millisecond_datetimes_round_trip():
    """A whole-millisecond ``datetime`` survives exactly.

    Python's ``datetime`` is microsecond-precision and the core stores an
    RFC3339 string, so a finer instant *could* be carried here — but the C ABI
    and Julia exchange instants as i64 unix milliseconds, where it could not.
    The store therefore records instants to the millisecond, as it does periods,
    and refuses anything finer on write rather than truncating it in one binding
    and not another. See ``test_sub_millisecond_datetimes_are_refused``.
    """
    store = Store.create(in_memory=True)
    precise = datetime(2024, 1, 1, 0, 0, 0, 123000, tzinfo=timezone.utc)
    key = _add(store, 1, _sts("load", np.arange(4, dtype=np.float64), initial=precise))

    got = store.read_by_id(key)
    assert got.initial_timestamp == precise
    assert got.initial_timestamp.microsecond == 123000
    # `get_metadata` returns the timestamp as an RFC3339 string, not a datetime
    # (FINDING F8), but the milliseconds are still there.
    assert store.get_metadata_by_id(key)["initial_timestamp"] == "2024-01-01T00:00:00.123+00:00"


def test_millisecond_datetimes_round_trip_through_disk(tmp_path):
    path = tmp_path / "milli.h5"
    precise = datetime(2024, 1, 1, 0, 0, 0, 999000, tzinfo=timezone.utc)
    store = Store.create(path=str(path), in_memory=False)
    key = _add(store, 1, _sts("load", np.arange(4, dtype=np.float64), initial=precise))
    store.flush()
    store.close()

    reopened = Store.open(str(path), read_only=True)
    assert reopened.read_by_id(key).initial_timestamp == precise


def test_sub_millisecond_datetimes_are_refused():
    """The instant half of the millisecond rule that periods already had.

    A `datetime` carrying microseconds that are not a whole millisecond names an
    instant the store cannot hand to every consumer unchanged, so the write is
    refused with an actionable error rather than silently moved.
    """
    store = Store.create(in_memory=True)
    for micro in (1, 999, 1500, 123456):
        precise = datetime(2024, 1, 1, 0, 0, 0, micro, tzinfo=timezone.utc)
        with pytest.raises(InvalidParameterError, match="finer than a millisecond"):
            _add(
                store,
                1,
                _sts("load", np.arange(4, dtype=np.float64), initial=precise),
            )


def test_a_microsecond_resolution_is_refused_rather_than_truncated():
    """Formerly FINDING F13, pinned as truncate-and-accept; now fixed.

    A `Period` is a whole number of milliseconds, so a finer `timedelta` loses
    its magnitude and encoded as ``PT0S``. The store used to *accept* such a
    series: a full read worked, a time-sliced read then failed on a zero-length
    step, and the resolution read back as ``PT0S`` rather than what was asked
    for. It is now rejected on write, as every forecast constructor already did.
    """
    store = Store.create(in_memory=True)
    with pytest.raises(InvalidParameterError, match="resolution"):
        _add(
            store,
            1,
            SingleTimeSeries(
                T0, timedelta(microseconds=1), np.arange(4, dtype=np.float64), "micro"
            ),
        )

    # One whole millisecond is the finest grid the store can express, and it is
    # unaffected.
    key = _add(
        store,
        1,
        SingleTimeSeries(
            T0, timedelta(milliseconds=1), np.arange(4, dtype=np.float64), "milli"
        ),
    )
    assert store.get_metadata_by_id(key)['resolution'] == "PT0.001S"
    assert store.read_by_id(key).length == 4


def test_sub_second_resolutions_are_exact_down_to_one_millisecond():
    store = Store.create(in_memory=True)
    for i, (label, resolution) in enumerate(
        [
            ("PT0.5S", timedelta(milliseconds=500)),
            ("PT0.001S", timedelta(milliseconds=1)),
            ("PT0.1S", timedelta(milliseconds=100)),
            ("PT1S", timedelta(seconds=1)),
        ]
    ):
        key = _add(
            store,
            i + 1,
            SingleTimeSeries(
                T0, resolution, np.arange(4, dtype=np.float64), f"res_{i}"
            ),
        )
        assert store.get_metadata_by_id(key)['resolution'] == label
        assert store.read_by_id(key).resolution == label

        # And the grid slices correctly at that resolution.
        sliced = store.read_by_ids_range(
            [key], (T0 + resolution, T0 + 3 * resolution)
        )[0]
        np.testing.assert_array_equal(
            np.asarray(sliced.data), np.array([1.0, 2.0])
        )
        assert sliced.initial_timestamp == T0 + resolution


def test_a_naive_datetime_is_recorded_as_zoneless():
    """A timestamp with no tzinfo names a wall clock, not an instant -- so it is
    recorded as one and handed back as one.

    Accepting it is only defensible because the read path returns the same
    spelling: a naive datetime does not equal an aware one and cannot even be
    compared with one, so a store that took one and returned the other would be
    worse than one that refused. It used to refuse.
    """
    series = SingleTimeSeries(
        datetime(2024, 1, 1), timedelta(hours=1), np.arange(4, dtype=np.float64), "naive"
    )
    assert series.time_reference == "zoneless"
    assert series.initial_timestamp == datetime(2024, 1, 1)
    assert series.initial_timestamp.tzinfo is None

    # And the conversion never goes through `astimezone`, which would apply the
    # machine's local zone -- the same script would then write a different
    # instant in Denver than in CI. The wall clock is taken as it stands.
    stored = SingleTimeSeries(
        datetime(2024, 1, 1), timedelta(hours=1), np.arange(4, dtype=np.float64), "naive"
    )
    assert stored.initial_timestamp == datetime(2024, 1, 1)


def test_pre_1970_and_far_future_timestamps_round_trip(tmp_path):
    path = tmp_path / "spans.h5"
    store = Store.create(path=str(path), in_memory=False)
    cases = {
        "pre_epoch": datetime(1900, 1, 1, tzinfo=timezone.utc),
        "just_before": datetime(1969, 12, 31, 23, 59, 59, tzinfo=timezone.utc),
        "epoch": datetime(1970, 1, 1, tzinfo=timezone.utc),
        "far_future": datetime(2200, 6, 15, 12, 30, 45, tzinfo=timezone.utc),
    }
    keys = {
        name: _add(store, i + 1, _sts(name, np.arange(4, dtype=np.float64), initial=ts))
        for i, (name, ts) in enumerate(cases.items())
    }
    store.flush()
    store.close()

    reopened = Store.open(str(path), read_only=True)
    for name, key in keys.items():
        assert reopened.read_by_id(key).initial_timestamp == cases[name], name


def test_non_sequential_timestamps_keep_millisecond_spacing():
    store = Store.create(in_memory=True)
    timestamps = [
        T0,
        T0 + timedelta(milliseconds=1),
        T0 + timedelta(milliseconds=2),
        T0 + timedelta(milliseconds=500),
    ]
    key = store.add_time_series(
        1,
        OWNER_TYPE,
        OWNER_CAT,
        NonSequentialTimeSeries(
            timestamps, np.arange(4, dtype=np.float64), "precise"
        ),
    )
    got = store.read_by_id(key)
    assert got.timestamps == timestamps, (
        "millisecond spacing must survive; a second-quantized encoding would "
        "collapse the first three"
    )


def test_sub_millisecond_non_sequential_timestamps_are_refused():
    """Every entry is checked, not just the first.

    Two timestamps less than a millisecond apart are distinct here and identical
    to any consumer reading them through the C ABI's millisecond boundary, where
    the vector then stops being strictly increasing. The write is refused, and
    the error names the offending index.
    """
    store = Store.create(in_memory=True)
    timestamps = [
        T0,
        T0 + timedelta(hours=1, microseconds=1),
        T0 + timedelta(hours=2),
    ]
    with pytest.raises(InvalidParameterError, match="timestamp 1"):
        store.add_time_series(
            1,
            OWNER_TYPE,
            OWNER_CAT,
            NonSequentialTimeSeries(
                timestamps, np.arange(3, dtype=np.float64), "precise"
            ),
        )


def test_a_century_spanning_non_sequential_series_round_trips(tmp_path):
    path = tmp_path / "century.h5"
    timestamps = [
        datetime(1900, 1, 1, tzinfo=timezone.utc),
        datetime(1970, 1, 1, tzinfo=timezone.utc),
        datetime(2024, 2, 29, 12, 0, tzinfo=timezone.utc),  # leap day
        datetime(2100, 12, 31, 23, 59, 59, tzinfo=timezone.utc),
    ]
    values = np.arange(4, dtype=np.float64) * 10

    store = Store.create(path=str(path), in_memory=False)
    key = store.add_time_series(
        1,
        OWNER_TYPE,
        OWNER_CAT,
        NonSequentialTimeSeries(timestamps, values, "century"),
    )
    store.flush()
    store.close()

    reopened = Store.open(str(path), read_only=True)
    got = reopened.read_by_id(key)
    assert got.timestamps == timestamps
    np.testing.assert_array_equal(np.asarray(got.data), values)


# ---------------------------------------------------------------------------
# Reader / mutation interaction (Phase 4.2)
# ---------------------------------------------------------------------------


def test_a_reader_built_before_a_removal_errors_on_the_next_read():
    """A ``StaticReader`` is an owned object here, so nothing stops the store
    being mutated behind it. PIN that the stale read *fails* rather than reading
    whatever now occupies the reclaimed slot — a silent success would risk
    handing back another series' data."""
    store = Store.create(in_memory=True)
    k1 = _add(store, 1, _sts("a", np.arange(4, dtype=np.float64)))
    _add(store, 2, _sts("b", np.arange(4, dtype=np.float64) + 100))
    assert store.num_distinct_arrays() == 2

    reader = store.build_static_reader(resolution=RES_1H)
    store.static_read(reader, T0)
    before = np.sort(reader.group_values(0))
    np.testing.assert_array_equal(before, np.array([0.0, 100.0]))

    store.remove_by_ids([k1])
    assert store.num_distinct_arrays() == 1

    with pytest.raises(NotFoundError):
        store.static_read(reader, T0)

    # A rebuilt reader works and sees only the survivor.
    rebuilt = store.build_static_reader(resolution=RES_1H)
    store.static_read(rebuilt, T0)
    np.testing.assert_array_equal(rebuilt.group_values(0), np.array([100.0]))


def test_a_reader_built_before_a_removal_of_a_shared_array_reads_stale_values():
    """When the removed series shared its array with a survivor, the array stays
    alive and the stale reader keeps returning its build-time snapshot — including
    the column whose association is gone."""
    store = Store.create(in_memory=True)
    values = np.arange(4, dtype=np.float64)
    k1 = _add(store, 1, _sts("a", values))
    _add(store, 2, _sts("b", values))
    assert store.num_distinct_arrays() == 1

    reader = store.build_static_reader(resolution=RES_1H)
    store.static_read(reader, T0)
    before = reader.group_values(0)
    assert before.shape == (2,)

    store.remove_by_ids([k1])
    assert store.num_distinct_arrays() == 1

    store.static_read(reader, T0)
    np.testing.assert_array_equal(
        reader.group_values(0),
        before,
        err_msg="a stale reader must return its snapshot, not garbage",
    )

    # A rebuilt reader has one column.
    rebuilt = store.build_static_reader(resolution=RES_1H)
    store.static_read(rebuilt, T0)
    assert rebuilt.group_values(0).shape == (1,)


def test_a_reader_built_before_an_add_does_not_see_the_new_series():
    """The column set is fixed at build time, so a caller stepping a timeline gets
    a stable shape for the whole sweep."""
    store = Store.create(in_memory=True)
    _add(store, 1, _sts("a", np.arange(4, dtype=np.float64)))

    reader = store.build_static_reader(resolution=RES_1H)
    store.static_read(reader, T0)
    assert reader.group_values(0).shape == (1,)

    _add(store, 2, _sts("b", np.arange(4, dtype=np.float64) + 100))

    store.static_read(reader, T0)
    assert reader.group_values(0).shape == (1,), "the reader's shape is a snapshot"

    rebuilt = store.build_static_reader(resolution=RES_1H)
    store.static_read(rebuilt, T0)
    assert rebuilt.group_values(0).shape == (2,)
