"""The guards that keep an already-saved artifact from being destroyed.

A store's two halves are written once and then, in the workflow this library is
built for, never touched in place again: a consumer builds in a scratch
directory and ``persist_to()``s the result. What threatens that saved pair is
not mainly crashes — every path that writes it stages and renames — but ordinary
calls that quietly do the wrong thing to a path that already holds a save.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import (
    MismatchedArtifactError,
    OwnerCategory,
    SingleTimeSeries,
    Store,
    StoreExistsError,
)


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


def saved_store(path) -> None:
    """A complete, saved store at ``path`` holding one series for owner 1."""
    with Store.create(path) as store:
        add(store, 1, 100.0)
        store.flush()


def test_creating_over_a_saved_store_is_refused(tmp_path):
    """The failure this guard exists for, in full.

    Creating truncates the HDF5 file but only *opens* the catalog, then stamps
    both halves with one fresh generation. Without the guard, pointing a build
    script at a path that already holds a save left an empty array file paired
    with the old catalog's rows — a store that opens cleanly, reports every
    series still present, and has nothing behind any of them. No crash needed.
    """
    path = tmp_path / "system.h5"
    saved_store(path)
    before = path.stat().st_size

    with pytest.raises(StoreExistsError):
        Store.create(path)

    # Refused means untouched, not partially applied.
    assert path.stat().st_size == before
    with Store.open(path, read_only=True) as store:
        assert len(store.list_keys()) == 1
        assert store.verify_integrity()["ok"]


def test_creating_over_a_lone_half_is_refused(tmp_path):
    """Either half alone is enough to poison a fresh store."""
    only_catalog = tmp_path / "catalog_only.h5"
    saved_store(only_catalog)
    only_catalog.unlink()
    with pytest.raises(StoreExistsError):
        Store.create(only_catalog)

    only_arrays = tmp_path / "arrays_only.h5"
    saved_store(only_arrays)
    (tmp_path / "arrays_only.h5.sqlite").unlink()
    with pytest.raises(StoreExistsError):
        Store.create(only_arrays)


def test_overwrite_discards_both_halves(tmp_path):
    path = tmp_path / "system.h5"
    saved_store(path)

    with Store.create(path, overwrite=True) as store:
        add(store, 2, 200.0)
        store.flush()

    # Had the old catalog survived, owner 1 would still be listed with nothing
    # behind it.
    with Store.open(path, read_only=True) as store:
        assert [k.owner_id for k in store.list_keys()] == [2]
        assert store.verify_integrity()["ok"]


def test_overwrite_is_rejected_for_an_in_memory_store():
    from infrastore import InvalidParameterError

    with pytest.raises(InvalidParameterError):
        Store.create(in_memory=True, overwrite=True)


def test_open_copy_leaves_the_original_alone(tmp_path):
    src = tmp_path / "system.h5"
    dest = tmp_path / "scratch.h5"
    saved_store(src)
    original = src.read_bytes()

    with Store.open_copy(src, dest) as copy:
        assert len(copy.list_keys()) == 1
        add(copy, 2, 200.0)
        copy.flush()

    assert src.read_bytes() == original
    with Store.open(src, read_only=True) as store:
        assert [k.owner_id for k in store.list_keys()] == [1]

    # The round trip a consumer actually runs: change the copy, save back over
    # the original, which one atomic rename replaces.
    with Store.open(dest) as copy:
        copy.persist_to(src)
    with Store.open(src, read_only=True) as reloaded:
        assert sorted(k.owner_id for k in reloaded.list_keys()) == [1, 2]
        assert reloaded.verify_integrity()["ok"]


def test_open_copy_refuses_a_live_destination(tmp_path):
    src = tmp_path / "system.h5"
    dest = tmp_path / "other.h5"
    saved_store(src)
    saved_store(dest)

    with pytest.raises(StoreExistsError):
        Store.open_copy(src, dest)


def test_a_half_artifact_does_not_open_as_an_empty_store(tmp_path):
    """A lone HDF5 half used to read back as a valid, empty store.

    The paired generation stamp now catches it: the arrays are stamped and the
    catalog created beside them is not. Both halves unstamped stays legal — that
    is an artifact written before stamping existed.
    """
    path = tmp_path / "system.h5"
    saved_store(path)
    (tmp_path / "system.h5.sqlite").unlink()

    with pytest.raises(MismatchedArtifactError):
        Store.open(path)


def test_persist_catalog_pairs_an_in_memory_catalog_with_its_arrays(tmp_path):
    path = tmp_path / "scratch.h5"

    with Store.create(path, catalog="memory") as store:
        add(store, 1, 100.0)
        assert not (tmp_path / "scratch.h5.sqlite").exists()
        # Writes only the catalog: the arrays are already where they belong, so
        # unlike persist_to() this copies nothing.
        store.persist_catalog()

    with Store.open(path, read_only=True) as reopened:
        assert len(reopened.list_keys()) == 1
        assert reopened.verify_integrity()["ok"]


def test_persist_catalog_is_a_checkpoint_not_a_mode_switch(tmp_path):
    path = tmp_path / "scratch.h5"

    with Store.create(path, catalog="memory") as store:
        add(store, 1, 100.0)
        store.persist_catalog()
        # Written after the checkpoint: RAM-only until the next one.
        add(store, 2, 200.0)
        store.flush()

        with Store.open(path, read_only=True) as reopened:
            assert [k.owner_id for k in reopened.list_keys()] == [1]

        store.persist_catalog()
        with Store.open(path, read_only=True) as reopened:
            assert sorted(k.owner_id for k in reopened.list_keys()) == [1, 2]


def test_a_failed_catalog_checkpoint_keeps_the_catalog_in_ram(tmp_path):
    """A checkpoint that cannot land must not consume the catalog it was
    writing. It stays in RAM, still complete and still writable, and the next
    attempt lands everything — including what was added after the failure."""
    scratch = tmp_path / "scratch.h5"
    sidecar = tmp_path / "scratch.h5.sqlite"

    with Store.create(scratch, catalog="memory") as store:
        add(store, 1, 100.0)

        # A directory where the sidecar belongs: staging succeeds, the rename
        # cannot.
        sidecar.mkdir()
        with pytest.raises(Exception):
            store.persist_catalog()
        sidecar.rmdir()

        add(store, 2, 200.0)
        store.persist_catalog()

    with Store.open(scratch, read_only=True) as reopened:
        assert sorted(k.owner_id for k in reopened.list_keys()) == [1, 2]


def test_an_abandoned_scratch_file_blocks_recreation(tmp_path):
    """The scratch-directory workflow's recovery path.

    A run that dies before landing its catalog leaves the array half behind. The
    next run must refuse to create over it rather than pairing a fresh empty
    catalog with the leftover arrays, and ``overwrite=True`` must be the way
    through.
    """
    scratch = tmp_path / "scratch.h5"
    with Store.create(scratch, catalog="memory") as store:
        add(store, 1, 100.0)
        store.flush()  # arrays land; the catalog dies with the process

    with pytest.raises(StoreExistsError):
        Store.create(scratch, catalog="memory")

    with Store.create(scratch, catalog="memory", overwrite=True) as fresh:
        add(fresh, 7, 700.0)
        fresh.persist_catalog()

    with Store.open(scratch, read_only=True) as store:
        assert [k.owner_id for k in store.list_keys()] == [7]


def test_open_copy_of_a_half_artifact_refuses_rather_than_reading_it_empty(tmp_path):
    """A source with no catalog is what an abandoned scratch run leaves.

    ``open_copy`` copies no catalog for it, deliberately, and the copy then fails
    to open rather than presenting the arrays as an empty store.
    """
    scratch = tmp_path / "scratch.h5"
    with Store.create(scratch, catalog="memory") as store:
        add(store, 1, 100.0)
        store.flush()

    with pytest.raises(MismatchedArtifactError):
        Store.open_copy(scratch, tmp_path / "copy.h5")
