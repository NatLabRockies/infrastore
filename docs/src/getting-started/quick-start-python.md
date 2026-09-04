# Quick Start (Python)

This walkthrough creates an in-memory store, adds a `SingleTimeSeries`, and reads it back — the
shortest path to a working round-trip. It assumes the `infrastore` wheel is installed in the active
environment; if `import infrastore` fails, see
[Integrate with Python](../how-to/integrate-python.md).

## A Minimal Round-Trip

```python
from datetime import datetime, timedelta, timezone

import numpy as np
from infrastore import OwnerCategory, SingleTimeSeries, Store

# `in_memory=True` means no filesystem I/O. Pass `path=` instead to write a
# HDF5 file plus its SQLite catalog.
store = Store.create(in_memory=True)

# The name lives on the series object, not on `add_time_series`.
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc),  # initial timestamp (timezone-aware)
    timedelta(hours=1),                         # resolution
    np.arange(24, dtype=np.float64) + 100,      # 24 hourly values
    "load",                                     # name
)

# The owner is identified by an integer id, an owner type, and a category.
# Features and units are optional.
series_id = store.add_time_series(
    owner_id=42,
    owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,
    features={"model_year": 2030},
    units="MW",
)

got = store.read_by_id(series_id)
print(f"read {got.length} values @ {got.resolution} from {got.initial_timestamp}")
# read 24 values @ PT1H from 2024-01-01 00:00:00+00:00
assert np.array_equal(np.asarray(got.data), np.asarray(ts.data))
```

## What Just Happened

1. **`Store.create(in_memory=True)`** built a store backed by an in-memory array backend and an
   in-memory SQLite metadata database.
2. **`add_time_series`** hashed the array, wrote it to the backend (deduplicating on the hash), and
   recorded a catalog association filed under
   `(owner_id, owner_category, type, name, resolution, interval, features)`. It returned that row's
   **id** — the handle to record in your own object model, and what every read and removal takes
   from here on.
3. **`read_by_id(series_id)`** looked up the row by primary key, read the array back by its content
   hash, and reconstructed a `SingleTimeSeries`.

The array is any NumPy array whose dtype is `float64`, `float32`, `int64`, `int32`, `int16`, `int8`,
`uint64`, `uint32`, `uint16`, `uint8`, or `bool` — whatever you pass round-trips unchanged. Shapes
beyond `(length,)` attach a per-step element shape, such as the coefficient tuple of a cost curve.

## Slice and List

Pass a `(start, end)` tuple of datetimes to read a window instead of the whole series (`end` is
exclusive):

```python
(window,) = store.read_by_ids_range(
    [series_id],
    (
        datetime(2024, 1, 1, 6, tzinfo=timezone.utc),
        datetime(2024, 1, 1, 12, tzinfo=timezone.utc),
    ),
)
print(window.length)   # 6
```

`list_metadata` is how you find a series you did not just write: it returns one dict per catalog
row, filtered by any combination of arguments, and each row carries the `id` to read it by:

```python
for m in store.list_metadata(owner_id=42):
    print(m["name"], m["resolution"], m["units"], m["features"])
# load PT1H MW {'model_year': 2030}
```

## Writing to Disk

Swap the constructor to persist:

```python
store = Store.create(path="system.h5")
# ... add_time_series ...
store.flush()   # sync buffered HDF5 writes to disk
```

This produces two files that travel together:

- `system.h5` — the HDF5 file holding the arrays.
- `system.h5.sqlite` — the catalog holding the metadata associations.

Reopen them later with `Store.open("system.h5", read_only=True)`.

## Next Steps

- Work through the [Python Developer Guide](../guides/python.md) for forecasts, bulk reads,
  associations, and error handling.
- Understand the [Data Model](../explanation/data-model.md): owners, keys, and features.
- Browse the full [Python API reference](../reference/python-api.md).
