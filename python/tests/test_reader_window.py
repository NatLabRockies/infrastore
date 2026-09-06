"""`build_static_reader(window_start=..., window_length=...)`.

Without a window a `StaticReader` takes its grid from the series it matched, so
a store whose `SingleTimeSeries` start at different instants or run for
different lengths has no reader at all — the shape of most real systems, where a
year of load sits beside a shorter schedule. Naming a window makes those sweep
together: each column reads at an offset of its own.

The interesting assertions are the refusals. A column silently dropped for not
covering the span, or an anchor quietly floored onto a step, would both return a
full, plausible, wrong row.
"""

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import InvalidParameterError, OwnerCategory, SingleTimeSeries, Store

UTC = timezone.utc
HOUR = timedelta(hours=1)


def t(hour):
    return datetime(2024, 1, 1, tzinfo=UTC) + timedelta(hours=hour)


def add(store, owner, name, start, length):
    """Hourly values equal to their own hour, so a read proves which row it hit."""
    return store.add_time_series(
        owner_id=owner,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=SingleTimeSeries(
            t(start),
            HOUR,
            np.arange(start, start + length, dtype=np.float64),
            name,
        ),
    )


@pytest.fixture
def ragged():
    """The shape that has no uniform reader: 24 hours beside a leap year."""
    store = Store.create(in_memory=True)
    add(store, 7, "load", 0, 24)
    add(store, 42, "active_power", 7, 8784)
    return store


class TestWithoutAWindow:
    def test_a_ragged_store_still_refuses(self, ragged):
        with pytest.raises(InvalidParameterError) as e:
            ragged.build_static_reader(resolution="PT1H")
        assert "requires a uniform grid" in str(e.value)

    def test_the_refusal_points_at_the_window(self, ragged):
        # The message a caller hits first is the one that has to say what to do.
        with pytest.raises(InvalidParameterError) as e:
            ragged.build_static_reader(resolution="PT1H")
        assert "build the reader over a window" in str(e.value)
        assert "filter to one grid" in str(e.value)

    def test_grids_read_as_dates_and_durations(self, ragged):
        with pytest.raises(InvalidParameterError) as e:
            ragged.build_static_reader(resolution="PT1H")
        msg = str(e.value)
        assert "2024-01-01T00:00:00Z" in msg and "PT1H" in msg
        assert "TimeDelta" not in msg


class TestWindowedSweep:
    def test_columns_read_at_offsets_of_their_own(self, ragged):
        reader = ragged.build_static_reader(resolution="PT1H", window_start=t(7))
        ragged.static_read(reader, t(7))
        # 'load' is at its row 7, 'active_power' at its row 0 -- both hour 7.
        assert list(reader.group_values(0)) == [7.0, 7.0]
        ragged.static_read(reader, t(23))
        assert list(reader.group_values(0)) == [23.0, 23.0]

    def test_the_grid_reports_the_window(self, ragged):
        grid = ragged.build_static_reader(
            resolution="PT1H", window_start=t(7), window_length=10
        ).grid()
        assert grid["initial_timestamp"].startswith("2024-01-01T07:00:00")
        assert grid["resolution"] == "PT1H"
        assert grid["length"] == 10

    def test_an_omitted_length_runs_as_far_as_every_column_reaches(self, ragged):
        reader = ragged.build_static_reader(resolution="PT1H", window_start=t(7))
        # 'load' has 17 hours left from 07:00; the longer series does not extend it.
        assert reader.grid()["length"] == 17
        assert reader.timestamps()[-1] == t(23)

    def test_reading_past_the_window_is_an_error(self, ragged):
        reader = ragged.build_static_reader(
            resolution="PT1H", window_start=t(7), window_length=3
        )
        ragged.static_read(reader, t(9))
        with pytest.raises(InvalidParameterError):
            ragged.static_read(reader, t(10))

    def test_every_column_is_still_present(self, ragged):
        reader = ragged.build_static_reader(resolution="PT1H", window_start=t(7))
        assert len(reader.groups()[0]["ids"]) == 2

    def test_a_window_on_a_uniform_store_is_a_slice(self):
        store = Store.create(in_memory=True)
        add(store, 1, "a", 0, 24)
        add(store, 2, "b", 0, 24)
        reader = store.build_static_reader(
            resolution="PT1H", window_start=t(6), window_length=3
        )
        assert reader.timestamps() == [t(6), t(7), t(8)]
        store.static_read(reader, t(8))
        assert list(reader.group_values(0)) == [8.0, 8.0]


class TestRefusals:
    def test_a_column_that_does_not_cover_the_window_is_named(self, ragged):
        with pytest.raises(InvalidParameterError) as e:
            ragged.build_static_reader(
                resolution="PT1H", window_start=t(7), window_length=8784
            )
        msg = str(e.value)
        assert "'load'" in msg and "owner 7" in msg
        assert "does not cover the reader window" in msg

    def test_an_anchor_before_a_columns_start_is_refused(self, ragged):
        with pytest.raises(InvalidParameterError) as e:
            ragged.build_static_reader(
                resolution="PT1H", window_start=t(3), window_length=4
            )
        assert "not on the grid" in str(e.value)

    def test_an_anchor_between_steps_is_not_floored(self, ragged):
        half_past = t(9) + timedelta(minutes=30)
        with pytest.raises(InvalidParameterError) as e:
            ragged.build_static_reader(
                resolution="PT1H", window_start=half_past, window_length=2
            )
        assert "not on the grid" in str(e.value)

    def test_a_window_length_with_no_anchor(self, ragged):
        with pytest.raises(InvalidParameterError) as e:
            ragged.build_static_reader(resolution="PT1H", window_length=4)
        assert "needs a start" in str(e.value)

    def test_an_empty_window(self, ragged):
        with pytest.raises(InvalidParameterError) as e:
            ragged.build_static_reader(
                resolution="PT1H", window_start=t(7), window_length=0
            )
        assert "is empty" in str(e.value)

    def test_the_irregular_types_take_no_window(self):
        store = Store.create(in_memory=True)
        add(store, 1, "a", 0, 24)
        with pytest.raises(InvalidParameterError) as e:
            store.build_static_reader(
                time_series_type="NonSequentialTimeSeries", window_start=t(0)
            )
        assert "takes no window" in str(e.value)

    def test_a_monthly_grid_is_refused_where_re_anchoring_moves_the_dates(self):
        # Jan-31 monthly is Jan-31, Feb-29, Mar-31; re-anchored at its own Feb-29
        # it would read Feb-29, Mar-29, Apr-29 -- right values, wrong dates.
        store = Store.create(in_memory=True)
        jan31 = datetime(2024, 1, 31, tzinfo=UTC)
        store.add_time_series(
            owner_id=1,
            owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=SingleTimeSeries(
                jan31, "P1M", np.arange(3.0), "monthly"
            ),
        )
        with pytest.raises(InvalidParameterError) as e:
            store.build_static_reader(
                resolution="P1M",
                window_start=datetime(2024, 2, 29, tzinfo=UTC),
                window_length=2,
            )
        assert "cannot be re-anchored" in str(e.value)
        # The series' own start is fine: nothing was clamped away.
        assert (
            store.build_static_reader(
                resolution="P1M", window_start=jan31, window_length=3
            ).grid()["length"]
            == 3
        )


class TestSpelling:
    def test_the_anchor_must_be_spelled_like_the_series(self):
        store = Store.create(in_memory=True)
        add(store, 1, "a", 0, 24)  # aware -> zoned
        naive = datetime(2024, 1, 1, 6)
        with pytest.raises(InvalidParameterError) as e:
            store.build_static_reader(
                resolution="PT1H", window_start=naive, window_length=2
            )
        assert "carry no zone" in str(e.value)
        assert "'a'" in str(e.value)

    def test_a_zoneless_store_takes_a_naive_anchor(self):
        store = Store.create(in_memory=True)
        store.add_time_series(
            owner_id=1,
            owner_type="Generator",
            owner_category=OwnerCategory.Component,
            time_series=SingleTimeSeries(
                datetime(2024, 1, 1), HOUR, np.arange(24.0), "wall"
            ),
        )
        reader = store.build_static_reader(
            resolution="PT1H", window_start=datetime(2024, 1, 1, 6), window_length=2
        )
        assert reader.grid()["time_reference"] == "zoneless"
        assert reader.grid()["length"] == 2
