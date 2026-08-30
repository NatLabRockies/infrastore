"""Cross-operation transactions through the Python binding.

The guarantee these pin down is the one a per-operation transaction cannot give:
several operations rolling back together, and — impossible outside a transaction
— a **removal** being undone. That works because the array store is
content-addressed and is made append-only for the transaction's duration, so
frees are deferred to the outermost commit and a rollback restores catalog rows
whose data is still present.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import (
    InvalidParameterError,
    OwnerCategory,
    ReadOnlyStoreError,
    SingleTimeSeries,
    Store,
)


def series(base: float) -> SingleTimeSeries:
    """A length-8 hourly series offset by ``base``, so equal bases share an array."""
    return SingleTimeSeries(
        initial_timestamp=datetime(2024, 1, 1, tzinfo=timezone.utc),
        resolution=timedelta(hours=1),
        data=np.arange(8, dtype=float) + base,
        name="load",
    )


def add(store: Store, owner_id: int, base: float):
    return store.add_time_series(
        owner_id=owner_id,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=series(base),
    )


def test_rollback_undoes_a_mixed_add_and_remove_span():
    store = Store.create(in_memory=True)
    k1 = add(store, 1, 0.0)

    with pytest.raises(RuntimeError, match="boom"):
        with store.transaction():
            add(store, 2, 100.0)
            store.remove_by_ids([k1])
            # Uncommitted work is visible inside the transaction, because reads
            # go through the same connection. No client-side overlay needed.
            assert len(store.list_metadata()) == 1
            assert store.in_transaction
            raise RuntimeError("boom")

    assert not store.in_transaction
    assert len(store.list_metadata()) == 1
    # Not just the catalog row: the array behind the removal survived.
    restored = store.read_by_id(k1)
    assert restored.data[0] == 0.0


def test_commit_makes_the_span_durable():
    store = Store.create(in_memory=True)
    with store.transaction():
        add(store, 1, 0.0)
        add(store, 2, 100.0)
    assert len(store.list_metadata()) == 2
    assert not store.in_transaction


def test_commit_applies_deferred_frees():
    store = Store.create(in_memory=True)
    k1 = add(store, 1, 0.0)
    add(store, 2, 100.0)

    with store.transaction():
        store.remove_by_ids([k1])

    assert len(store.list_metadata()) == 1
    # A committed removal reclaims its array exactly as an untransacted one would.
    assert store.num_distinct_arrays() == 1


def test_enter_binds_the_store():
    store = Store.create(in_memory=True)
    with store.transaction() as bound:
        add(bound, 1, 0.0)
    assert len(store.list_metadata()) == 1


def test_nesting_inner_rollback_leaves_outer_open():
    store = Store.create(in_memory=True)
    with store.transaction():
        add(store, 1, 0.0)
        with pytest.raises(RuntimeError):
            with store.transaction():
                add(store, 2, 100.0)
                raise RuntimeError("inner")
        assert store.in_transaction
        assert len(store.list_metadata()) == 1
    assert len(store.list_metadata()) == 1


def test_outer_rollback_discards_committed_inner_transactions():
    store = Store.create(in_memory=True)
    with pytest.raises(RuntimeError):
        with store.transaction():
            with store.transaction():
                add(store, 1, 0.0)
            raise RuntimeError("outer")
    assert len(store.list_metadata()) == 0
    assert store.num_distinct_arrays() == 0


def test_commit_or_rollback_without_a_transaction_raises():
    store = Store.create(in_memory=True)
    with pytest.raises(InvalidParameterError):
        store.commit_transaction()
    with pytest.raises(InvalidParameterError):
        store.rollback_transaction()
    assert not store.in_transaction


def test_read_only_store_cannot_begin_a_transaction(tmp_path):
    path = tmp_path / "store.h5"
    store = Store.create(path=str(path))
    add(store, 1, 0.0)
    store.flush()
    del store

    reopened = Store.open(path=str(path), read_only=True)
    with pytest.raises(ReadOnlyStoreError):
        reopened.begin_transaction()


def test_rollback_survives_a_reopen(tmp_path):
    """Rollback restores the on-disk artifact, not merely the in-memory view."""
    path = tmp_path / "store.h5"
    store = Store.create(path=str(path))
    k1 = add(store, 1, 0.0)

    with pytest.raises(RuntimeError):
        with store.transaction():
            add(store, 2, 100.0)
            store.remove_by_ids([k1])
            raise RuntimeError("boom")
    store.flush()
    del store

    reopened = Store.open(path=str(path), read_only=True)
    keys = reopened.list_metadata()
    assert len(keys) == 1
    assert reopened.read_by_id(keys[0]["id"]).data[0] == 0.0
