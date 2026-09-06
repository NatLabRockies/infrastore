"""`initial_timestamp` / `length` as *filter* arguments: select one grid.

Two `SingleTimeSeries` on different owners may share a name and a resolution and
still start at different instants — legal, since the catalog files them under
distinct owners — and a `StaticReader` over both cannot be built. There are two
answers, and they are not the same answer:

* a **window** (`window_start` / `window_length`) sweeps a span across whatever
  matched, letting each column read at an offset of its own;
* this **filter** matches only the series already on one grid, so the odd ones
  out are not there to constrain the sweep at all.

Use the window when the ragged series should all take part, the filter when they
should not. These tests pin the filter, and that the two compose rather than
collide.
"""

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import InvalidParameterError, OwnerCategory, SingleTimeSeries, Store

UTC = timezone.utc
HOUR = timedelta(hours=1)


def t(hour):
    return datetime(2024, 1, 1, tzinfo=UTC) + timedelta(hours=hour)


def add(store, owner, start, length, name="active_power"):
    return store.add_time_series(
        owner_id=owner,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=SingleTimeSeries(
            t(start), HOUR, np.arange(start, start + length, dtype=np.float64), name
        ),
    )


@pytest.fixture
def mixed():
    """One stray day of data beside two full-length series on a later grid."""
    store = Store.create(in_memory=True)
    add(store, 7, 0, 24)
    add(store, 42, 7, 8784)
    add(store, 43, 7, 8784)
    return store


class TestListing:
    def test_initial_timestamp_alone(self, mixed):
        rows = mixed.list_metadata(initial_timestamp=t(7))
        assert sorted(r["owner_id"] for r in rows) == [42, 43]

    def test_length_alone(self, mixed):
        rows = mixed.list_metadata(length=24)
        assert [r["owner_id"] for r in rows] == [7]

    def test_the_pair_names_a_whole_grid(self, mixed):
        rows = mixed.list_metadata(resolution="PT1H", initial_timestamp=t(7), length=8784)
        assert sorted(r["owner_id"] for r in rows) == [42, 43]

    def test_a_grid_no_row_is_on_matches_nothing(self, mixed):
        # A filter selects; it does not assert. An anchor nothing was written
        # with is an empty listing, not an error.
        assert mixed.list_metadata(initial_timestamp=t(3)) == []
        assert mixed.list_metadata(initial_timestamp=t(7), length=99) == []

    def test_irregular_rows_match_no_initial_timestamp(self):
        # They store none, and SQL equality is never true against NULL — the
        # same trap `component_field` documents.
        store = Store.create(in_memory=True)
        from infrastore import NonSequentialTimeSeries

        store.add_time_series(
            owner_id=1,
            owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=NonSequentialTimeSeries(
                [t(0), t(5)], np.array([1.0, 2.0]), "sparse"
            ),
        )
        assert len(store.list_metadata()) == 1
        assert store.list_metadata(initial_timestamp=t(0)) == []

    def test_it_reaches_the_other_filter_taking_calls(self, mixed):
        assert mixed.has_any_time_series(initial_timestamp=t(7), length=8784)
        assert not mixed.has_any_time_series(initial_timestamp=t(7), length=24)
        assert mixed.list_names(length=24) == ["active_power"]
        assert mixed.list_owner_types(initial_timestamp=t(0)) == ["Generator"]


class TestReader:
    def test_it_makes_a_ragged_store_readable_at_full_length(self, mixed):
        # The point of the filter: the 24-hour series is excluded, so the sweep
        # is the whole 8784 rather than the 17 hours the three have in common.
        with pytest.raises(InvalidParameterError):
            mixed.build_static_reader(resolution="PT1H")

        reader = mixed.build_static_reader(
            resolution="PT1H", initial_timestamp=t(7), length=8784
        )
        assert reader.grid()["length"] == 8784
        assert len(reader.groups()[0]["ids"]) == 2
        mixed.static_read(reader, t(8790))
        assert list(reader.group_values(0)) == [8790.0, 8790.0]

    def test_the_filter_and_the_window_compose(self, mixed):
        # Select the cohort, then sweep a span inside it.
        reader = mixed.build_static_reader(
            resolution="PT1H",
            initial_timestamp=t(7),
            length=8784,
            window_start=t(100),
            window_length=24,
        )
        assert reader.grid()["initial_timestamp"].startswith("2024-01-05T04:00")  # t(100)
        assert reader.grid()["length"] == 24

    def test_the_two_are_different_arguments(self, mixed):
        # `window_start` sweeps across ragged series; `initial_timestamp` picks
        # which series are there at all. Same instant, different outcome.
        windowed = mixed.build_static_reader(resolution="PT1H", window_start=t(7))
        assert len(windowed.groups()[0]["ids"]) == 3   # the stray joins
        assert windowed.grid()["length"] == 17         # and caps the span

        filtered = mixed.build_static_reader(resolution="PT1H", initial_timestamp=t(7))
        assert len(filtered.groups()[0]["ids"]) == 2   # the stray is gone
        assert filtered.grid()["length"] == 8784


class TestRemoval:
    def test_removing_the_stray_cohort(self, mixed):
        # The maintenance operation the filter makes expressible: retire one
        # grid without naming its owners.
        assert mixed.remove_by_filter(initial_timestamp=t(0), length=24) == 1
        reader = mixed.build_static_reader(resolution="PT1H")
        assert reader.grid()["length"] == 8784
