# Python Developer Guide

This guide covers building on the `time_series_store` PyO3 module. For exact signatures and return
shapes, see the [Python API reference](../reference/python-api.md). To install the wheel into your
environment, see [Integrate with Python](../how-to/integrate-python.md).

## Import

```python
from datetime import datetime, timedelta, timezone
import numpy as np
from time_series_store import TimeSeriesStore, SingleTimeSeries, OwnerCategory, TimeSeriesType
```

The module exposes `TimeSeriesStore`, `SingleTimeSeries`, `TimeSeriesKey`, the `TimeSeriesType` and
`OwnerCategory` enums, and an exception hierarchy rooted at `TimeSeriesError`.

## Open or Create a Store

```python
# In-memory: no filesystem I/O.
store = TimeSeriesStore.create(in_memory=True)

# On disk: writes system.nc and system.nc.sqlite.
store = TimeSeriesStore.create(path="system.nc")

# Reopen read-only.
store = TimeSeriesStore.open("system.nc", read_only=True)
```

## Build a Series

`SingleTimeSeries` takes a timezone-aware `datetime`, a `timedelta` resolution, and a NumPy
`float64` array:

```python
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc),
    timedelta(hours=1),
    np.arange(24, dtype=np.float64) + 100,
    "load",  # name (required)
)
```

Use timezone-aware datetimes (UTC is stored). The binding is dtype-generic — it accepts and returns
NumPy arrays of `float64`, `float32`, `int64`, `int32`, `uint64`, or `bool`, and whatever dtype you
pass round-trips unchanged. The array may be multi-dimensional: shape `(length,)` for scalar steps,
or `(length, k1, …)` to attach a per-step element shape (such as cost-curve coefficients). The
required `name` is an association attribute carried on the object — the same array can be added
under different names. Use `NonSequentialTimeSeries(timestamps, data, name)` for explicitly
timestamped series.

## Add a Series

```python
key = store.add_time_series(
    owner_id=42,
    owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,   # name comes from ts
    features={"model_year": 2030, "scenario": "high"},
    units="MW",
)
```

`features` is a plain dict whose values are `int`, `float`, `bool`, or `str`. Adding a series whose
[key](../explanation/data-model.md#keys) already exists raises `DuplicateTimeSeriesError`. The
returned `key` exposes `owner_id`, `owner_category`, `time_series_type`, `name`, `resolution`, and
`features` as read-only properties.

## Read a Series

```python
got = store.get_time_series(key)
assert np.array_equal(np.asarray(got.data), np.asarray(ts.data))
print(got.length, got.initial_timestamp, got.resolution)
```

Slice on the time axis with a `(start, end)` tuple of datetimes (`end` exclusive):

```python
window = store.get_time_series(
    key,
    time_range=(
        datetime(2024, 1, 1, 6, tzinfo=timezone.utc),
        datetime(2024, 1, 1, 12, tzinfo=timezone.utc),
    ),
)
```

To read **many whole series at once** — e.g. loading everything for a plot — `bulk_read` takes a
list of keys and returns the typed series objects in the same order. Packed `SingleTimeSeries` are
read in one decompress-once pass per dataset, which is much faster than a `get_time_series` per key:

```python
series = store.bulk_read(keys)   # keys: list[TimeSeriesKey]
```

## Query Metadata

`list_time_series` returns a list of plain dicts, filtered by any combination of arguments (the
`features` argument is a subset match):

```python
for m in store.list_time_series(
    owner_id=42,
    owner_category=OwnerCategory.Component,
    time_series_type=TimeSeriesType.SingleTimeSeries,
):
    print(m["name"], m["resolution"], m["units"], m["features"])

# The owner is the (owner_id, owner_category) pair.
keys = store.get_time_series_keys(42, OwnerCategory.Component)
exists = store.has_time_series(key)
resolutions = store.get_resolutions()          # list[str] (ISO 8601 durations)
counts = store.get_time_series_counts()        # dict
```

## Remove and Maintain

```python
store.remove_time_series(key)
# The owner is the (owner_id, owner_category) pair.
n = store.clear_time_series(42, OwnerCategory.Component)   # all series for one owner; returns count
store.clear_time_series()                                  # remove everything

report = store.compact()            # {"slots_reclaimed": ..., "datasets_dropped": ...}
errors = store.verify_integrity()   # [] when intact
```

## Persist to Disk

```python
store.flush()   # sync buffered writes; afterwards system.nc + system.nc.sqlite can be copied
```

Keep the two files together — the `.nc` and `.nc.sqlite` pair is a single logical store.

## Error Handling

All exceptions inherit from `TimeSeriesError`, so you can catch broadly or narrowly:

```python
from time_series_store import NotFoundError, DuplicateTimeSeriesError, TimeSeriesError

try:
    store.add_time_series(...)
except DuplicateTimeSeriesError:
    ...                       # key already exists
except TimeSeriesError as e:
    ...                       # anything else from the store
```

One gotcha: because Python's `bool` is a subclass of `int`, the binding deliberately checks `bool`
first, so `True`/`False` feature values are stored as booleans (not as `1`/`0` integers).

## A Complete Round-Trip

```python
from datetime import datetime, timedelta, timezone
import numpy as np
from time_series_store import TimeSeriesStore, SingleTimeSeries, OwnerCategory

store = TimeSeriesStore.create(in_memory=True)
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc),
    timedelta(hours=1),
    np.arange(24, dtype=np.float64) + 100,
    "load",
)
key = store.add_time_series(
    owner_id=42, owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,
    features={"model_year": 2030}, units="MW",
)
got = store.get_time_series(key)
assert got.name == "load"
assert np.array_equal(np.asarray(got.data), np.asarray(ts.data))
```

## Diagnostics and tracing

The store emits structured tracing spans for every significant operation. To see them, initialize a
subscriber before your first store call.

**Via environment variable** — set `RUST_LOG` before starting Python. The module auto-initializes a
subscriber on import when this variable is set:

```sh
RUST_LOG=debug python myscript.py
# or, to limit output to the store core only:
RUST_LOG=time_series_store_core=debug python myscript.py
```

**Programmatically** — call `init_tracing` with a filter directive string:

```python
from time_series_store import init_tracing

init_tracing("time_series_store_core=debug")

store = TimeSeriesStore.create(in_memory=True)
store.add_time_series(...)   # spans appear on stderr
```

`init_tracing` is a no-op if a subscriber is already registered (including the automatic one from
`RUST_LOG`). The filter syntax is the same as `RUST_LOG`: comma-separated `target=level` pairs, or a
bare level such as `"debug"` to match everything. Useful targets:

| Target                   | What it covers                                               |
| ------------------------ | ------------------------------------------------------------ |
| `time_series_store_core` | All store operations — `add`, `get`, `remove` and NetCDF I/O |
