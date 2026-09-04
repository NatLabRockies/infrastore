"""Tests for the Phase-3 Python surface: readers, discovery/removal,
richer metadata rows, transform params, application_data, key set semantics."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import (
    Deterministic,
    InvalidParameterError,
    NonSequentialTimeSeries,
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


def test_add_application_data_and_get_metadata():
    store = Store.create(in_memory=True)
    key = store.add_time_series(
        owner_id=1, owner_type="Generator", owner_category=OwnerCategory.Component,
        time_series=_sts("load", 10.0), units="MW", application_data="Profile",
    )
    meta = store.get_metadata_by_id(key)
    assert meta["units"] == "MW"
    assert meta["application_data"] == "Profile"
    assert meta["element_type"] == "f64"
    assert meta["element_shape"] == []
    assert meta["initial_timestamp"] is not None


def test_unit_descriptors_round_trip():
    store = Store.create(in_memory=True)
    key = store.add_time_series(
        owner_id=1, owner_type="Generator", owner_category=OwnerCategory.Component,
        time_series=_sts("load", 10.0), units="MW",
        quantity_kind="ActivePower", unit_system="component_base",
        component_field="max_active_power",
    )
    meta = store.get_metadata_by_id(key)
    assert meta["quantity_kind"] == "ActivePower"
    assert meta["unit_system"] == "component_base"
    assert meta["component_field"] == "max_active_power"
    # The list row is the same record, so it must agree with the point lookup.
    row = store.list_metadata()[0]
    assert row["unit_system"] == "component_base"
    assert row["component_field"] == "max_active_power"



def test_component_field_filter():
    store = Store.create(in_memory=True)
    for owner, name, field in [
        (1, "max_active_power", "max_active_power"),
        (1, "rating", "rating"),
        (2, "max_active_power", "max_active_power"),
        (3, "legacy", None),
    ]:
        kwargs = {"component_field": field} if field else {}
        store.add_time_series(
            owner_id=owner, owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=_sts(name, float(owner)), **kwargs,
        )

    # One field, every component that varies it.
    keys = store.list_metadata(component_field="max_active_power")
    assert sorted(k['owner_id'] for k in keys) == [1, 2]

    # Composes with the owner scope.
    scoped = store.list_metadata(owner_id=1, component_field="max_active_power")
    assert len(scoped) == 1

    # Exact and case-sensitive; no glob semantics.
    assert store.list_metadata(component_field="max_active") == []
    assert store.list_metadata(component_field="Max_Active_Power") == []

    # A row that declares none is unreachable through this filter.
    assert store.list_metadata(component_field="legacy") == []

    # It reaches the reader filter too, which is the columnar sweep case.
    reader = store.build_static_reader(
        timedelta(hours=1), component_field="max_active_power"
    )
    assert sum(len(g["ids"]) for g in reader.groups()) == 2


def test_unit_system_unset_is_unspecified_not_natural_units():
    # Omitting the basis records nothing. Reading it back as "natural_units"
    # would assert a basis the writer never declared -- and would silently
    # mislabel per-unit values written by an older build.
    store = Store.create(in_memory=True)
    key = store.add_time_series(
        owner_id=1, owner_type="Generator", owner_category=OwnerCategory.Component,
        time_series=_sts("load", 10.0), units="MW",
    )
    meta = store.get_metadata_by_id(key)
    assert meta["unit_system"] is None
    assert meta["quantity_kind"] is None
    assert meta["component_field"] is None


def test_unknown_unit_system_is_rejected():
    # Raising beats degrading to None: a misspelled basis that silently became
    # "unspecified" would leave per-unit values indistinguishable from
    # undeclared ones.
    store = Store.create(in_memory=True)
    with pytest.raises(InvalidParameterError):
        store.add_time_series(
            owner_id=1, owner_type="Generator", owner_category=OwnerCategory.Component,
            time_series=_sts("load", 10.0), unit_system="system_base",
        )


def test_bulk_read_time_range():
    store = Store.create(in_memory=True)
    k1 = store.add_time_series(owner_id=1, owner_type="Generator",
                              owner_category=OwnerCategory.Component, time_series=_sts("load", 100.0))
    k2 = store.add_time_series(owner_id=2, owner_type="Generator",
                              owner_category=OwnerCategory.Component, time_series=_sts("load", 200.0))
    rng = (_t0() + timedelta(hours=2), _t0() + timedelta(hours=5))
    sliced = store.read_by_ids_range([k1, k2], rng)
    for i, k in enumerate([k1, k2]):
        expected = store.read_by_ids_range([k], rng)[0]
        np.testing.assert_array_equal(sliced[i].data, expected.data)


def test_discovery_and_removal():
    store = Store.create(in_memory=True)
    store.add_time_series(owner_id=1, owner_type="Generator",
                          owner_category=OwnerCategory.Component, time_series=_sts("load", 1.0))
    store.add_time_series(owner_id=2, owner_type="Bus",
                          owner_category=OwnerCategory.Component, time_series=_sts("voltage", 2.0))
    store.add_time_series(owner_id=3, owner_type="Generator",
                          owner_category=OwnerCategory.Component, time_series=_det("fc"))

    assert store.get_intervals() == ["PT1H"]
    assert store.get_intervals(time_series_type=TimeSeriesType.SingleTimeSeries) == []
    assert sorted(store.list_names()) == ["fc", "load", "voltage"]
    assert store.list_names(owner_type="Generator") == ["fc", "load"]
    assert sorted(store.list_owner_types()) == ["Bus", "Generator"]

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
    # An id is a plain value: the same series listed again gives the same id,
    # and ids work in a set with no equality of their own to keep consistent.
    (row,) = store.list_metadata(owner_id=1)
    k1_again = row["id"]
    assert k1_again == k1
    assert len({k1, k2, k1_again}) == 2


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

    assert grid["time_series_type"] == "SingleTimeSeries"


def test_static_reader_over_an_irregular_cohort():
    """Irregular series sharing one timestamp vector read columnar too: the
    cohort is a real timeline, just an explicit one."""
    store = Store.create(in_memory=True)
    # Three instants with no constant step between them.
    stamps = [_t0(), _t0() + timedelta(minutes=37), _t0() + timedelta(hours=9)]
    for owner, base in ((2, 20.0), (1, 10.0)):
        data = np.arange(len(stamps), dtype=np.float64) + base
        store.add_time_series(
            owner_id=owner,
            owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=NonSequentialTimeSeries(stamps, data, "outage"),
        )

    reader = store.build_static_reader(
        time_series_type=TimeSeriesType.NonSequentialTimeSeries
    )
    grid = reader.grid()
    assert grid["time_series_type"] == "NonSequentialTimeSeries"
    assert grid["length"] == 3
    # No constant step to report; the instants themselves are the timeline.
    assert grid["resolution"] is None
    assert reader.timestamps() == stamps

    for index, at in enumerate(stamps):
        store.static_read(reader, at)
        np.testing.assert_array_equal(
            reader.group_values(0), np.array([10.0 + index, 20.0 + index])
        )

    # Between two instants there is no value to read.
    with pytest.raises(InvalidParameterError):
        store.static_read(reader, _t0() + timedelta(minutes=1))
    # An irregular series has no resolution, so filtering on one is refused.
    with pytest.raises(InvalidParameterError):
        store.build_static_reader(
            timedelta(hours=1),
            time_series_type=TimeSeriesType.NonSequentialTimeSeries,
        )
    # And a series on a different axis cannot join the cohort.
    store.add_time_series(
        owner_id=3,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=NonSequentialTimeSeries(
            [_t0(), _t0() + timedelta(minutes=38), _t0() + timedelta(hours=9)],
            np.array([1.0, 2.0, 3.0]),
            "outage",
        ),
    )
    with pytest.raises(InvalidParameterError):
        store.build_static_reader(
            time_series_type=TimeSeriesType.NonSequentialTimeSeries
        )


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
    rows = store.list_metadata(interval=timedelta(hours=1))
    assert len(rows) == 1
    row = rows[0]
    for field in ("initial_timestamp", "horizon", "interval", "count",
                  "percentiles", "element_type", "element_shape", "application_data",
                  "quantity_kind", "unit_system", "component_field"):
        assert field in row
    assert row["interval"] == "PT1H"
    # No forecast at a different interval.
    assert store.list_metadata(interval=timedelta(hours=2)) == []


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
