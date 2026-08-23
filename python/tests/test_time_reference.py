"""How a series' timestamps were *spelled*, from Python.

The store records instants; `time_reference` records what those instants were
written as. Python's own asymmetry is what makes this load-bearing rather than
cosmetic::

    datetime(2024, 1, 1) == datetime(2024, 1, 1, tzinfo=timezone.utc)   # False
    datetime(2024, 1, 1) <  datetime(2024, 1, 1, tzinfo=timezone.utc)   # TypeError

So accepting a naive datetime is only defensible because the read path returns
one: a store that took a naive value and handed back an aware one would be worse
than the refusal it replaced.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo

import numpy as np
import pytest

from infrastore import (
    InvalidParameterError,
    NonSequentialTimeSeries,
    OwnerCategory,
    SingleTimeSeries,
    Store,
)

HOUR = timedelta(hours=1)
DENVER = ZoneInfo("America/Denver")


def series(initial, name="load", length=8):
    return SingleTimeSeries(
        initial, HOUR, np.arange(length, dtype=np.float64), name
    )


def add(store, owner, ts, **kwargs):
    return store.add_time_series(
        owner_id=owner,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=ts,
        **kwargs,
    )


class TestInference:
    """Each input type records the spelling it names, inferred at the boundary.

    The core cannot infer: by the time a timestamp reaches it everything is a
    UTC instant and the intent is gone.
    """

    def test_naive_is_a_wall_clock(self):
        s = series(datetime(2024, 1, 1))
        assert s.time_reference == "zoneless"
        assert s.initial_timestamp == datetime(2024, 1, 1)

    def test_utc_is_utc(self):
        s = series(datetime(2024, 1, 1, tzinfo=timezone.utc))
        assert s.time_reference == "utc"

    def test_a_fixed_offset_is_kept_as_an_offset(self):
        s = series(datetime(2024, 1, 1, tzinfo=timezone(timedelta(hours=-7))))
        assert s.time_reference == "-07:00"

    def test_a_named_zone_is_kept_by_name(self):
        s = series(datetime(2024, 1, 1, tzinfo=DENVER))
        assert s.time_reference == "America/Denver"

    def test_zoneinfo_utc_is_the_zone_not_the_literal(self):
        # The two render identically forever; the distinction only shows up in
        # what the catalog reports back, which is the point of recording a
        # spelling at all. The `key` case is tested before the offset case for
        # exactly this.
        s = series(datetime(2024, 1, 1, tzinfo=ZoneInfo("UTC")))
        assert s.time_reference == "UTC"
        assert series(datetime(2024, 1, 1, tzinfo=timezone.utc)).time_reference == "utc"

    def test_a_naive_datetime_is_never_read_through_the_machines_zone(self):
        # `astimezone` on a naive datetime assumes *system local time*, so the
        # same script would write a different instant on a laptop in Denver than
        # in CI on UTC. The fields are read as they stand instead.
        store = Store.create(in_memory=True)
        key = add(store, 1, series(datetime(2024, 6, 1, 12)))
        meta = store.get_metadata(key)
        assert meta["initial_timestamp"] == "2024-06-01T12:00:00"

    def test_a_timestamp_vector_must_agree_on_one_spelling(self):
        stamps = [
            datetime(2024, 1, 1, tzinfo=timezone.utc),
            datetime(2024, 1, 1, 3),
        ]
        with pytest.raises(InvalidParameterError, match="disagree"):
            NonSequentialTimeSeries(stamps, np.arange(2, dtype=np.float64), "events")


class TestRoundTrip:
    """A spelling the store accepts is a spelling it hands back."""

    @pytest.mark.parametrize(
        "initial, spelling",
        [
            (datetime(2024, 1, 1), "zoneless"),
            (datetime(2024, 1, 1, tzinfo=timezone.utc), "utc"),
            (datetime(2024, 1, 1, tzinfo=timezone(timedelta(hours=-7))), "-07:00"),
            (datetime(2024, 1, 1, tzinfo=DENVER), "America/Denver"),
        ],
    )
    def test_what_comes_out_equals_what_went_in(self, initial, spelling):
        store = Store.create(in_memory=True)
        key = add(store, 1, series(initial))
        got = store.get_time_series(key)
        assert got.time_reference == spelling
        assert got.initial_timestamp == initial
        assert store.get_metadata(key)["time_reference"] == spelling

    def test_the_fold_of_an_ambiguous_hour_survives(self):
        # Both wall clocks read 01:00 in Denver on the fall-back day. They are
        # two distinct instants, and the zone name plus the instant is enough to
        # reconstruct which one -- lossless, without a `fold` of our own.
        store = Store.create(in_memory=True)
        first = datetime(2020, 11, 1, 1, tzinfo=DENVER, fold=0)
        second = datetime(2020, 11, 1, 1, tzinfo=DENVER, fold=1)
        assert first.utcoffset() != second.utcoffset()
        for owner, written in ((1, first), (2, second)):
            got = store.get_time_series(add(store, owner, series(written)))
            assert got.initial_timestamp == written
            assert got.initial_timestamp.utcoffset() == written.utcoffset()

    def test_an_irregular_vector_keeps_its_spelling(self):
        store = Store.create(in_memory=True)
        stamps = [datetime(2024, 1, 1, h) for h in (0, 3, 7)]
        key = add(
            store,
            1,
            NonSequentialTimeSeries(stamps, np.arange(3, dtype=np.float64), "events"),
        )
        got = store.get_time_series(key)
        assert got.time_reference == "zoneless"
        assert got.timestamps == stamps

    def test_an_explicit_argument_overrides_the_inference(self):
        store = Store.create(in_memory=True)
        key = add(store, 1, series(datetime(2024, 1, 1)), time_reference="America/Denver")
        assert store.get_time_series(key).time_reference == "America/Denver"

    def test_an_unrecognized_zone_warns_but_stores(self):
        # Existence is audited, never gated: gating would refuse legitimate data
        # whenever IANA moves ahead of this interpreter's tzdata.
        store = Store.create(in_memory=True)
        with pytest.warns(UserWarning, match="tz database"):
            key = add(store, 1, series(datetime(2024, 1, 1)), time_reference="America/Dever")
        assert store.get_metadata(key)["time_reference"] == "America/Dever"


class TestQueryBounds:
    """A bound has to be spelled the way the series is; no coercion."""

    def setup_method(self):
        self.store = Store.create(in_memory=True)
        self.zoned = add(self.store, 1, series(datetime(2024, 1, 1, tzinfo=DENVER)))
        self.wall = add(self.store, 2, series(datetime(2024, 1, 1)))

    def test_an_aware_bound_need_not_match_the_series_offset(self):
        # Slicing is instant arithmetic, and any offset names the same instant.
        start = datetime(2024, 1, 1, 2, tzinfo=DENVER)
        got = self.store.get_time_series(
            self.zoned, time_range=(start.astimezone(timezone.utc), start + 2 * HOUR)
        )
        assert len(got.data) == 2

    def test_a_wall_clock_bound_against_instants_is_refused(self):
        with pytest.raises(InvalidParameterError, match="wall clock|no zone"):
            self.store.get_time_series(
                self.zoned,
                time_range=(datetime(2024, 1, 1, 2), datetime(2024, 1, 1, 4)),
            )

    def test_an_instant_bound_against_wall_clocks_is_refused(self):
        with pytest.raises(InvalidParameterError, match="zoneless"):
            self.store.get_time_series(
                self.wall,
                time_range=(
                    datetime(2024, 1, 1, 2, tzinfo=timezone.utc),
                    datetime(2024, 1, 1, 4, tzinfo=timezone.utc),
                ),
            )

    def test_both_ends_of_a_range_must_agree(self):
        with pytest.raises(InvalidParameterError, match="spelled differently"):
            self.store.get_time_series(
                self.zoned,
                time_range=(
                    datetime(2024, 1, 1, 2, tzinfo=timezone.utc),
                    datetime(2024, 1, 1, 4),
                ),
            )


class TestMixedSelections:
    """One bound, or one shared timestamp axis, cannot serve both groups."""

    def setup_method(self):
        self.store = Store.create(in_memory=True)
        self.zoned = add(
            self.store, 1, series(datetime(2024, 1, 1, tzinfo=timezone.utc))
        )
        self.wall = add(self.store, 2, series(datetime(2024, 1, 1)))

    def test_an_unranged_bulk_read_is_unaffected(self):
        # Without a bound there is nothing for the two groups to disagree about,
        # and each series carries its own spelling back.
        got = self.store.bulk_read([self.zoned, self.wall])
        assert [s.time_reference for s in got] == ["utc", "zoneless"]

    def test_a_ranged_bulk_read_over_a_mixed_selection_is_refused(self):
        with pytest.raises(InvalidParameterError, match="zoneless"):
            self.store.bulk_read(
                [self.zoned, self.wall],
                time_range=(
                    datetime(2024, 1, 1, tzinfo=timezone.utc),
                    datetime(2024, 1, 1, 4, tzinfo=timezone.utc),
                ),
            )

    def test_a_mixed_cohort_is_refused_at_reader_build_time(self):
        # At build time, where the error can name the series that disagree --
        # not at read time, where all it could say is that the bound is wrong.
        with pytest.raises(InvalidParameterError, match="zoneless"):
            self.store.build_static_reader(HOUR)

    def test_the_zoneless_filter_is_the_constructive_half(self):
        for zoneless, spelling in ((True, "zoneless"), (False, "utc")):
            reader = self.store.build_static_reader(HOUR, zoneless=zoneless)
            assert reader.grid()["time_reference"] == spelling
            assert len(reader.groups()[0]["keys"]) == 1
            assert len(self.store.list_time_series(zoneless=zoneless)) == 1

    def test_python_never_leaves_the_reference_unset(self):
        # Unset is reachable from the Rust core (and from a store written before
        # the column existed), and `zoneless=False` must return those rows --
        # that rule is asserted in the core's own suite, where a `None`
        # reference can actually be constructed.
        #
        # What is Python-specific, and what this pins, is that Python cannot
        # produce one: every datetime it accepts names a spelling, so no series
        # added from here ever lands in the store undeclared.
        store = Store.create(in_memory=True)
        for owner, initial in enumerate(
            (
                datetime(2024, 1, 1),
                datetime(2024, 1, 1, tzinfo=timezone.utc),
                datetime(2024, 1, 1, tzinfo=timezone(timedelta(hours=-7))),
                datetime(2024, 1, 1, tzinfo=DENVER),
            ),
            start=1,
        ):
            add(store, owner, series(initial))
        assert all(
            row["time_reference"] is not None for row in store.list_time_series()
        )


class TestReaderAxis:
    """A reader has one axis, so it has one spelling for it."""

    def test_a_cohort_that_agrees_reports_what_it_agrees_on(self):
        store = Store.create(in_memory=True)
        for owner in (1, 2):
            add(store, owner, series(datetime(2024, 1, 1, tzinfo=DENVER)))
        reader = store.build_static_reader(HOUR)
        assert reader.grid()["time_reference"] == "America/Denver"
        assert reader.timestamps()[0] == datetime(2024, 1, 1, tzinfo=DENVER)

    def test_mixed_zoned_spellings_report_the_shared_truth(self):
        # All three name instants, and an axis of instants is what a reader
        # materializes -- so this is fine, and the axis is spelled with what is
        # true of all of them.
        store = Store.create(in_memory=True)
        # The same instant, written three ways -- a reader needs one grid, and
        # a grid is instants.
        instant = datetime(2024, 1, 1, tzinfo=timezone.utc)
        add(store, 1, series(instant))
        add(store, 2, series(instant.astimezone(DENVER)))
        add(store, 3, series(instant.astimezone(timezone(timedelta(hours=5, minutes=30)))))
        reader = store.build_static_reader(HOUR)
        assert reader.grid()["time_reference"] == "utc"
        assert len(reader.groups()[0]["keys"]) == 3
        assert reader.timestamps()[0] == instant


class TestSubMinuteOffsets:
    """`datetime` carries an offset to the microsecond; the store records minutes.

    A sub-minute offset therefore cannot be stored faithfully, and the gap has
    to be refused rather than rounded: the instant would survive the round trip
    while the wall clock moved, which is the one failure this feature exists to
    prevent.
    """

    @pytest.mark.parametrize(
        "offset",
        [
            timedelta(seconds=60, microseconds=500000),
            timedelta(seconds=30),
            timedelta(hours=-7, microseconds=1),
            timedelta(seconds=-90),
        ],
    )
    def test_an_offset_that_is_not_whole_minutes_is_refused(self, offset):
        with pytest.raises(InvalidParameterError, match="whole number of minutes"):
            series(datetime(2024, 1, 1, tzinfo=timezone(offset)))

    def test_the_check_happens_before_the_narrowing_to_minutes(self):
        # 60.5s truncates to 60 -- a whole minute -- so a check applied after
        # the cast accepted it and recorded "+00:01", storing the instant
        # correctly and shifting the wall clock by 500 ms on the way back.
        tz = timezone(timedelta(seconds=60, microseconds=500000))
        with pytest.raises(InvalidParameterError):
            series(datetime(2024, 1, 1, tzinfo=tz))

    @pytest.mark.parametrize("minutes", [-1439, -420, 0, 330, 1439])
    def test_whole_minute_offsets_still_pass(self, minutes):
        s = series(datetime(2024, 1, 1, tzinfo=timezone(timedelta(minutes=minutes))))
        assert s.time_reference is not None


class TestPointReadSpelling:
    """A point read is a query bound too, and obeys the same rule as a range.

    `static_read`/`forecast_read` used to take only the instant, so a naive wall
    clock could query an instant-bearing reader (and an aware datetime a
    zoneless one) after being reinterpreted as UTC -- returning a *row* where
    the identical mismatch on a ranged read raises.
    """

    def test_a_naive_point_cannot_query_an_instant_bearing_axis(self):
        store = Store.create(in_memory=True)
        add(store, 1, series(datetime(2024, 1, 1, tzinfo=timezone.utc)))
        reader = store.build_static_reader(HOUR)
        with pytest.raises(InvalidParameterError, match="zoneless|no zone"):
            store.static_read(reader, datetime(2024, 1, 1, 1))

    def test_an_aware_point_cannot_query_a_wall_clock_axis(self):
        store = Store.create(in_memory=True)
        add(store, 1, series(datetime(2024, 1, 1)))
        reader = store.build_static_reader(HOUR)
        with pytest.raises(InvalidParameterError, match="zoneless|no zone"):
            store.static_read(reader, datetime(2024, 1, 1, 1, tzinfo=timezone.utc))

    def test_a_matched_spelling_still_reads(self):
        # Both directions, so the check cannot be passing by refusing everything.
        store = Store.create(in_memory=True)
        add(store, 1, series(datetime(2024, 1, 1, tzinfo=timezone.utc)))
        reader = store.build_static_reader(HOUR)
        store.static_read(reader, datetime(2024, 1, 1, 1, tzinfo=timezone.utc))
        assert reader.group_values(0)[0] == 1.0

        wall = Store.create(in_memory=True)
        add(wall, 1, series(datetime(2024, 1, 1)))
        wall_reader = wall.build_static_reader(HOUR)
        wall.static_read(wall_reader, datetime(2024, 1, 1, 1))
        assert wall_reader.group_values(0)[0] == 1.0

    def test_an_aware_point_need_not_match_the_axis_offset(self):
        # Slicing is instant arithmetic: any aware spelling names the same
        # instant, so only the zoned/zoneless category has to agree.
        store = Store.create(in_memory=True)
        add(store, 1, series(datetime(2024, 1, 1, tzinfo=timezone.utc)))
        reader = store.build_static_reader(HOUR)
        denver = datetime(2024, 1, 1, 1, tzinfo=timezone.utc).astimezone(DENVER)
        store.static_read(reader, denver)
        assert reader.group_values(0)[0] == 1.0

    def test_the_forecast_point_read_obeys_the_same_rule(self):
        store = Store.create(in_memory=True)
        add(store, 1, series(datetime(2024, 1, 1, tzinfo=timezone.utc), length=8))
        store.transform_single_time_series(timedelta(hours=2), HOUR)
        reader = store.build_forecast_reader("Deterministic", HOUR)
        with pytest.raises(InvalidParameterError, match="zoneless|no zone"):
            store.forecast_read(reader, datetime(2024, 1, 1, 1))
        store.forecast_read(reader, datetime(2024, 1, 1, 1, tzinfo=timezone.utc))
