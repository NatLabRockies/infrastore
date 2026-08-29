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

    def test_a_result_hashes_stably(self):
        # Regression: ``__hash__`` once seeded a fresh hasher per call, so the
        # same object hashed differently each time and never found itself in
        # a set or dict.
        store = Store.create(in_memory=True)
        added = _add(store, "load")
        assert hash(added) == hash(added)
        assert added in {added}
        assert {added: "x"}[added] == "x"

    def test_an_add_never_takes_an_id(self):
        """The catalog assigns; no add surface accepts an id.

        The one writer that files rows under ids a caller supplies is the
        OpenAPI row import, which replays a document that already recorded
        them. Everything else lets the catalog assign, so an id a caller is
        holding cannot be pushed back in through an add.
        """
        store = Store.create(in_memory=True)
        with pytest.raises(TypeError):
            _add(store, "load", id=500)

        added = _add(store, "load")
        store.remove_time_series(added.key)
        # The id is retired rather than free: the next add gets a new one.
        assert _add(store, "second", owner=2).id == added.id + 1

    def test_an_identity_collision_still_says_so(self):
        store = Store.create(in_memory=True)
        _add(store, "load")
        with pytest.raises(TimeSeriesError) as dup:
            _add(store, "load")
        assert "already in use" not in str(dup.value)

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

    def test_remove_by_ids_is_all_or_nothing(self):
        store = Store.create(in_memory=True)
        ids = [_add(store, name).id for name in ("a", "b", "c")]

        # One dangling id fails the batch and leaves every row in place: a
        # stale reference says the caller's model disagrees with the store.
        with pytest.raises(NotFoundError):
            store.remove_by_ids([ids[0], 9999])
        assert all(store.association_exists(i) for i in ids)

        # The rest go together, and a repeated id is removed — and counted —
        # once.
        assert store.remove_by_ids([ids[0], ids[1], ids[0]]) == 2
        assert not store.association_exists(ids[0])
        assert not store.association_exists(ids[1])
        assert store.association_exists(ids[2])
        assert [k.name for k in store.list_keys()] == ["c"]

        assert store.remove_by_ids([]) == 0

    def test_remove_by_ids_reclaims_only_the_last_reference(self):
        """The array behind a removed row survives while anything else uses it.

        Removing by reference goes through the same refcount as removing by
        key; the two owners here share one content-addressed array.
        """
        store = Store.create(in_memory=True)
        first = _add(store, "load", owner=1).id
        second = _add(store, "load", owner=2).id
        assert store.num_distinct_arrays() == 1

        store.remove_by_ids([first])
        assert store.num_distinct_arrays() == 1
        store.remove_by_ids([second])
        assert store.num_distinct_arrays() == 0



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

    def test_an_attachments_id_is_assigned_not_carried(self):
        """A row listed from one store, attached to another, is filed afresh.

        ``SupplementalAttributeAssociation`` carries an ``id`` because a
        listing populates one, but the constructor takes none and an add
        ignores it: this catalog's wire form has no id, so there is never a
        document reference to preserve.
        """
        source = Store.create(in_memory=True)
        source.add_supplemental_attribute_associations(
            [self._attach(1, 100), self._attach(2, 100)]
        )
        (_, second) = source.list_supplemental_attribute_associations()
        assert second.id == 2

        target = Store.create(in_memory=True)
        assert target.add_supplemental_attribute_association(second) == 1

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
