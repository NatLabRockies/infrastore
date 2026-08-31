"""`PersistentTimeSeries`: the step-function read semantics through the Python
binding, and the per-column-breakpoint `StaticReader` built on them."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import (
    InvalidParameterError,
    NonSequentialTimeSeries,
    OwnerCategory,
    PersistentTimeSeries,
    Store,
    TimeSeriesType,
)


def month(m: int) -> datetime:
    return datetime(2024, m, 1, tzinfo=timezone.utc)


def curve(name: str, months: list[int]) -> PersistentTimeSeries:
    """A step function over `months`, valued `10 * month`."""
    values = np.array([m * 10.0 for m in months], dtype=np.float64)
    return PersistentTimeSeries([month(m) for m in months], values, name)


def add(store: Store, owner_id: int, series: PersistentTimeSeries) -> int:
    return store.add_time_series(
        owner_id=owner_id,
        owner_type="ThermalStandard",
        owner_category=OwnerCategory.Component,
        time_series=series,
    )


def hold_last(months: list[int], at: datetime) -> float:
    """The value in force at `at`, computed without the store."""
    return max(m for m in months if month(m) <= at) * 10.0


def test_round_trip_and_descriptors():
    store = Store.create(in_memory=True)
    series = curve("gas_price", [1, 4, 7, 10])
    id = store.add_time_series(
        owner_id=7,
        owner_type="ThermalStandard",
        owner_category=OwnerCategory.Component,
        time_series=series,
        # The application's own expansion policy rides in application_data; the
        # store never interprets it.
        application_data='{"as_time_series": false, "force_scalar_mode": "midpoint"}',
        units="USD/MMBtu",
        component_field="fuel_cost",
    )

    back = store.read_by_id(id)
    assert isinstance(back, PersistentTimeSeries)
    assert back.timestamps == [month(m) for m in (1, 4, 7, 10)]
    np.testing.assert_array_equal(back.data, series.data)
    assert len(back) == 4
    assert "PersistentTimeSeries" in repr(back)

    meta = store.get_metadata_by_id(id)
    assert meta["time_series_type"] == "PersistentTimeSeries"
    assert meta["units"] == "USD/MMBtu"
    assert meta["component_field"] == "fuel_cost"
    assert (
        meta["application_data"]
        == '{"as_time_series": false, "force_scalar_mode": "midpoint"}'
    )


def test_a_range_read_starts_at_the_breakpoint_in_force():
    store = Store.create(in_memory=True)
    id = store.add_time_series(
        owner_id=1,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=curve("gas", [1, 4, 7, 10]),
    )

    # A window opening mid-April must still define a value at its start, so the
    # slice begins at the April breakpoint — one earlier than the first
    # breakpoint inside the window. A NonSequentialTimeSeries would start at
    # July, and that difference is the whole reason this is a separate type.
    (sliced,) = store.read_by_ids_range(
        [id], (month(4) + timedelta(days=10), month(9))
    )
    assert sliced.timestamps == [month(4), month(7)]
    np.testing.assert_array_equal(sliced.data, np.array([40.0, 70.0]))

    # A window entirely inside one step still returns that step.
    (inside,) = store.read_by_ids_range(
        [id], (month(5), month(5) + timedelta(days=1))
    )
    assert inside.timestamps == [month(4)]

    # Before the first breakpoint a step function is undefined; the store says
    # so rather than clamping.
    with pytest.raises(InvalidParameterError, match="before the first breakpoint"):
        store.read_by_ids_range([id], (month(1) - timedelta(days=1), month(6)))


def test_malformed_breakpoints_are_refused():
    with pytest.raises(InvalidParameterError, match="strictly increasing"):
        PersistentTimeSeries(
            [month(4), month(1)], np.array([1.0, 2.0]), "backwards"
        )
    with pytest.raises(InvalidParameterError):
        PersistentTimeSeries([month(1)], np.array([1.0, 2.0]), "mismatched")


def test_it_shares_storage_with_an_identical_non_sequential_series():
    """`PackGroup` is keyed by the time axis, never by the series type, so
    identical bytes on identical timestamps dedup across the two types."""
    store = Store.create(in_memory=True)
    stamps = [month(1), month(4), month(7)]
    values = np.array([10.0, 40.0, 70.0])
    for series in (
        PersistentTimeSeries(stamps, values, "shared"),
        NonSequentialTimeSeries(stamps, values, "shared"),
    ):
        store.add_time_series(
            owner_id=1,
            owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=series,
        )
    assert len(store.list_metadata()) == 2
    assert store.num_distinct_arrays() == 1


def test_a_naive_breakpoint_vector_is_zoneless():
    """The spelling is inferred from the input exactly as it is for every other
    type: a naive datetime is a wall clock naming no instant."""
    store = Store.create(in_memory=True)
    naive = [datetime(2024, m, 1) for m in (1, 6)]
    series = PersistentTimeSeries(naive, np.array([1.0, 2.0]), "wall_clock")
    assert series.time_reference == "zoneless"
    id = store.add_time_series(
        owner_id=1,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=series,
    )
    back = store.read_by_id(id)
    assert back.time_reference == "zoneless"
    assert back.timestamps == naive


def test_static_reader_over_columns_with_independent_breakpoints():
    """The one place a reader's columns need not share a timeline: a step
    function has a value at every instant from its first breakpoint on."""
    store = Store.create(in_memory=True)
    monthly = list(range(1, 13))
    quarterly = [1, 4, 7, 10]
    semi = [1, 6]
    add(store, 1, curve("monthly", monthly))
    add(store, 2, curve("quarterly", quarterly))
    add(store, 3, curve("semi", semi))

    reader = store.build_static_reader(
        time_series_type=TimeSeriesType.PersistentTimeSeries
    )
    grid = reader.grid()
    assert grid["time_series_type"] == "PersistentTimeSeries"
    # No constant step, and the axis is the union of every column's breakpoints:
    # here the monthly vector, which subsumes the other two.
    assert grid["resolution"] is None
    assert reader.timestamps() == [month(m) for m in monthly]

    # Column order is deterministic; map each column to the vector it sits on.
    groups = reader.groups()
    # A group holds association ids, so a test that wants names asks the store
    # for them — the round trip a caller makes.
    names = [
        store.get_metadata_by_id(id)["name"]
        for group in groups
        for id in group["ids"]
    ]
    vectors = {"monthly": monthly, "quarterly": quarterly, "semi": semi}

    # Sweep every union instant and compare each column against an
    # independently computed hold-last reference. This is what catches a
    # desynchronized column-to-vector mapping, which otherwise produces
    # plausible wrong numbers rather than an error.
    for at in reader.timestamps():
        store.static_read(reader, at)
        got = np.concatenate([reader.group_values(g) for g in range(len(groups))])
        want = np.array([hold_last(vectors[n], at) for n in names])
        np.testing.assert_array_equal(got, want, err_msg=f"at {at}")


def test_reading_before_a_column_s_first_breakpoint_names_that_column():
    store = Store.create(in_memory=True)
    add(store, 1, curve("early", [1, 6]))
    late = add(store, 2, curve("late", [9]))
    reader = store.build_static_reader(
        time_series_type=TimeSeriesType.PersistentTimeSeries
    )
    # The union starts in January, but `late` has nothing before September. The
    # error names the column by the id a caller can look it up with.
    with pytest.raises(InvalidParameterError, match=f"association {late}"):
        store.static_read(reader, month(1))
    # From September on every column resolves.
    store.static_read(reader, month(9))


def test_a_persistent_reader_takes_no_resolution():
    store = Store.create(in_memory=True)
    add(store, 1, curve("gas", [1, 6]))
    with pytest.raises(InvalidParameterError, match="no resolution filter"):
        store.build_static_reader(
            timedelta(hours=1),
            time_series_type=TimeSeriesType.PersistentTimeSeries,
        )


def test_a_bulk_read_reconstructs_the_type():
    store = Store.create(in_memory=True)
    ids = [
        store.add_time_series(
            owner_id=owner,
            owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=curve("gas", months),
        )
        for owner, months in ((1, [1, 6]), (2, [2, 3, 11]))
    ]
    out = store.read_by_ids(ids)
    assert all(isinstance(s, PersistentTimeSeries) for s in out)
    assert [len(s) for s in out] == [2, 3]


def test_index_in_force_at_covers_the_four_boundary_cases():
    """The lookup that defines the type, reachable from the binding that needs
    it. Without this a consumer round-trips a curve through the store and still
    has to reimplement hold-last client-side, leaving two implementations free
    to drift."""
    months = [1, 4, 7]
    c = curve("gas", months)

    def value_at(at: datetime) -> float:
        return float(c.data[c.index_in_force_at(at)])

    # 1. Exactly at a breakpoint -> that breakpoint's value (right-continuous).
    for m in months:
        assert value_at(month(m)) == m * 10.0

    # 2. Between breakpoints -> the previous value. This is the case that
    #    diverges from NonSequentialTimeSeries, where it is a hard error.
    assert value_at(month(2)) == 10.0
    assert value_at(month(4) + timedelta(seconds=1)) == 40.0

    # 3. After the last breakpoint -> the last value, forever.
    assert value_at(month(12)) == 70.0
    assert value_at(datetime(2099, 1, 1, tzinfo=timezone.utc)) == 70.0

    # 4. Before the first -> an error naming the series, never a clamp.
    with pytest.raises(InvalidParameterError) as excinfo:
        c.index_in_force_at(month(1) - timedelta(milliseconds=1))
    assert "gas" in str(excinfo.value)
    assert "before the first breakpoint" in str(excinfo.value)


def test_index_in_force_at_agrees_with_a_series_read_back_from_a_store():
    """The lookup is a property of the series, so it must survive the round
    trip -- which is the whole point, since the consumer asking is holding a
    curve the store handed it."""
    months = [1, 3, 6, 11]
    store = Store.create(in_memory=True)
    ident = add(store, 1, curve("fuel", months))
    back = store.read_by_id(ident)
    for m in range(1, 13):
        at = month(m)
        assert float(back.data[back.index_in_force_at(at)]) == hold_last(months, at)


def test_index_in_force_at_refuses_a_query_spelled_unlike_the_series():
    """A naive datetime does not name an instant, so it cannot be resolved
    against breakpoints that do -- the same rule a ranged read applies, rather
    than silently reading the wall clock as UTC."""
    aware = curve("aware", [1, 4])
    with pytest.raises(InvalidParameterError):
        aware.index_in_force_at(datetime(2024, 2, 1))

    naive_stamps = [datetime(2024, 1, 1), datetime(2024, 4, 1)]
    zoneless = PersistentTimeSeries(naive_stamps, np.array([1.0, 2.0]), "wall_clock")
    assert zoneless.index_in_force_at(datetime(2024, 2, 1)) == 0
    with pytest.raises(InvalidParameterError):
        zoneless.index_in_force_at(month(2))
