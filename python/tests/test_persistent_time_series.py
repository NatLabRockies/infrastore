"""`PersistentTimeSeries`: the step-function read semantics through the Python
binding, and the per-column-breakpoint `StaticReader` built on them."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import (
    InvalidParameterError,
    NonSequentialTimeSeries,
    NotFoundError,
    OwnerCategory,
    PersistentTimeSeries,
    Store,
    TimeSeriesType,
    decode_element_values,
    encode_element_values,
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
    """The hold-last lookup, reachable from the one binding that needs it.

    Before this, a caller could round-trip a curve through the store and still
    have to re-implement the rule client-side, which is two chances to get the
    boundary wrong.
    """
    series = curve("gas_price", [1, 4, 7, 10])

    # Exactly at a breakpoint: that breakpoint's own row. The boundary is `<=`,
    # which is what right-continuity means.
    assert series.index_in_force_at(month(1)) == 0
    assert series.index_in_force_at(month(7)) == 2
    # Between two: the earlier one is still in force.
    assert series.index_in_force_at(month(4) - timedelta(milliseconds=1)) == 0
    assert series.index_in_force_at(month(6)) == 1
    # Past the last: held forward forever.
    assert series.index_in_force_at(month(10)) == 3
    assert series.index_in_force_at(datetime(2031, 5, 4, tzinfo=timezone.utc)) == 3
    # Before the first: undefined, and an error rather than a clamp to 0.
    with pytest.raises(InvalidParameterError, match="before the first breakpoint"):
        series.index_in_force_at(month(1) - timedelta(milliseconds=1))

    # The index addresses both axes, so it is how a caller gets the value.
    assert series.data[series.index_in_force_at(month(6))] == 40.0


def test_index_in_force_at_survives_a_round_trip():
    """The lookup answers the same on a series read back out of a store as on
    the one that went in — it is a property of the breakpoints, not of how the
    object was built."""
    store = Store.create(in_memory=True)
    months = [1, 4, 7, 10]
    id = add(store, 7, curve("gas_price", months))
    back = store.read_by_id(id)

    for m in range(1, 13):
        at = month(m)
        assert back.data[back.index_in_force_at(at)] == hold_last(months, at)


def test_index_in_force_at_refuses_a_query_spelled_the_other_way():
    """A query bound must be spelled the way the series is. An aware datetime
    against a zoneless series would otherwise be reinterpreted as UTC and
    answered, where the same mismatch on a ranged read is refused."""
    zoneless = PersistentTimeSeries(
        [datetime(2024, m, 1) for m in (1, 6)], np.array([1.0, 2.0]), "wall_clock"
    )
    with pytest.raises(InvalidParameterError, match="zoneless"):
        zoneless.index_in_force_at(datetime(2024, 3, 1, tzinfo=timezone.utc))
    # The naive query it was written with still works.
    assert zoneless.index_in_force_at(datetime(2024, 3, 1)) == 0

    # And the converse: a wall clock cannot query an instant-bearing series.
    zoned = curve("gas_price", [1, 6])
    with pytest.raises(InvalidParameterError, match="does not name one"):
        zoned.index_in_force_at(datetime(2024, 3, 1))


def test_project_onto_agrees_with_hold_last_at_every_instant():
    """The projection read: the type's own lookup applied once per instant.

    A consumer asking "what were these values on each of my simulation
    timestamps" gets the answer from the store that owns the rule, instead of
    re-deriving hold-last beside a copy of the breakpoints.
    """
    months = [1, 4, 7, 10]
    series = curve("gas_price", months)

    # Every breakpoint, the millisecond on each side of one, a mid-step, and an
    # instant far past the last -- the boundary is where a step function gets
    # got wrong.
    at = [month(m) for m in range(1, 13)]
    at += [month(4) - timedelta(milliseconds=1), month(4) + timedelta(milliseconds=1)]
    at.append(datetime(2031, 5, 4, tzinfo=timezone.utc))

    projected = series.project_onto(at)
    assert projected.shape == (len(at),)
    assert list(projected) == [hold_last(months, t) for t in at]


def test_project_onto_is_a_gather_not_a_slice():
    """Unsorted and repeated instants are fine: each resolves independently and
    the caller's order is the output order. Nothing is sorted or deduplicated."""
    series = curve("gas_price", [1, 4, 7, 10])
    projected = series.project_onto([month(9), month(1), month(9), month(5)])
    assert list(projected) == [70.0, 10.0, 70.0, 40.0]


def test_projecting_onto_no_instants_returns_an_empty_array():
    series = curve("gas_price", [1, 4])
    empty = series.project_onto([])
    assert empty.shape == (0,)
    assert empty.dtype == np.float64


def test_one_instant_before_the_first_breakpoint_fails_the_whole_projection():
    """The bad instant is last, after three that resolve fine: the call still
    fails outright rather than returning a partial answer."""
    series = curve("gas_price", [1, 4, 7])
    at = [month(2), month(5), month(8), month(1) - timedelta(milliseconds=1)]
    with pytest.raises(InvalidParameterError, match="before the first breakpoint"):
        series.project_onto(at)


def test_read_projected_evaluates_a_stored_curve():
    store = Store.create(in_memory=True)
    months = [1, 4, 7, 10]
    id = add(store, 7, curve("gas_price", months))

    at = [month(m) for m in range(1, 13)]
    projected = store.read_projected(id, at)
    assert list(projected) == [hold_last(months, t) for t in at]

    # An id naming no row is a stale reference, as for every other read.
    with pytest.raises(NotFoundError):
        store.read_projected(9999, at)


def test_read_projected_by_ids_keeps_each_curve_on_its_own_breakpoints():
    """The one place a set need not share a timeline, so a cohort of curves that
    do not line up is still one call."""
    store = Store.create(in_memory=True)
    ids = [
        add(store, 1, curve("quarterly", [1, 4, 7, 10])),
        add(store, 2, curve("semi", [1, 6])),
    ]
    out = store.read_projected_by_ids(ids, [month(3), month(8)])
    assert [list(a) for a in out] == [[10.0, 70.0], [10.0, 60.0]]


def test_a_projection_over_any_other_type_is_refused():
    """Projecting a SingleTimeSeries would need a resampling policy — the
    application's choice to make, which is why this read is for one type."""
    store = Store.create(in_memory=True)
    id = store.add_time_series(
        owner_id=1,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=NonSequentialTimeSeries(
            [month(1), month(4)], np.array([10.0, 40.0]), "irregular"
        ),
    )
    with pytest.raises(InvalidParameterError, match="resampling policy"):
        store.read_projected(id, [month(2)])


def test_a_projected_curve_decodes_as_its_element_type():
    """What Phase E needs: a step function over cost curves.

    The projection copies rows whole, padding included, and leaves the element
    type alone — so `decode_element_values` reads the result exactly as it reads
    `.data`, and a fuel cost curve stored as a step function comes back as
    curves rather than as a packing.
    """
    store = Store.create(in_memory=True)
    curves = [
        [{"x": 10.0, "y": 100.0}, {"x": 20.0, "y": 210.0}],  # January
        [{"x": 12.0, "y": 130.0}, {"x": 22.0, "y": 250.0}],  # July
    ]
    array = encode_element_values(curves, "piecewise_linear")
    id = store.add_time_series(
        owner_id=1,
        owner_type="ThermalStandard",
        owner_category=OwnerCategory.Component,
        time_series=PersistentTimeSeries([month(1), month(7)], array, "fuel_cost_curve"),
        element_type="piecewise_linear",
        # The non-curve fields ride in application_data: they are per-series
        # constants, not part of the value at an instant.
        application_data='{"volume_hours": 24, "cost_curve_type": "piecewise"}',
    )
    row = store.get_metadata_by_id(id)
    assert row["element_type"] == "piecewise_linear"

    projected = store.read_projected(id, [month(3), month(9), month(1)])
    assert projected.shape == (3, 5)
    assert decode_element_values(projected, row["element_type"]) == [
        curves[0],  # March holds January's curve
        curves[1],  # September holds July's
        curves[0],  # and January is itself
    ]

    # The stored array decodes the same way, which is the point: a projection
    # is the same values in a different order, not a different encoding.
    stored = store.read_by_id(id)
    assert decode_element_values(stored.data, row["element_type"]) == curves


def test_time_series_type_integers_are_append_only():
    """`TimeSeriesType` exposes `__int__`, so its numbering is public.

    `PersistentTimeSeries` is declared last rather than beside the other static
    types, because inserting it beside them would have renumbered every
    forecast variant and changed what `int(TimeSeriesType.Deterministic)`
    returns between two releases. The values below are also the storage codes
    the catalog, the C ABI, and the protobuf enum use, which is the alignment
    that made the accidental renumbering worth catching.
    """
    assert [
        int(TimeSeriesType.SingleTimeSeries),
        int(TimeSeriesType.NonSequentialTimeSeries),
        int(TimeSeriesType.Deterministic),
        int(TimeSeriesType.DeterministicSingleTimeSeries),
        int(TimeSeriesType.Probabilistic),
        int(TimeSeriesType.Scenarios),
        int(TimeSeriesType.PersistentTimeSeries),
    ] == [0, 1, 2, 3, 4, 5, 6]
