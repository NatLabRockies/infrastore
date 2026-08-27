"""The catalog association id, as Python sees it.

A write reports the id its row was filed under; a read resolves an id back to
the row. What makes the id worth storing in a caller's own model is that it is
never reissued once its row is deleted — a reference can go stale, but it can
never quietly come to mean a different series.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import (
    AddedTimeSeries,
    InvalidParameterError,
    NotFoundError,
    OwnerCategory,
    SingleTimeSeries,
    Store,
    SupplementalAttributeAssociation,
    TimeSeriesError,
)


def _t0() -> datetime:
    return datetime(2024, 1, 1, tzinfo=timezone.utc)


def _sts(name: str, base: float = 0.0, length: int = 4) -> SingleTimeSeries:
    data = np.arange(length, dtype=np.float64) + base
    return SingleTimeSeries(_t0(), timedelta(hours=1), data, name)


def _add(
    store: Store, name: str, *, owner: int = 1, base: float = 0.0, **kwargs
) -> AddedTimeSeries:
    return store.add_time_series(
        owner, "Generator", OwnerCategory.Component, _sts(name, base), **kwargs
    )


class TestWriting:
    def test_a_write_reports_the_id_it_used(self):
        store = Store.create(in_memory=True)
        added = _add(store, "load")
        assert isinstance(added, AddedTimeSeries)
        assert added.id == 1
        assert added.key.name == "load"
        assert store.get_metadata(added.key)["id"] == added.id

    def test_bulk_reports_one_id_per_item_in_order(self):
        store = Store.create(in_memory=True)
        items = [
            {
                "owner_id": i,
                "owner_type": "Generator",
                "owner_category": OwnerCategory.Component,
                "time_series": _sts("load", base=float(i)),
            }
            for i in range(3)
        ]
        added = store.add_time_series_bulk(items)
        assert [a.id for a in added] == [1, 2, 3]
        assert [a.key.owner_id for a in added] == [0, 1, 2]

    def test_an_explicit_id_is_honored_and_ratchets_the_counter(self):
        store = Store.create(in_memory=True)
        assert _add(store, "imported", id=500).id == 500
        # The next assigned id starts past the explicit one rather than
        # colliding with it.
        assert _add(store, "assigned").id == 501

    def test_a_taken_id_is_a_distinct_error_from_a_duplicate_series(self):
        store = Store.create(in_memory=True)
        _add(store, "load", id=7)

        with pytest.raises(TimeSeriesError) as taken:
            _add(store, "other", owner=2, id=7)
        assert "already in use" in str(taken.value)

        # The identity collision keeps saying what it always said.
        with pytest.raises(TimeSeriesError) as dup:
            _add(store, "load")
        assert "already in use" not in str(dup.value)

    def test_a_batch_may_not_mix_supplied_and_assigned_ids(self):
        store = Store.create(in_memory=True)
        base = {
            "owner_id": 1,
            "owner_type": "Generator",
            "owner_category": OwnerCategory.Component,
        }
        with pytest.raises(InvalidParameterError):
            store.add_time_series_bulk(
                [
                    {**base, "time_series": _sts("a"), "id": 10},
                    {**base, "time_series": _sts("b")},
                ]
            )

    def test_id_is_not_a_usable_feature_name(self):
        store = Store.create(in_memory=True)
        with pytest.raises(InvalidParameterError):
            _add(store, "load", features={"id": 3})


class TestReading:
    def test_an_id_resolves_to_its_row_or_to_nothing(self):
        store = Store.create(in_memory=True)
        added = _add(store, "load")

        meta = store.get_metadata_by_id(added.id)
        assert meta is not None
        assert meta["name"] == "load"
        assert meta["id"] == added.id
        assert store.association_exists(added.id)

        # A miss is an answer, not an exception: a caller validating the
        # references in its model is asking whether one still resolves.
        assert store.get_metadata_by_id(9999) is None
        assert not store.association_exists(9999)

    def test_a_removed_rows_id_stops_resolving_and_is_not_reused(self):
        store = Store.create(in_memory=True)
        added = _add(store, "load")
        store.remove_time_series(added.key)
        assert not store.association_exists(added.id)

        replacement = _add(store, "load")
        assert replacement.id != added.id
        assert not store.association_exists(added.id)

    def test_read_by_ids_follows_the_order_it_was_given(self):
        store = Store.create(in_memory=True)
        ids = [_add(store, name, base=base).id for name, base in
               [("a", 1.0), ("b", 10.0), ("c", 100.0)]]

        got = store.read_by_ids([ids[2], ids[0], ids[2]])
        assert [float(np.asarray(g.data)[0]) for g in got] == [100.0, 1.0, 100.0]

        with pytest.raises(NotFoundError):
            store.read_by_ids([ids[0], 9999])
        assert store.read_by_ids([]) == []


class TestDerivedViews:
    def _long(self, store: Store) -> AddedTimeSeries:
        data = np.arange(24, dtype=np.float64)
        series = SingleTimeSeries(_t0(), timedelta(hours=1), data, "load")
        return store.add_time_series(
            1, "Generator", OwnerCategory.Component, series
        )

    def test_a_view_adds_a_row_and_no_array(self):
        store = Store.create(in_memory=True)
        source = self._long(store)
        before = store.num_distinct_arrays()

        view = store.add_derived_view(
            source.key, timedelta(hours=6), timedelta(hours=6)
        )
        assert store.num_distinct_arrays() == before
        assert view.id != source.id
        meta = store.get_metadata_by_id(view.id)
        assert meta["time_series_type"] == "DeterministicSingleTimeSeries"
        assert meta["data_hash"] == store.get_metadata_by_id(source.id)["data_hash"]

    def test_a_view_can_be_filed_under_a_given_id(self):
        store = Store.create(in_memory=True)
        source = self._long(store)
        view = store.add_derived_view(
            source.key, timedelta(hours=6), timedelta(hours=6), id=4242
        )
        assert view.id == 4242


class TestAssociations:
    def _attach(self, component_id: int, attribute_id: int):
        return SupplementalAttributeAssociation(
            component_id, "Generator", attribute_id, "GeographicInfo"
        )

    def test_attaching_reports_its_id(self):
        store = Store.create(in_memory=True)
        assert store.add_supplemental_attribute_association(self._attach(1, 100)) == 1
        assert store.add_supplemental_attribute_associations(
            [self._attach(2, 100), self._attach(3, 100)]
        ) == [2, 3]

        rows = store.list_supplemental_attribute_associations()
        assert [r.id for r in rows] == [1, 2, 3]

    def test_an_id_is_outside_equality_and_hashing(self):
        """A row read back must equal the value that wrote it.

        Identity is the endpoint pair, so folding the id into equality would
        make a stored row unequal to its own source — and break ``__hash__``'s
        contract with ``__eq__`` for every set these land in.
        """
        store = Store.create(in_memory=True)
        fresh = self._attach(1, 100)
        store.add_supplemental_attribute_association(fresh)
        (stored,) = store.list_supplemental_attribute_associations()

        assert stored.id == 1
        assert fresh.id is None
        assert stored == fresh
        assert hash(stored) == hash(fresh)
        assert stored in {fresh}


def test_ids_survive_a_persist_and_reopen(tmp_path):
    """The path a system serialization actually takes: the catalog is copied,
    not rewritten, so every reference stored against it stays valid."""
    path = tmp_path / "store.h5"
    store = Store.create(path=str(path), in_memory=False)
    expected = {name: _add(store, name).id for name in ("first", "second", "third")}
    store.flush()
    del store

    reopened = Store.open(str(path), read_only=True)
    for name, id_ in expected.items():
        assert reopened.get_metadata_by_id(id_)["name"] == name
