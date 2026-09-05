"""`SingleTimeSeries.timestamps`: the materialized grid.

Deliberately *not* in `test_arrow.py`. That module skips wholesale without
pyarrow, and this property is part of the default install — putting its only
coverage there would leave it untested in the environment most callers have.

The irregular types' `timestamps` is a stored vector and is covered by the
round-trip suites; what needs its own tests is the computed one, because a
`P1M` resolution steps on the calendar and no multiplication reproduces that.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo

import numpy as np
import pytest

from infrastore import (
    InvalidParameterError,
    OwnerCategory,
    SingleTimeSeries,
    Store,
)

UTC_START = datetime(2024, 1, 1, tzinfo=timezone.utc)


def hourly(length: int, **kwargs) -> SingleTimeSeries:
    return SingleTimeSeries(
        UTC_START, "PT1H", np.arange(float(length)), "load", **kwargs
    )


def test_walks_the_fixed_grid():
    assert hourly(3).timestamps == [UTC_START + timedelta(hours=k) for k in range(3)]


def test_length_matches_the_series():
    assert len(hourly(24).timestamps) == 24


def test_one_entry_per_time_step_not_per_element():
    """A multidimensional per-step value has more elements than time steps."""
    series = SingleTimeSeries(UTC_START, "PT1H", np.arange(24.0).reshape(4, 2, 3), "md")
    assert len(series.timestamps) == 4


def test_empty_series_has_no_timestamps():
    assert hourly(0).timestamps == []


def test_month_resolution_steps_on_the_calendar():
    """The whole reason this is computed in the core rather than left to the
    caller: month ends are not a multiple of any fixed span."""
    series = SingleTimeSeries(
        datetime(2024, 1, 31, tzinfo=timezone.utc), "P1M", np.arange(4.0), "monthly"
    )
    assert series.timestamps == [
        datetime(2024, 1, 31, tzinfo=timezone.utc),
        datetime(2024, 2, 29, tzinfo=timezone.utc),
        datetime(2024, 3, 31, tzinfo=timezone.utc),
        datetime(2024, 4, 30, tzinfo=timezone.utc),
    ]


def test_year_resolution_crosses_a_leap_day():
    series = SingleTimeSeries(
        datetime(2024, 2, 29, tzinfo=timezone.utc), "P1Y", np.arange(2.0), "yearly"
    )
    assert series.timestamps[1] == datetime(2025, 2, 28, tzinfo=timezone.utc)


def test_first_entry_is_the_initial_timestamp():
    series = hourly(5)
    assert series.timestamps[0] == series.initial_timestamp


# ---- Spelling --------------------------------------------------------------


def test_timestamps_are_spelled_the_way_the_series_was_written():
    denver = ZoneInfo("America/Denver")
    series = SingleTimeSeries(
        datetime(2024, 1, 1, tzinfo=denver), "PT1H", np.arange(3.0), "zoned"
    )
    assert [t.tzinfo for t in series.timestamps] == [denver] * 3


def test_zoneless_series_gives_naive_timestamps():
    """A naive datetime names a wall clock; handing back aware ones would make
    the values incomparable with what the caller wrote."""
    series = SingleTimeSeries(datetime(2024, 1, 1), "PT1H", np.arange(3.0), "naive")
    assert series.timestamps == [datetime(2024, 1, 1, k) for k in range(3)]
    assert all(t.tzinfo is None for t in series.timestamps)


def test_a_zoned_grid_crosses_a_dst_transition_by_instant():
    """A `PT1H` resolution steps instants, not wall clocks, so the spring-forward
    hour is skipped in local time — the grid is a spelling, not a local clock.
    A local-clock grid belongs in a NonSequentialTimeSeries.

    The subtraction is spelled through UTC on purpose. Python ignores a shared
    `tzinfo` when subtracting two aware datetimes and differences their wall
    clocks instead, so `stamps[1] - stamps[0]` reads 2 hours across this
    transition even though the instants are one hour apart. That is a trap for
    anyone measuring a step off these values, and it is Python's, not the
    store's — the instants below are exactly one resolution apart.
    """
    denver = ZoneInfo("America/Denver")
    series = SingleTimeSeries(
        datetime(2024, 3, 10, 1, tzinfo=denver), "PT1H", np.arange(2.0), "dst"
    )
    stamps = series.timestamps
    assert stamps[0].hour == 1
    assert stamps[1].hour == 3  # 02:00 local does not exist that day
    as_utc = [t.astimezone(timezone.utc) for t in stamps]
    assert as_utc[1] - as_utc[0] == timedelta(hours=1)
    # The wall-clock reading Python gives for the same pair, pinned so the
    # difference between the two is a documented fact rather than a surprise.
    assert stamps[1] - stamps[0] == timedelta(hours=2)


# ---- Through the store -----------------------------------------------------


def test_grid_survives_a_store_round_trip():
    store = Store.create(in_memory=True)
    series = SingleTimeSeries(
        datetime(2024, 1, 31, tzinfo=timezone.utc), "P1M", np.arange(3.0), "monthly"
    )
    series_id = store.add_time_series(
        owner_id=1,
        owner_type="ThermalStandard",
        owner_category=OwnerCategory.Component,
        time_series=series,
    )
    assert store.read_by_id(series_id).timestamps == series.timestamps


def test_timestamps_needs_no_optional_dependency(monkeypatch):
    """It is part of the default install, unlike `to_arrow()`."""
    import sys

    monkeypatch.setitem(sys.modules, "pyarrow", None)
    assert len(hourly(3).timestamps) == 3


@pytest.mark.parametrize("dtype", ["float32", "int64", "bool"])
def test_grid_is_independent_of_the_value_dtype(dtype):
    series = SingleTimeSeries(UTC_START, "PT1H", np.ones(3, dtype=dtype), "x")
    assert series.timestamps == [UTC_START + timedelta(hours=k) for k in range(3)]


# ---- from_timestamps: hand over the timeline you have ----------------------


def denver_walk(start: datetime, n: int, step: timedelta) -> list[datetime]:
    """A local-clock walk stepped in *instants*.

    `aware + timedelta` is wall-clock arithmetic in Python and would silently
    skip the repeated hour on a fall-back day; going through UTC is what makes
    this the timeline a meter actually records. See
    `test_wall_clock_arithmetic_skips_an_hour_and_is_caught`.
    """
    out, t = [], start
    for _ in range(n):
        out.append(t)
        t = (t.astimezone(timezone.utc) + step).astimezone(start.tzinfo)
    return out


def test_hourly_local_grid_in_a_dst_zone_compacts():
    """The case the store must keep supporting: DST moves the offset, not the
    length of an hour, so an hourly local grid *is* a uniform instant grid."""
    denver = ZoneInfo("America/Denver")
    hours = denver_walk(datetime(2024, 11, 3, tzinfo=denver), 6, timedelta(hours=1))
    series = SingleTimeSeries.from_timestamps(hours, np.arange(6.0), "load")
    assert series.resolution == "PT1H"
    assert series.timestamps == hours
    assert series.time_reference == "America/Denver"


def test_daily_local_grid_in_a_dst_zone_is_refused_naming_the_remedy():
    denver = ZoneInfo("America/Denver")
    days = denver_walk(datetime(2024, 11, 1, tzinfo=denver), 5, timedelta(days=1))
    # Stepping a *day* of instants does not stay on local midnight.
    with pytest.raises(InvalidParameterError, match="NonSequentialTimeSeries"):
        SingleTimeSeries.from_timestamps(
            [datetime(2024, 11, d, tzinfo=denver) for d in range(1, 6)],
            np.arange(5.0),
            "peak",
        )
    # And the instant-stepped version is a plain P1D grid, which is fine.
    assert SingleTimeSeries.from_timestamps(days, np.arange(5.0), "peak").resolution == "P1D"


def test_wall_clock_arithmetic_skips_an_hour_and_is_caught():
    """`aware + timedelta` steps the wall clock, so on a fall-back day it jumps
    01:00 straight to 02:00 and loses a real hour. Nothing in the values marks
    it; asserting `resolution="PT1H"` would have stored the gap silently."""
    denver = ZoneInfo("America/Denver")
    base = datetime(2024, 11, 3, tzinfo=denver)
    walked = [base + timedelta(hours=k) for k in range(4)]
    as_utc = [t.astimezone(timezone.utc).hour for t in walked]
    assert as_utc == [6, 7, 9, 10]  # 08:00 UTC is missing
    with pytest.raises(InvalidParameterError, match="index 2"):
        SingleTimeSeries.from_timestamps(walked, np.arange(4.0), "load")


def test_month_end_grid_infers_calendar_months():
    stamps = [
        datetime(2024, 1, 31, tzinfo=timezone.utc),
        datetime(2024, 2, 29, tzinfo=timezone.utc),
        datetime(2024, 3, 31, tzinfo=timezone.utc),
    ]
    series = SingleTimeSeries.from_timestamps(stamps, np.arange(3.0), "monthly")
    assert series.resolution == "P1M"
    assert series.timestamps == stamps


def test_length_must_match_the_array():
    with pytest.raises(InvalidParameterError, match="one entry per time step"):
        SingleTimeSeries.from_timestamps(
            [UTC_START, UTC_START + timedelta(hours=1)], np.arange(5.0), "x"
        )


def test_a_single_timestamp_cannot_imply_a_resolution():
    with pytest.raises(InvalidParameterError, match="at least two"):
        SingleTimeSeries.from_timestamps([UTC_START], np.arange(1.0), "x")


def test_descriptors_and_multidimensional_values_carry_through():
    stamps = [UTC_START + timedelta(hours=k) for k in range(3)]
    series = SingleTimeSeries.from_timestamps(
        stamps, np.arange(12.0).reshape(3, 4), "curve", units="MW", component_field="max_active_power"
    )
    assert series.units == "MW"
    assert series.component_field == "max_active_power"
    assert series.length == 3
    assert np.asarray(series.data).shape == (3, 4)


def test_a_proven_series_stores_and_reads_back():
    denver = ZoneInfo("America/Denver")
    hours = denver_walk(datetime(2024, 11, 3, tzinfo=denver), 6, timedelta(hours=1))
    store = Store.create(in_memory=True)
    series = SingleTimeSeries.from_timestamps(hours, np.arange(6.0), "load")
    series_id = store.add_time_series(
        owner_id=1,
        owner_type="ThermalStandard",
        owner_category=OwnerCategory.Component,
        time_series=series,
    )
    assert store.read_by_id(series_id).timestamps == hours


# ---- The refusal -----------------------------------------------------------


def test_a_calendar_scale_period_on_a_named_zone_is_refused_on_add():
    """Asserting a resolution the store cannot check is where this went wrong;
    the refusal is at `add`, which is the funnel every write passes through."""
    denver = ZoneInfo("America/Denver")
    store = Store.create(in_memory=True)
    for resolution in ["P1D", "P7D", "P1M"]:
        series = SingleTimeSeries(
            datetime(2024, 11, 1, tzinfo=denver), resolution, np.arange(5.0), "peak"
        )
        with pytest.raises(InvalidParameterError, match="from_timestamps"):
            store.add_time_series(
                owner_id=1,
                owner_type="ThermalStandard",
                owner_category=OwnerCategory.Component,
                time_series=series,
            )


@pytest.mark.parametrize("resolution", ["PT15M", "PT1H", "PT6H"])
def test_sub_daily_periods_on_a_named_zone_stay_legal(resolution):
    """Refusing these would push callers onto a fixed offset, which is silently
    wrong for half the year."""
    denver = ZoneInfo("America/Denver")
    store = Store.create(in_memory=True)
    series = SingleTimeSeries(
        datetime(2024, 11, 1, tzinfo=denver), resolution, np.arange(5.0), "load"
    )
    assert store.add_time_series(
        owner_id=1,
        owner_type="ThermalStandard",
        owner_category=OwnerCategory.Component,
        time_series=series,
    )


@pytest.mark.parametrize("reference", ["utc", "zoneless", "-07:00"])
def test_a_calendar_period_is_allowed_where_it_cannot_drift(reference):
    store = Store.create(in_memory=True)
    series = SingleTimeSeries(
        datetime(2024, 11, 1, tzinfo=timezone.utc),
        "P1M",
        np.arange(5.0),
        "monthly",
        time_reference=reference,
    )
    assert store.add_time_series(
        owner_id=1,
        owner_type="ThermalStandard",
        owner_category=OwnerCategory.Component,
        time_series=series,
    )
