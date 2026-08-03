"""Catalog placement: where the SQLite catalog lives and when it reaches disk.

An *attached* catalog is the ``<path>.sqlite`` file and every commit is durable.
An *in-memory* catalog lives in RAM and reaches disk only through
``persist_to()``, which suits building a store in a scratch directory beside
volatile state — a crash loses that state anyway.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import InvalidParameterError, OwnerCategory, SingleTimeSeries, Store


def make_series(base: float = 100.0) -> SingleTimeSeries:
    initial = datetime(2024, 1, 1, tzinfo=timezone.utc)
    data = np.arange(24, dtype=np.float64) + base
    return SingleTimeSeries(initial, timedelta(hours=1), data, "load")


def add(store: Store, owner_id: int, base: float = 100.0) -> None:
    store.add_time_series(
        owner_id=owner_id,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=make_series(base),
    )


def test_in_memory_catalog_writes_no_sidecar_until_persist(tmp_path):
    scratch = tmp_path / "scratch.h5"
    dest = tmp_path / "system.h5"

    store = Store.create(scratch, catalog="memory")
    assert store.catalog == "memory"
    add(store, 1)
    store.flush()

    assert scratch.exists(), "arrays still stream to the HDF5 file"
    assert not (tmp_path / "scratch.h5.sqlite").exists(), (
        "nothing is durable until persist_to"
    )

    store.persist_to(dest)
    assert (tmp_path / "system.h5.sqlite").exists()
    store.close()


def test_scratch_store_persists_and_reopens(tmp_path):
    scratch = tmp_path / "scratch.h5"
    dest = tmp_path / "system.h5"

    with Store.create(scratch, catalog="memory") as store:
        add(store, 1, 100.0)
        add(store, 2, 200.0)
        store.persist_to(dest)

    with Store.open(dest, read_only=True) as saved:
        assert saved.catalog == "attached"
        assert len(saved.list_keys()) == 2


def test_saved_store_loads_into_memory_and_saves_again(tmp_path):
    first = tmp_path / "first.h5"

    with Store.create(first) as store:
        add(store, 1)
        store.flush()

    # Load the pair into RAM, mutate, save back over the same destination.
    with Store.open(first, catalog="memory") as loaded:
        assert loaded.catalog == "memory"
        add(loaded, 2, 200.0)
        loaded.persist_to(first)

    with Store.open(first, read_only=True) as saved:
        assert len(saved.list_keys()) == 2


def test_catalog_defaults_match_the_backend(tmp_path):
    # Unspecified reproduces the pre-argument behavior in both directions, so
    # existing call sites are unmoved.
    with Store.create(in_memory=True) as store:
        assert store.catalog == "memory"
    with Store.create(tmp_path / "plain.h5") as store:
        assert store.catalog == "attached"


def test_in_memory_backend_rejects_an_attached_catalog():
    with pytest.raises(InvalidParameterError):
        Store.create(in_memory=True, catalog="attached")


def test_unknown_catalog_is_rejected(tmp_path):
    with pytest.raises(InvalidParameterError):
        Store.create(tmp_path / "s.h5", catalog="bogus")


def test_persist_is_rejected_inside_a_transaction(tmp_path):
    scratch = tmp_path / "scratch.h5"
    with Store.create(scratch, catalog="memory") as store:
        store.begin_transaction()
        add(store, 1)
        with pytest.raises(InvalidParameterError):
            store.persist_to(tmp_path / "system.h5")

def test_compaction_still_pairs_with_an_in_memory_catalog(tmp_path):
    """Compaction rewrites the array half and sweeps the catalog.

    With the catalog in RAM only the first of those lands, so the rewritten file
    has to carry the stamp the in-memory catalog is still holding — otherwise the
    checkpoint that follows pairs two halves that disagree.
    """
    scratch = tmp_path / "scratch.h5"

    with Store.create(scratch, catalog="memory") as store:
        add(store, 1, 100.0)
        add(store, 2, 200.0)
        store.clear_time_series(owner_id=2, owner_category=OwnerCategory.Component)

        report = store.compact()
        assert report["slots_reclaimed"] + report["datasets_dropped"] > 0
        assert not (tmp_path / "scratch.h5.sqlite").exists(), (
            "compaction must not land a catalog nobody asked to write"
        )
        store.persist_catalog()

    with Store.open(scratch, read_only=True) as reopened:
        assert [k.owner_id for k in reopened.list_keys()] == [1]
        assert reopened.verify_integrity()["ok"]


def test_rollback_under_an_in_memory_catalog_takes_its_arrays_with_it(tmp_path):
    """The array half of a rollback happens against the backend, so a catalog in
    RAM changes nothing about it. What this guards against is a rolled-back array
    surviving as an orphan the eventual checkpoint never names."""
    scratch = tmp_path / "scratch.h5"

    with Store.create(scratch, catalog="memory") as store:
        add(store, 1, 100.0)
        store.begin_transaction()
        add(store, 2, 200.0)
        store.rollback_transaction()
        store.persist_catalog()

    with Store.open(scratch) as reopened:
        assert [k.owner_id for k in reopened.list_keys()] == [1]
        assert reopened.verify_integrity()["ok"]
        assert reopened.compact()["datasets_dropped"] == 0


def test_a_failed_save_leaves_the_store_live(tmp_path):
    """``persist_to()`` drops the live HDF5 handle before copying the file, so a
    save that cannot stage has to put it back. Without that the store is stranded
    on an empty placeholder and every later call quietly reads nothing."""
    scratch = tmp_path / "scratch.h5"

    with Store.create(scratch, catalog="memory") as store:
        add(store, 1, 100.0)
        with pytest.raises(Exception):
            store.persist_to(tmp_path / "no-such-dir" / "system.h5")

        # Still the same store: readable, writable, saveable.
        assert len(store.list_keys()) == 1
        add(store, 2, 200.0)
        good = tmp_path / "system.h5"
        store.persist_to(good)

    with Store.open(good, read_only=True) as saved:
        assert sorted(k.owner_id for k in saved.list_keys()) == [1, 2]
