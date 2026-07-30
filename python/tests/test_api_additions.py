"""Tests for the Phase-3 Python surface: readers, discovery/removal/rename,
richer metadata rows, transform params, ext, key set semantics."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import (
    Deterministic,
    InvalidParameterError,
    OwnerCategory,
    SingleTimeSeries,
    Store,
    TimeSeriesError,
    TimeSeriesType,
)


def _t0() -> datetime:
    return datetime(2030, 1, 1, tzinfo=timezone.utc)


def _sts(name: str, base: float, length: int = 8) -> SingleTimeSeries:
    data = np.arange(length, dtype=np.float64) + base
    return SingleTimeSeries(_t0(), timedelta(hours=1), data, name)


def _det(name: str) -> Deterministic:
    # H=2, count=3, interval 1h. Row-major [H, count].
    data = np.arange(6, dtype=np.float64).reshape(2, 3)
    return Deterministic(_t0(), timedelta(hours=1), timedelta(hours=2), timedelta(hours=1), 3, data, name)


def test_add_ext_and_get_metadata():
    store = Store.create(in_memory=True)
    key = store.add_time_series(
        owner_id=1, owner_type="Generator", owner_category=OwnerCategory.Component,
        time_series=_sts("load", 10.0), units="MW", ext="Profile",
    )
    meta = store.get_metadata(key)
    assert meta["units"] == "MW"
    assert meta["ext"] == "Profile"
    assert meta["element_type"] == "f64"
    assert meta["element_shape"] == []
    assert meta["initial_timestamp"] is not None


def test_bulk_read_time_range():
    store = Store.create(in_memory=True)
    k1 = store.add_time_series(owner_id=1, owner_type="Generator",
                              owner_category=OwnerCategory.Component, time_series=_sts("load", 100.0))
    k2 = store.add_time_series(owner_id=2, owner_type="Generator",
                              owner_category=OwnerCategory.Component, time_series=_sts("load", 200.0))
    rng = (_t0() + timedelta(hours=2), _t0() + timedelta(hours=5))
    sliced = store.bulk_read([k1, k2], time_range=rng)
    for i, k in enumerate([k1, k2]):
        expected = store.get_time_series(k, time_range=rng)
        np.testing.assert_array_equal(sliced[i].data, expected.data)


def test_discovery_and_removal_and_rename():
    store = Store.create(in_memory=True)
    store.add_time_series(owner_id=1, owner_type="Generator",
                          owner_category=OwnerCategory.Component, time_series=_sts("load", 1.0))
    store.add_time_series(owner_id=2, owner_type="Bus",
                          owner_category=OwnerCategory.Component, time_series=_sts("voltage", 2.0))
    kf = store.add_time_series(owner_id=3, owner_type="Generator",
                               owner_category=OwnerCategory.Component, time_series=_det("fc"))

    assert store.get_intervals() == ["PT1H"]
    assert store.get_intervals(time_series_type=TimeSeriesType.SingleTimeSeries) == []
    assert sorted(store.list_names()) == ["fc", "load", "voltage"]
    assert store.list_names(owner_type="Generator") == ["fc", "load"]
    assert sorted(store.list_owner_types()) == ["Bus", "Generator"]

    # rename
    nk = store.rename_time_series(kf, "fc2")
    assert nk.name == "fc2"
    assert store.get_metadata(nk)["name"] == "fc2"

    # remove_by_filter
    removed = store.remove_by_filter(owner_id=2)
    assert removed == 1
    assert store.list_names(owner_id=2) == []


def test_transform_with_params_and_forecast_parameters():
    store = Store.create(in_memory=True)
    store.add_time_series(owner_id=1, owner_type="Generator",
                          owner_category=OwnerCategory.Component, time_series=_sts("load", 5.0, length=5))
    n = store.transform_single_time_series(
        timedelta(hours=2), timedelta(hours=1),
        owner_category=OwnerCategory.Component, resolution=timedelta(hours=1),
    )
    assert n == 1
    params = store.get_forecast_parameters(resolution=timedelta(hours=1))
    assert params["initial_timestamp"] is not None


def test_keys_usable_in_sets():
    store = Store.create(in_memory=True)
    k1 = store.add_time_series(owner_id=1, owner_type="Generator",
                               owner_category=OwnerCategory.Component, time_series=_sts("load", 1.0))
    k2 = store.add_time_series(owner_id=2, owner_type="Generator",
                               owner_category=OwnerCategory.Component, time_series=_sts("load", 2.0))
    # Same key looked up again is equal + hashes equal.
    (k1_again,) = [k for k in store.list_keys(owner_id=1)]
    assert k1_again == k1
    s = {k1, k2, k1_again}
    assert len(s) == 2


def test_static_reader():
    store = Store.create(in_memory=True)
    store.add_time_series(owner_id=1, owner_type="Generator",
                          owner_category=OwnerCategory.Component, time_series=_sts("load", 10.0, length=4))
    store.add_time_series(owner_id=2, owner_type="Generator",
                          owner_category=OwnerCategory.Component, time_series=_sts("load", 20.0, length=4))
    reader = store.build_static_reader(timedelta(hours=1))
    grid = reader.grid()
    assert grid["length"] == 4
    assert grid["resolution"] == "PT1H"
    groups = reader.groups()
    assert len(groups) == 1
    assert groups[0]["dtype"] == "f64"
    assert groups[0]["element_type"] == "f64"

    stamps = reader.timestamps()
    assert len(stamps) == 4

    store.static_read(reader, _t0() + timedelta(hours=2))
    vals = reader.group_values(0)
    # Two columns (owners 1, 2), scalar element shape -> shape (2,).
    assert vals.shape == (2,)
    np.testing.assert_array_equal(np.sort(vals), np.array([12.0, 22.0]))

    # Off-grid raises the concrete type, not just "something".
    with pytest.raises(InvalidParameterError):
        store.static_read(reader, _t0() + timedelta(minutes=30))


def test_forecast_reader():
    store = Store.create(in_memory=True)
    store.add_time_series(owner_id=1, owner_type="Generator",
                          owner_category=OwnerCategory.Component, time_series=_det("fc"))
    reader = store.build_forecast_reader(TimeSeriesType.Deterministic, timedelta(hours=1))
    tl = reader.timeline()
    assert tl["count"] == 3
    assert tl["interval"] == "PT1H"
    entries = reader.entries()
    assert len(entries) == 1

    # A single entry occupies its own slot.
    assert reader.num_slots() == 1
    assert reader.entry_slot(0) == 0
    with pytest.raises(InvalidParameterError):
        reader.entry_slot(1)

    store.forecast_read(reader, _t0() + timedelta(hours=1))
    window = reader.entry_values(0)
    # window shape [H] = (2,); window k=1 of [[0,1,2],[3,4,5]] is [1, 4].
    assert window.shape == (2,)
    np.testing.assert_array_equal(window, np.array([1.0, 4.0]))


def test_forecast_reader_shared_slot_dedup():
    # Two owners carrying the identical forecast dedup to one backing array, so
    # the reader reports one slot shared by both entries.
    store = Store.create(in_memory=True)
    for oid in (1, 2):
        store.add_time_series(owner_id=oid, owner_type="Generator",
                              owner_category=OwnerCategory.Component, time_series=_det("fc"))
    reader = store.build_forecast_reader(TimeSeriesType.Deterministic, timedelta(hours=1))
    assert len(reader.entries()) == 2
    assert reader.num_slots() == 1
    assert reader.entry_slot(0) == reader.entry_slot(1) == 0

    # Both entries resolve to the same window, materialized once per slot.
    store.forecast_read(reader, _t0() + timedelta(hours=1))
    np.testing.assert_array_equal(reader.entry_values(0), reader.entry_values(1))


def test_list_time_series_new_fields_and_interval_filter():
    store = Store.create(in_memory=True)
    store.add_time_series(owner_id=1, owner_type="Generator",
                          owner_category=OwnerCategory.Component, time_series=_det("fc"))
    rows = store.list_time_series(interval=timedelta(hours=1))
    assert len(rows) == 1
    row = rows[0]
    for field in ("initial_timestamp", "horizon", "interval", "count",
                  "percentiles", "element_type", "element_shape", "ext"):
        assert field in row
    assert row["interval"] == "PT1H"
    # No forecast at a different interval.
    assert store.list_time_series(interval=timedelta(hours=2)) == []


def test_close_and_repr():
    store = Store.create(in_memory=True)
    store.add_time_series(
        owner_id=1, owner_type="Generator",
        owner_category=OwnerCategory.Component, time_series=_sts("load", 1.0),
    )
    assert "in-memory" in repr(store)
    assert "read_only=False" in repr(store)
    assert "closed" not in repr(store)

    store.close()
    assert "closed" in repr(store)
    # Subsequent operations raise the base binding error with a clear message.
    with pytest.raises(TimeSeriesError, match="store is closed"):
        store.list_names()
    # close() is idempotent.
    store.close()


def test_context_manager_reopen(tmp_path):
    path = tmp_path / "s.h5"
    with Store.create(path=str(path)) as store:
        store.add_time_series(
            owner_id=1, owner_type="Generator",
            owner_category=OwnerCategory.Component, time_series=_sts("load", 1.0),
        )
        store.flush()
    # The with-block closed the store; operations now raise.
    with pytest.raises(TimeSeriesError, match="store is closed"):
        store.list_names()

    # Reopen read-only via the context manager and read the data back.
    with Store.open(str(path), read_only=True) as ro:
        assert ro.read_only is True
        assert str(path) in repr(ro)
        assert ro.list_names() == ["load"]
