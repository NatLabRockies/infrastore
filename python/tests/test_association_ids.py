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
    InvalidParameterError,
    NotFoundError,
    OwnerCategory,
    OwnerMismatchError,
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
) -> int:
    return store.add_time_series(
        owner, "Generator", OwnerCategory.Component, _sts(name, base), **kwargs
    )


class TestWriting:
    def test_a_write_reports_the_id_it_used(self):
        store = Store.create(in_memory=True)
        added = _add(store, "load")
        assert isinstance(added, int)
        assert added == 1
        assert store.get_metadata_by_id(added)['name'] == "load"
        assert store.get_metadata_by_id(added)["id"] == added

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
        assert [a for a in added] == [1, 2, 3]
        assert [store.get_metadata_by_id(a)['owner_id'] for a in added] == [0, 1, 2]

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
        store.remove_by_ids([added])
        # The id is retired rather than free: the next add gets a new one.
        assert _add(store, "second", owner=2) == added + 1

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

        meta = store.get_metadata_by_id(added)
        assert meta is not None
        assert meta["name"] == "load"
        assert meta["id"] == added
        assert store.association_exists(added)

        # A miss is an answer, not an exception: a caller validating the
        # references in its model is asking whether one still resolves.
        assert store.get_metadata_by_id(9999) is None
        assert not store.association_exists(9999)

    def test_a_removed_rows_id_stops_resolving_and_is_not_reused(self):
        store = Store.create(in_memory=True)
        added = _add(store, "load")
        store.remove_by_ids([added])
        assert not store.association_exists(added)

        replacement = _add(store, "load")
        assert replacement != added
        assert not store.association_exists(added)

    def test_read_by_ids_follows_the_order_it_was_given(self):
        store = Store.create(in_memory=True)
        ids = [_add(store, name, base=base) for name, base in
               [("a", 1.0), ("b", 10.0), ("c", 100.0)]]

        got = store.read_by_ids([ids[2], ids[0], ids[2]])
        assert [float(np.asarray(g.data)[0]) for g in got] == [100.0, 1.0, 100.0]

        with pytest.raises(NotFoundError):
            store.read_by_ids([ids[0], 9999])
        assert store.read_by_ids([]) == []

    def test_read_by_id_resolves_a_window_in_one_call(self):
        store = Store.create(in_memory=True)
        # 24 hourly points, values 0.0..23.0.
        data = np.arange(24, dtype=np.float64)
        added = store.add_time_series(
            1,
            "Generator",
            OwnerCategory.Component,
            SingleTimeSeries(_t0(), timedelta(hours=1), data, "load"),
        )

        # No keywords is the whole series, the answer read_by_ids gives.
        assert np.asarray(store.read_by_id(added).data).tolist() == data.tolist()

        # A window names exactly the steps asked for and moves the returned
        # series' initial timestamp onto the one it starts at -- and it does so
        # without a metadata read first.
        sliced = store.read_by_id(
            added, start_time=_t0() + timedelta(hours=4), len=3
        )
        assert np.asarray(sliced.data).tolist() == [4.0, 5.0, 6.0]
        assert sliced.initial_timestamp == _t0() + timedelta(hours=4)
        assert np.asarray(store.read_by_id(added, len=2).data).tolist() == [0.0, 1.0]

        # A window is checked where a time range is clipped: the same over-long
        # request yields the two steps that exist through read_by_ids_range, and
        # is a mistake through read_by_id.
        (clamped,) = store.read_by_ids_range(
            [added],
            (
                _t0() + timedelta(hours=22),
                _t0() + timedelta(hours=52),
            ),
        )
        assert len(np.asarray(clamped.data)) == 2
        with pytest.raises(InvalidParameterError):
            store.read_by_id(added, start_time=_t0() + timedelta(hours=22), len=30)

        # A start between two steps is off the grid, not rounded onto it.
        with pytest.raises(InvalidParameterError):
            store.read_by_id(added, start_time=_t0() + timedelta(minutes=30))

        # count selects forecast windows; on a static series it is refused
        # rather than silently dropped.
        with pytest.raises(InvalidParameterError):
            store.read_by_id(added, count=2)

        # A read is already committed to acting on the reference.
        with pytest.raises(NotFoundError):
            store.read_by_id(9999)

    def test_remove_by_ids_is_all_or_nothing(self):
        store = Store.create(in_memory=True)
        ids = [_add(store, name) for name in ("a", "b", "c")]

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
        assert [r["name"] for r in store.list_metadata()] == ["c"]

        assert store.remove_by_ids([]) == 0

    def test_remove_by_ids_reclaims_only_the_last_reference(self):
        """The array behind a removed row survives while anything else uses it.

        Removing by reference goes through the same refcount as removing by
        key; the two owners here share one content-addressed array.
        """
        store = Store.create(in_memory=True)
        first = _add(store, "load", owner=1)
        second = _add(store, "load", owner=2)
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
    expected = {name: _add(store, name) for name in ("first", "second", "third")}
    store.flush()
    del store

    reopened = Store.open(str(path), read_only=True)
    for name, id_ in expected.items():
        assert reopened.get_metadata_by_id(id_)["name"] == name


class TestOwnerGuardedIdAddressing:
    """``owner_id`` / ``owner_category`` on the id-addressed read and removal.

    For the caller whose model addresses a series by id but reasons about it as
    one component's — "retire this component's series" — where the id alone is
    the wrong request. The check cannot be assembled out of the unguarded parts:
    an id survives ``replace_owner``, so a ``get_metadata_by_id`` that confirms
    the owner and a ``remove_by_ids`` that then deletes leave a window in which
    the row moves and the removal retires the *new* owner's series.
    """

    def test_read_serves_the_owner_that_holds_the_row(self):
        store = Store.create(in_memory=True)
        id_ = _add(store, "load")
        data = store.read_by_id(
            id_, owner_id=1, owner_category=OwnerCategory.Component
        )
        assert list(data.data) == [0.0, 1.0, 2.0, 3.0]

    def test_read_refuses_every_other_owner(self):
        store = Store.create(in_memory=True)
        id_ = _add(store, "load")
        with pytest.raises(OwnerMismatchError):
            store.read_by_id(id_, owner_id=2, owner_category=OwnerCategory.Component)
        # The category is half the owner: the same integer in the other category
        # is a different owner, since a component and a supplemental attribute
        # can share an id.
        with pytest.raises(OwnerMismatchError):
            store.read_by_id(
                id_, owner_id=1, owner_category=OwnerCategory.SupplementalAttribute
            )

    def test_a_dangling_id_is_not_found_rather_than_mismatched(self):
        """Nothing owns it, so there is no belief about ownership to be stale."""
        store = Store.create(in_memory=True)
        with pytest.raises(NotFoundError):
            store.read_by_id(9999, owner_id=1, owner_category=OwnerCategory.Component)

    def test_a_guarded_removal_naming_the_wrong_owner_deletes_nothing(self):
        store = Store.create(in_memory=True)
        id_ = _add(store, "load")
        with pytest.raises(OwnerMismatchError):
            store.remove_by_ids(
                [id_], owner_id=2, owner_category=OwnerCategory.Component
            )
        assert store.association_exists(id_)

    def test_the_guard_closes_the_reassignment_race(self):
        store = Store.create(in_memory=True)
        id_ = _add(store, "load")
        store.replace_owner(1, 3, OwnerCategory.Component)

        # An unguarded removal here would retire the *new* owner's series.
        with pytest.raises(OwnerMismatchError):
            store.remove_by_ids(
                [id_], owner_id=1, owner_category=OwnerCategory.Component
            )
        assert store.association_exists(id_)

        assert (
            store.remove_by_ids(
                [id_], owner_id=3, owner_category=OwnerCategory.Component
            )
            == 1
        )
        assert not store.association_exists(id_)

    def test_a_mismatch_late_in_a_batch_rolls_the_whole_batch_back(self):
        store = Store.create(in_memory=True)
        mine = _add(store, "mine", owner=1)
        theirs = _add(store, "theirs", owner=2)
        with pytest.raises(OwnerMismatchError):
            store.remove_by_ids(
                [mine, theirs], owner_id=1, owner_category=OwnerCategory.Component
            )
        assert store.association_exists(mine)
        assert store.association_exists(theirs)

    @pytest.mark.parametrize(
        "kwargs",
        [
            {"owner_id": 1},
            {"owner_category": OwnerCategory.Component},
        ],
    )
    def test_half_an_owner_is_refused_rather_than_ignored(self, kwargs):
        """Silently checking less than the caller asked for is the one answer a
        guard must not give."""
        store = Store.create(in_memory=True)
        id_ = _add(store, "load")
        with pytest.raises(InvalidParameterError):
            store.read_by_id(id_, **kwargs)
        with pytest.raises(InvalidParameterError):
            store.remove_by_ids([id_], **kwargs)

    def test_the_unguarded_forms_are_unchanged(self):
        store = Store.create(in_memory=True)
        id_ = _add(store, "load")
        assert list(store.read_by_id(id_).data) == [0.0, 1.0, 2.0, 3.0]
        assert store.remove_by_ids([id_]) == 1
