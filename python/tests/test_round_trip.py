"""End-to-end round-trip tests for the time_series Python bindings."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from time_series_store import (
    DuplicateTimeSeriesError,
    InvalidParameterError,
    NotFoundError,
    OwnerCategory,
    ReadOnlyStoreError,
    SingleTimeSeries,
    NonSequentialTimeSeries,
    TimeSeriesStore,
    TimeSeriesType,
)


def make_series(
    initial_year: int = 2024,
    length: int = 24,
    base: float = 100.0,
    name: str = "load",
) -> SingleTimeSeries:
    initial = datetime(initial_year, 1, 1, tzinfo=timezone.utc)
    resolution = timedelta(hours=1)
    data = np.arange(length, dtype=np.float64) + base
    return SingleTimeSeries(initial, resolution, data, name)


def test_in_memory_round_trip():
    store = TimeSeriesStore.create(in_memory=True)
    s = make_series()
    key = store.add_time_series(
        owner_uuid="42",
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=s,
        units="MW",
    )
    assert key.owner_uuid == "42"
    assert key.time_series_type == TimeSeriesType.SingleTimeSeries

    got = store.get_time_series(key)
    assert got.length == 24
    assert got.initial_timestamp == s.initial_timestamp
    assert got.name == "load"
    np.testing.assert_array_equal(np.asarray(got.data), np.asarray(s.data))


def test_persistent_round_trip(tmp_path):
    path = tmp_path / "store.nc"
    s = make_series(2024, 12, 1.0)

    store = TimeSeriesStore.create(path=str(path), in_memory=False)
    key = store.add_time_series(
        owner_uuid="1",
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=s,
    )
    store.flush()
    del store  # drop file handle

    reopened = TimeSeriesStore.open(path=str(path), read_only=True)
    keys = reopened.get_time_series_keys("1")
    assert len(keys) == 1
    got = reopened.get_time_series(keys[0])
    assert got.name == "load"
    np.testing.assert_array_equal(np.asarray(got.data), np.asarray(s.data))

    report = reopened.verify_integrity()
    assert report == [], f"integrity errors: {report}"


@pytest.mark.parametrize(
    "kwargs",
    [
        {"compression": "none"},
        {"compression": "deflate", "compression_level": 9, "shuffle": False},
        {"compression": "deflate", "compression_level": 1, "shuffle": True},
    ],
)
def test_compression_round_trip(tmp_path, kwargs):
    """Each compression policy stores and reads back identical data."""
    path = tmp_path / "store.nc"
    s = make_series(2024, 12, 1.0)

    store = TimeSeriesStore.create(path=str(path), in_memory=False, **kwargs)
    store.add_time_series("1", "Generator", OwnerCategory.Component, s)
    store.flush()
    del store

    reopened = TimeSeriesStore.open(path=str(path), read_only=True)
    # The persisted policy is restored on open.
    comp = reopened.get_compression()
    expected = kwargs.get("compression", "deflate")
    assert comp["compression"] == expected
    if expected == "deflate":
        assert comp["level"] == kwargs.get("compression_level", 3)
        assert comp["shuffle"] == kwargs.get("shuffle", True)
    keys = reopened.get_time_series_keys("1")
    got = reopened.get_time_series(keys[0])
    np.testing.assert_array_equal(np.asarray(got.data), np.asarray(s.data))
    assert reopened.verify_integrity() == []


def test_get_compression_in_memory_is_none():
    store = TimeSeriesStore.create(in_memory=True)
    assert store.get_compression()["compression"] == "none"


def test_invalid_compression_rejected(tmp_path):
    path = tmp_path / "store.nc"
    with pytest.raises(InvalidParameterError):
        TimeSeriesStore.create(path=str(path), in_memory=False, compression="lz4")
    with pytest.raises(InvalidParameterError):
        TimeSeriesStore.create(
            path=str(path), in_memory=False, compression="deflate", compression_level=99
        )


def test_features_disambiguate_keys():
    store = TimeSeriesStore.create(in_memory=True)
    s1 = make_series(base=1.0)
    s2 = make_series(base=100.0)

    store.add_time_series(
        owner_uuid="1",
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=s1,
        features={"model_year": 2030, "is_baseline": True},
    )
    store.add_time_series(
        owner_uuid="1",
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=s2,
        features={"model_year": 2035},
    )

    all_rows = store.list_time_series(owner_uuid="1")
    assert len(all_rows) == 2

    only_2035 = store.list_time_series(features={"model_year": 2035})
    assert len(only_2035) == 1
    assert only_2035[0]["features"]["model_year"] == 2035


def test_duplicate_key_raises():
    store = TimeSeriesStore.create(in_memory=True)
    s = make_series()
    store.add_time_series("1", "Generator", OwnerCategory.Component, s)
    with pytest.raises(DuplicateTimeSeriesError):
        store.add_time_series("1", "Generator", OwnerCategory.Component, s)


def test_missing_key_raises_not_found():
    store = TimeSeriesStore.create(in_memory=True)
    s = make_series()
    key = store.add_time_series("1", "Generator", OwnerCategory.Component, s)
    store.remove_time_series(key)
    with pytest.raises(NotFoundError):
        store.get_time_series(key)


def test_time_range_slicing():
    store = TimeSeriesStore.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    resolution = timedelta(hours=1)
    data = np.array([10.0, 20.0, 30.0, 40.0, 50.0, 60.0])
    s = SingleTimeSeries(initial, resolution, data, "load")
    key = store.add_time_series("1", "Generator", OwnerCategory.Component, s)

    start = initial + timedelta(hours=2)
    end = initial + timedelta(hours=5)
    got = store.get_time_series(key, time_range=(start, end))
    assert got.length == 3
    assert got.initial_timestamp == start
    np.testing.assert_array_equal(np.asarray(got.data), np.array([30.0, 40.0, 50.0]))


def test_read_only_blocks_writes(tmp_path):
    path = tmp_path / "store.nc"
    store = TimeSeriesStore.create(path=str(path), in_memory=False)
    store.add_time_series("1", "Generator", OwnerCategory.Component, make_series())
    store.flush()
    del store

    ro = TimeSeriesStore.open(path=str(path), read_only=True)
    assert ro.read_only is True
    with pytest.raises(ReadOnlyStoreError):
        ro.add_time_series("2", "Generator", OwnerCategory.Component, make_series())


def test_invalid_feature_value_raises():
    store = TimeSeriesStore.create(in_memory=True)
    with pytest.raises(InvalidParameterError):
        store.add_time_series(
            "1", "Generator", OwnerCategory.Component, make_series(),
            features={"bad": [1, 2, 3]},  # lists aren't valid feature values (int/float/bool/str)
        )


def test_counts_and_resolutions():
    store = TimeSeriesStore.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    data = np.array([1.0, 2.0, 3.0])

    for owner, res in [("1", timedelta(hours=1)), ("2", timedelta(minutes=15)), ("3", timedelta(hours=4))]:
        s = SingleTimeSeries(initial, res, data, "load")
        store.add_time_series(owner, "Generator", OwnerCategory.Component, s)

    counts = store.get_time_series_counts()
    assert counts["static_time_series"] == 3
    assert counts["components_with_time_series"] == 3

    resolutions = store.get_resolutions()
    assert resolutions == [timedelta(minutes=15), timedelta(hours=1), timedelta(hours=4)]


def test_numpy_array_received_as_ndarray():
    """Sanity check: data round-tripped is a numpy ndarray, with the original dtype."""
    store = TimeSeriesStore.create(in_memory=True)
    s = make_series()
    key = store.add_time_series("1", "Generator", OwnerCategory.Component, s)
    got = store.get_time_series(key)
    arr = np.asarray(got.data)
    assert isinstance(arr, np.ndarray)
    assert arr.dtype == np.float64
    assert arr.shape == (24,)


def test_non_sequential_round_trip_and_slice():
    store = TimeSeriesStore.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    timestamps = [
        initial,
        initial + timedelta(hours=4),
        initial + timedelta(days=2),
    ]
    series = NonSequentialTimeSeries(timestamps, np.array([10.0, 20.0, 30.0]), "events")
    key = store.add_time_series(
        "irregular", "Generator", OwnerCategory.Component, series,
    )

    assert key.time_series_type == TimeSeriesType.NonSequentialTimeSeries
    assert key.resolution is None
    got = store.get_time_series(
        key,
        time_range=(initial + timedelta(hours=1), initial + timedelta(days=3)),
    )
    assert isinstance(got, NonSequentialTimeSeries)
    assert got.name == "events"
    assert got.timestamps == timestamps[1:]
    np.testing.assert_array_equal(np.asarray(got.data), np.array([20.0, 30.0]))


def test_non_sequential_rejects_invalid_timestamps():
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    with pytest.raises(InvalidParameterError):
        NonSequentialTimeSeries(
            [initial, initial],
            np.array([1.0, 2.0]),
            "events",
        )


def test_dtype_round_trip():
    """Non-float64 numpy dtypes round-trip with their dtype preserved."""
    store = TimeSeriesStore.create(in_memory=True)
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    res = timedelta(hours=1)

    for dtype in (np.int64, np.int32, np.float32, np.uint64):
        s = SingleTimeSeries(initial, res, np.array([1, 2, 3], dtype=dtype), f"ts_{dtype.__name__}")
        key = store.add_time_series("o", "Generator", OwnerCategory.Component, s)
        arr = np.asarray(store.get_time_series(key).data)
        assert arr.dtype == dtype
        assert arr.tolist() == [1, 2, 3]


def test_add_time_series_bulk(tmp_path):
    """Bulk add commits all series in one transaction and returns keys in order."""
    path = tmp_path / "bulk.nc"
    store = TimeSeriesStore.create(path=str(path), in_memory=False)
    items = [
        {
            "owner_uuid": str(i),
            "owner_type": "Generator",
            "owner_category": OwnerCategory.Component,
            "time_series": make_series(base=float(i)),
            "features": {"scenario": i},
            "units": "MW",
        }
        for i in range(10)
    ]
    keys = store.add_time_series_bulk(items)
    assert len(keys) == 10
    for i, key in enumerate(keys):
        assert key.owner_uuid == str(i)
        got = store.get_time_series(key)
        np.testing.assert_array_equal(
            np.asarray(got.data), np.arange(24, dtype=np.float64) + float(i)
        )


def test_add_time_series_bulk_rolls_back_on_error():
    """A duplicate in the batch rolls back every item."""
    store = TimeSeriesStore.create(in_memory=True)
    dup = {
        "owner_uuid": "1",
        "owner_type": "Generator",
        "owner_category": OwnerCategory.Component,
        "time_series": make_series(),
    }
    with pytest.raises(DuplicateTimeSeriesError):
        store.add_time_series_bulk([dup, dict(dup)])
    assert store.get_time_series_keys("1") == []


def test_add_time_series_bulk_rejects_missing_keys():
    store = TimeSeriesStore.create(in_memory=True)
    with pytest.raises(InvalidParameterError, match="owner_uuid"):
        store.add_time_series_bulk([{"owner_type": "Generator"}])
