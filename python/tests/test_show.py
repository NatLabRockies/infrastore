"""`Store.show()`: the human-readable summary of what a store holds.

The numbers it prints all come from methods with their own tests
(`counts_by_type`, `time_series_counts_detailed`, `num_distinct_arrays`, the two
association counts), so these tests are about the *rendering*: that every count
reaches the page, that the type breakdown is grouped static-then-forecast rather
than in the catalog's numeric code order, and that `file=` is honoured.
"""

import io
from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

import infrastore
from infrastore import (
    Deterministic,
    OwnerCategory,
    ParentChildAssociation,
    PersistentTimeSeries,
    SingleTimeSeries,
    Store,
    SupplementalAttributeAssociation,
)

START = datetime(2024, 1, 1, tzinfo=timezone.utc)


def render(store):
    """`show()`'s text, captured rather than printed."""
    buf = io.StringIO()
    store.show(file=buf)
    return buf.getvalue()


def add_static(store, owner_id, values, name="load"):
    return store.add_time_series(
        owner_id=owner_id,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=SingleTimeSeries(START, timedelta(hours=1), values, name),
    )


def add_forecast(store, owner_id, name="fc"):
    return store.add_time_series(
        owner_id=owner_id,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=Deterministic(
            START,
            timedelta(hours=1),
            timedelta(hours=4),
            timedelta(hours=1),
            2,
            np.arange(8.0).reshape(4, 2),
            name,
        ),
    )


def add_persistent(store, owner_id, name="status"):
    return store.add_time_series(
        owner_id=owner_id,
        owner_type="Bus",
        owner_category=OwnerCategory.Component,
        time_series=PersistentTimeSeries(
            [START, START + timedelta(hours=5)],
            np.array([1.0, 2.0]),
            name,
        ),
    )


class TestEmptyStore:
    def test_says_none_rather_than_zero(self):
        # A bare "0" under a heading would read as one type with no rows.
        text = render(Store.create(in_memory=True))
        assert "Time series: none" in text
        assert "association" not in text

    def test_still_reports_the_other_sections(self):
        text = render(Store.create(in_memory=True))
        assert "Owners with time series: 0 components, 0 supplemental attributes" in text
        assert "Supplemental attribute attachments: 0" in text
        assert "Parent/child edges: 0" in text


class TestHeader:
    def test_in_memory_store(self):
        assert render(Store.create(in_memory=True)).startswith(
            "Store: in-memory (read-write)\n"
        )

    def test_path_and_read_only(self, tmp_path):
        path = tmp_path / "system.h5"
        Store.create(path=str(path)).close()
        with Store.open(str(path), read_only=True) as store:
            assert render(store).startswith(f"Store: {path} (read-only)\n")


class TestCountsByType:
    def test_one_line_per_present_type_with_its_count(self):
        store = Store.create(in_memory=True)
        for i in range(3):
            add_static(store, i, np.arange(24.0) + i)
        add_forecast(store, 9)
        lines = render(store).splitlines()
        assert "Time series: 4 associations over 4 distinct arrays" in lines
        assert [line.split() for line in lines if line.startswith("  ")] == [
            ["SingleTimeSeries", "3"],
            ["Deterministic", "1"],
        ]

    def test_absent_types_are_left_out(self):
        store = Store.create(in_memory=True)
        add_static(store, 1, np.arange(24.0))
        assert "Probabilistic" not in render(store)

    def test_static_types_come_before_forecasts(self):
        # `counts_by_type` orders by the numeric type code, which puts
        # `PersistentTimeSeries` *after* the forecasts because it was appended
        # to a list that is an on-disk contract. `show` regroups.
        store = Store.create(in_memory=True)
        add_forecast(store, 1)
        add_persistent(store, 2)
        add_static(store, 3, np.arange(24.0))
        text = render(store)
        assert (
            text.index("SingleTimeSeries")
            < text.index("PersistentTimeSeries")
            < text.index("Deterministic")
        )

    def test_shared_arrays_count_once(self):
        # Two owners, identical values: two associations, one stored array.
        store = Store.create(in_memory=True)
        add_static(store, 1, np.arange(24.0))
        add_static(store, 2, np.arange(24.0))
        assert "Time series: 2 associations over 1 distinct array" in render(store)

    def test_counts_are_right_aligned_under_padded_names(self):
        store = Store.create(in_memory=True)
        for i in range(10):
            add_static(store, i, np.arange(24.0) + i)
        add_persistent(store, 99)
        lines = [line for line in render(store).splitlines() if line.startswith("  ")]
        assert lines == [
            "  SingleTimeSeries      10",
            "  PersistentTimeSeries   1",
        ]


class TestOwnersAndCatalogs:
    def test_owner_categories_are_counted_separately(self):
        store = Store.create(in_memory=True)
        add_static(store, 1, np.arange(24.0))
        add_static(store, 2, np.arange(24.0) + 1)
        store.add_time_series(
            owner_id=50,
            owner_type="GeographicInfo",
            owner_category=OwnerCategory.SupplementalAttribute,
            time_series=SingleTimeSeries(
                START, timedelta(hours=1), np.arange(24.0) + 2, "load"
            ),
        )
        assert (
            "Owners with time series: 2 components, 1 supplemental attribute"
            in render(store)
        )

    def test_association_catalogs(self):
        store = Store.create(in_memory=True)
        store.add_supplemental_attribute_associations(
            [SupplementalAttributeAssociation(1, "Generator", 100, "GeographicInfo")]
        )
        store.add_parent_child_associations(
            [
                ParentChildAssociation(1, "Generator", 10, "Bus"),
                ParentChildAssociation(2, "Generator", 10, "Bus"),
            ]
        )
        text = render(store)
        assert "Supplemental attribute attachments: 1" in text
        assert "Parent/child edges: 2" in text


class TestPlumbing:
    def test_defaults_to_stdout(self, capsys):
        store = Store.create(in_memory=True)
        add_static(store, 1, np.arange(24.0))
        store.show()
        assert capsys.readouterr().out == render(store)

    def test_file_is_keyword_only(self):
        store = Store.create(in_memory=True)
        with pytest.raises(TypeError):
            store.show(io.StringIO())

    def test_a_closed_store_raises(self):
        store = Store.create(in_memory=True)
        store.close()
        with pytest.raises(infrastore.TimeSeriesError):
            store.show()

    def test_returns_none(self):
        assert Store.create(in_memory=True).show(file=io.StringIO()) is None
