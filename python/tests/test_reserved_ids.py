"""`Store.reserve_association_ids`: mint association ids before the rows exist.

A writer that embeds a `TimeSeriesKey` into a component before its batch is
flushed needs the id up front. Reservation hands out a contiguous run the
catalog will never assign on its own; each id is spent by putting it on an
`add_time_series_bulk` item.
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

T0 = datetime(2030, 1, 1, tzinfo=timezone.utc)


def _sts(name: str) -> SingleTimeSeries:
    return SingleTimeSeries(T0, timedelta(hours=1), np.arange(4.0), name)


def _add(store: Store, owner_id: int, name: str) -> None:
    store.add_time_series(
        owner_id=owner_id,
        owner_type="Generator",
        owner_category=OwnerCategory.Component,
        time_series=_sts(name),
    )


def test_reservation_is_a_contiguous_ascending_run_past_the_live_rows():
    store = Store.create(in_memory=True)
    _add(store, 1, "live")
    highest = store.get_metadata(store.list_keys()[0])["association_id"]

    first = store.reserve_association_ids(3)
    assert first > highest

    # A second reservation starts past the first run; the two never overlap.
    second = store.reserve_association_ids(2)
    assert second == first + 3
    run = list(range(first, first + 3)) + list(range(second, second + 2))
    assert run == sorted(run) and len(set(run)) == len(run)


def test_a_series_added_under_a_reserved_id_reads_back_by_that_id():
    store = Store.create(in_memory=True)
    first = store.reserve_association_ids(3)
    reserved = [first, first + 1, first + 2]

    store.add_time_series_bulk(
        [
            {
                "owner_id": owner,
                "owner_type": "Generator",
                "owner_category": OwnerCategory.Component,
                "time_series": _sts(name),
                "association_id": assoc_id,
            }
            for assoc_id, owner, name in zip(reserved, (1, 2, 3), ("a", "b", "c"))
        ]
    )

    for assoc_id, name in zip(reserved, ("a", "b", "c")):
        meta = store.get_metadata_by_association_id(assoc_id)
        assert meta["association_id"] == assoc_id
        assert meta["name"] == name


def test_an_ordinary_add_does_not_collide_with_a_reserved_run():
    store = Store.create(in_memory=True)
    first = store.reserve_association_ids(4)

    # Only the middle of the run is spent; the unspent ids stay gaps and must
    # still not be handed to the plain add path.
    store.add_time_series_bulk(
        [
            {
                "owner_id": 1,
                "owner_type": "Generator",
                "owner_category": OwnerCategory.Component,
                "time_series": _sts("reserved"),
                "association_id": first + 1,
            }
        ]
    )
    _add(store, 2, "ordinary")

    fresh = store.get_metadata(
        next(k for k in store.list_keys() if k.name == "ordinary")
    )["association_id"]
    assert fresh >= first + 4


def test_reserving_zero_is_refused():
    store = Store.create(in_memory=True)
    with pytest.raises(InvalidParameterError, match="at least 1"):
        store.reserve_association_ids(0)
    # Nothing was spent: the first real add still takes id 1.
    _add(store, 1, "load")
    assert store.get_metadata(store.list_keys()[0])["association_id"] == 1


def test_reserving_on_a_read_only_store_is_refused(tmp_path):
    path = tmp_path / "s.h5"
    with Store.create(path=str(path)) as store:
        _add(store, 1, "load")
        store.flush()
    with Store.open(str(path), read_only=True) as ro:
        with pytest.raises(ReadOnlyStoreError):
            ro.reserve_association_ids(2)
