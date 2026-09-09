"""The write-buffer budget: how wide a dataset a run of single adds writes.

Inside a transaction a packed add joins a pending block per shape group instead
of filling a growth-pool slot, and each block becomes one dataset at the commit.
The budget is the ceiling on what those blocks hold, and so the one thing left
that separates a loop of ``add_time_series`` from ``add_time_series_bulk`` of the
same series: a batch handed over as a list is written as one block with no
budget applied, because the caller is already holding it.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import numpy as np
import pytest

from infrastore import InvalidParameterError, OwnerCategory, SingleTimeSeries, Store

# One column below is 24 float64 values.
COLUMN_BYTES = 24 * 8


def series(base: float) -> SingleTimeSeries:
    return SingleTimeSeries(
        initial_timestamp=datetime(2024, 1, 1, tzinfo=timezone.utc),
        resolution=timedelta(hours=1),
        data=np.arange(24, dtype=float) + base,
        name="load",
    )


def add_ten(store: Store) -> None:
    with store.transaction():
        for owner in range(1, 11):
            store.add_time_series(
                owner_id=owner,
                owner_type="Generator",
                owner_category=OwnerCategory.Component,
                time_series=series(owner * 100.0),
            )


def packed_shapes(path) -> dict[str, tuple[int, ...]]:
    # h5py is an optional test dependency, as it is for the interop suite, so
    # only the two tests that read the datasets back skip without it — the
    # budget's own behaviour is checked either way.
    h5py = pytest.importorskip("h5py", reason="h5py not installed")
    with h5py.File(path, "r") as f:
        group = f["time_series/single"]
        return {
            name: group[name].shape
            for name in group
            if not name.endswith("_h")
        }


def test_the_default_is_128_mib():
    store = Store.create(in_memory=True)
    assert store.write_buffer_bytes == 128 << 20


def test_a_narrow_budget_spills_a_span_into_several_datasets(tmp_path):
    path = tmp_path / "narrow.h5"
    store = Store.create(path=str(path))
    store.write_buffer_bytes = COLUMN_BYTES * 4
    assert store.write_buffer_bytes == COLUMN_BYTES * 4
    add_ten(store)
    store.flush()
    store.close()

    assert packed_shapes(path) == {
        "sts_f64_s_24_PT1H": (24, 4),
        "sts_f64_s_24_PT1H__1": (24, 4),
        "sts_f64_s_24_PT1H__2": (24, 2),
    }


def test_a_wide_budget_writes_the_dataset_the_bulk_add_writes(tmp_path):
    looped = tmp_path / "looped.h5"
    store = Store.create(path=str(looped))
    store.write_buffer_bytes = COLUMN_BYTES * 64
    add_ten(store)
    store.flush()
    store.close()

    bulked = tmp_path / "bulked.h5"
    other = Store.create(path=str(bulked))
    other.add_time_series_bulk(
        [
            {
                "owner_id": owner,
                "owner_type": "Generator",
                "owner_category": OwnerCategory.Component,
                "time_series": series(owner * 100.0),
            }
            for owner in range(1, 11)
        ]
    )
    other.flush()
    other.close()

    assert packed_shapes(looped) == packed_shapes(bulked) == {"sts_f64_s_24_PT1H": (24, 10)}


def test_zero_is_refused_and_leaves_the_budget_alone():
    store = Store.create(in_memory=True)
    before = store.write_buffer_bytes
    with pytest.raises(InvalidParameterError):
        store.write_buffer_bytes = 0
    assert store.write_buffer_bytes == before
