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
    "load",  # name (required); pass scaling_factor_multiplier=... for the optional scaling expression
)
```

Use timezone-aware datetimes (UTC is stored). Pass `dtype=np.float64` — the Python binding works in
`float64`. The array may be multi-dimensional: shape `(length,)` for scalar steps, or
`(length, k1, …)` to attach a per-step element shape (such as cost-curve coefficients). The required
`name` and optional `scaling_factor_multiplier` are association attributes carried on the object —
the same array can be added under different names. Use
`NonSequentialTimeSeries(timestamps, data, name)` for explicitly timestamped series.

## Add a Series

```python
key = store.add_time_series(
    owner_uuid="42",
    owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,   # name / scaling_factor_multiplier come from ts
    features={"model_year": 2030, "scenario": "high"},
    units="MW",
)
```

`features` is a plain dict whose values are `int`, `float`, `bool`, or `str`. Adding a series whose
[key](../explanation/data-model.md#keys) already exists raises `DuplicateTimeSeriesError`. The
returned `key` exposes `owner_uuid`, `time_series_type`, `name`, `resolution`, and `features` as
read-only properties.

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

## Query Metadata

`list_time_series` returns a list of plain dicts, filtered by any combination of arguments (the
`features` argument is a subset match):

```python
for m in store.list_time_series(owner_uuid="42", time_series_type=TimeSeriesType.SingleTimeSeries):
    print(m["name"], m["resolution_seconds"], m["units"], m["features"])

keys = store.get_time_series_keys("42")
exists = store.has_time_series(key)
resolutions = store.get_resolutions()          # list[timedelta]
counts = store.get_time_series_counts()        # dict
```

## Remove and Maintain

```python
store.remove_time_series(key)
n = store.clear_time_series("42")   # remove all series for owner "42"; returns count
store.clear_time_series()           # remove everything

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
    owner_uuid="42", owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,
    features={"model_year": 2030}, units="MW",
)
got = store.get_time_series(key)
assert got.name == "load"
assert np.array_equal(np.asarray(got.data), np.asarray(ts.data))
```
