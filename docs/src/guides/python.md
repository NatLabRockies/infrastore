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

The module exposes `TimeSeriesStore`; the static series classes `SingleTimeSeries` and
`NonSequentialTimeSeries`; the forecast classes `Deterministic`, `Probabilistic`, and `Scenarios`;
`TimeSeriesKey`; the `TimeSeriesType` and `OwnerCategory` enums; the `init_tracing` function; and an
exception hierarchy rooted at `TimeSeriesError`.

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

`SingleTimeSeries` takes a timezone-aware `datetime`, a resolution (a `timedelta` or an ISO 8601
duration string such as `"PT1H"` — the string form is required for calendar periods like `"P1M"`),
and a NumPy array:

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
returned `key` exposes `owner_id`, `owner_category`, `time_series_type`, `name`, `resolution`,
`interval`, and `features` as read-only properties (`resolution` and `interval` are ISO 8601
duration strings or `None`).

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

# Reassign every series from one owner to another; returns the number moved.
moved = store.replace_owner(42, 43, OwnerCategory.Component)

report = store.compact()            # {"slots_reclaimed": ..., "datasets_dropped": ...,
                                   #  "feature_sets_reclaimed": ...}
errors = store.verify_integrity()   # [] when intact
```

## Associations

Two catalog tables record relationships between entities the store does not otherwise model, wholly
independently of time series: which supplemental attributes are attached to which components, and
directed parent/child edges between components. Removing a time series never touches either, and
vice versa — see
[Associations Between Entities](../explanation/data-model.md#associations-between-entities).

Filter arguments are keyword-only, all optional, and ANDed; passing none matches everything.

```python
from time_series_store import (
    SupplementalAttributeAssociation,
    ParentChildAssociation,
    DuplicateAssociationError,
)

store.add_supplemental_attribute_association(
    SupplementalAttributeAssociation(42, "Generator", 100, "GeographicInfo")
)

# Bulk add is one all-or-nothing transaction.
store.add_supplemental_attribute_associations([
    SupplementalAttributeAssociation(43, "Generator", 100, "GeographicInfo"),
    SupplementalAttributeAssociation(43, "Generator", 101, "Outage"),
])

# Queries run in both directions, returning distinct ids in ascending order.
assert store.list_supplemental_attribute_ids(component_id=43) == [100, 101]
assert store.list_components_with_attributes(attribute_id=100) == [42, 43]
assert store.has_supplemental_attribute_association(component_id=42, attribute_id=100)

# `*_types` filters take CONCRETE type names. Expanding an abstract type into its
# subtypes is the caller's job — the store has no type hierarchy. An empty list is a
# deliberate "none of these" and matches nothing.
assert store.list_supplemental_attribute_ids(
    component_id=43, attribute_types=["Outage"]
) == [101]

assert store.count_supplemental_attributes() == 2        # distinct attributes
assert store.count_components_with_attributes() == 2     # distinct components
store.supplemental_attribute_counts_by_type()
# [('GeographicInfo', 2), ('Outage', 1)]
store.supplemental_attribute_summary()
# [{'component_type': 'Generator', 'attribute_type': 'GeographicInfo', 'count': 2}, ...]
```

Identity is the `(component_id, attribute_id)` pair. The type names ride along for filtering and are
not part of it, so re-attaching the same pair under different type names is still a duplicate:

```python
try:
    store.add_supplemental_attribute_association(
        SupplementalAttributeAssociation(42, "Load", 100, "Outage")
    )
except DuplicateAssociationError as e:
    print(e)   # attribute 100 is already attached to component 42

# Removal returns a count. Matching nothing returns 0 rather than raising, so assert on
# the count yourself if you expected a hit.
assert store.remove_supplemental_attribute_associations(component_id=43) == 2
```

Parent/child edges work the same way, except that identity is the **ordered** pair — the reverse of
an edge is a different edge — and both endpoints are always components:

```python
store.add_parent_child_association(ParentChildAssociation(42, "Generator", 7, "Bus"))
store.add_parent_child_associations([ParentChildAssociation(43, "Generator", 7, "Bus")])

assert store.list_children(parent_id=42) == [7]
assert store.list_parents(child_id=7) == [42, 43]
assert store.count_parent_child_associations() == 2

# Renumbering a component rewrites both ends of every edge.
assert store.replace_parent_child_component_id(42, 99) == 1
assert store.list_parents(child_id=7) == [43, 99]
```

Neither table is reachable over gRPC or the `tss` CLI.

## Persist to Disk

```python
store.flush()   # sync buffered writes; afterwards system.nc + system.nc.sqlite can be copied
```

Keep the two files together — the `.nc` and `.nc.sqlite` pair is a single logical store.

## Error Handling

The store's own exceptions inherit from `TimeSeriesError`, so you can catch broadly or narrowly:

```python
from time_series_store import NotFoundError, DuplicateTimeSeriesError, TimeSeriesError

try:
    store.add_time_series(...)
except DuplicateTimeSeriesError:
    ...                       # key already exists
except TimeSeriesError as e:
    ...                       # anything else from the store
```

Argument parsing is the exception to that rule: a period argument (`resolution`, `horizon`,
`interval`) that is a malformed ISO 8601 duration string raises a plain `ValueError`, and one that
is neither a `timedelta` nor a `str` raises a plain `TypeError`. Neither is a `TimeSeriesError`, so
`except TimeSeriesError` will not catch them.

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
