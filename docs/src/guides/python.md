# Python Developer Guide

This guide covers building on the `infrastore` PyO3 module, from installing the wheel to the calls a
consumer package makes. For exact signatures and return shapes, see the
[Python API reference](../reference/python-api.md).

## Install

Python 3.11 or newer. The wheels are prebuilt and statically linked, so a consumer package such as
infrasys needs nothing else:

```sh
pip install infrastore
```

The wheel is built against the **`abi3-py311`** stable ABI, so one wheel works on CPython 3.11 and
every newer 3.x without recompiling.

### From a checkout

Building from source needs the [build tools](../getting-started/installation.md#build-prerequisites)
(`cmake`, a C compiler, `protobuf`) — but **no system HDF5**. The binding is built with
[maturin](https://www.maturin.rs/); `maturin develop` compiles the extension and installs it into
the active virtual environment:

```sh
cd crates/infrastore-py
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy tzdata  # tzdata: zoneinfo on Windows
pip install netCDF4 h5py                 # only for the HDF5-interop tests
maturin develop

python -c "import infrastore; print(infrastore.__version__)"
pytest ../../python/tests
```

To produce a wheel you can install elsewhere:

```sh
maturin build --release
# -> target/wheels/infrastore-<version>-cp311-abi3-<platform>.whl
```

Installing an unreleased core into a consumer's environment is the same `maturin develop`, run with
that consumer's venv active.

### If it does not import

- **`ImportError` for the extension** — Ensure you ran `maturin develop` in the active venv, or that
  you `pip install`-ed the wheel into the interpreter you are running.
- **HDF5 build errors with `HDF5_DIR` set** — Unset it. The vendored build compiles its own HDF5 and
  the variable redirects it at an external install while static libraries are still requested (see
  [Build Prerequisites](../getting-started/installation.md#build-prerequisites)).
- **`InvalidParameterError` on add** — Pass a NumPy array (any shape) whose dtype is one of
  `float64`, `float32`, `int64`, `int32`, `int16`, `int8`, `uint64`, `uint32`, `uint16`, `uint8`, or
  `bool`; any other dtype (e.g. `complex128` or a string dtype) raises. Feature values must be
  `int`/`float`/`bool`/`str`. Timestamps for a `NonSequentialTimeSeries` must be strictly
  increasing.

## Import

```python
from datetime import datetime, timedelta, timezone
import numpy as np
from infrastore import Store, SingleTimeSeries, OwnerCategory, TimeSeriesType
```

The module exposes `Store` and `Transaction`; the static series classes `SingleTimeSeries` and
`NonSequentialTimeSeries`; the forecast classes `Deterministic`, `Probabilistic`, and `Scenarios`;
the readers `StaticReader` and `ForecastReader`; the association records
`SupplementalAttributeAssociation` and `ParentChildAssociation`; the `TimeSeriesType` and
`OwnerCategory` enums; the `init_tracing` and `decode_element_values` functions; `__version__`; and
an exception hierarchy rooted at `TimeSeriesError`.

If you are building a package on top of infrastore — the way
[infrasys](https://github.com/NatLabRockies/infrasys) does — read
[Embedding in a Parent Package](./embedding.md) alongside this guide: it covers the lifecycle
(scratch store, `persist_to`, `open_copy`), id mapping, and lookup semantics that this page only
shows the calls for.

## Open or Create a Store

```python
# In-memory: no filesystem I/O.
store = Store.create(in_memory=True)

# On disk: writes system.h5 and system.h5.sqlite.
store = Store.create(path="system.h5")

# Reopen read-only.
store = Store.open("system.h5", read_only=True)
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
NumPy arrays of `float64`, `float32`, `int64`, `int32`, `int16`, `int8`, `uint64`, `uint32`,
`uint16`, `uint8`, or `bool`, and whatever dtype you pass round-trips unchanged. The array may be
multi-dimensional: shape `(length,)` for scalar steps, or `(length, k1, …)` to attach a per-step
element shape (such as cost-curve coefficients). The required `name` is an association attribute
carried on the object — the same array can be added under different names. Use
`NonSequentialTimeSeries(timestamps, data, name)` for explicitly timestamped series.

## Add a Series

```python
series_id = store.add_time_series(
    owner_id=42,
    owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,   # name comes from ts
    features={"model_year": 2030, "scenario": "high"},
    units="MW",
)
# `series_id` is the catalog row's id: how every read and removal
# addresses the series, and one integer to keep in your own model.
```

`features` is a plain dict whose values are `int`, `float`, `bool`, or `str`. Adding a series whose
[identity](../explanation/data-model.md#identity) already exists raises `DuplicateTimeSeriesError`.
The add returns the id and nothing else. To see the rest of the row — `owner_id`, `owner_category`,
`time_series_type`, `name`, `resolution`, `interval`, `features`, and the descriptors below — ask
`store.get_metadata_by_id(series_id)`, or `store.list_metadata(...)` for a set of them (`resolution`
and `interval` come back as ISO 8601 duration strings or `None`).

### Descriptors

Beyond `units`, an association can carry `quantity_kind` (what the values measure — `"ActivePower"`;
the one record of what per-unit values mean), `unit_system` (`"natural_units"` or
`"component_base"`; unset means _unspecified_, not natural units), `component_field` (the field on
the owning component these values vary — `"max_active_power"`; also a filter), and
`application_data` (an opaque string the store returns verbatim — the package-owned slot):

```python
series_id = store.add_time_series(
    owner_id=42, owner_type="Generator", owner_category=OwnerCategory.Component,
    time_series=ts,
    units="MW", quantity_kind="ActivePower", unit_system="natural_units",
    component_field="max_active_power",
    application_data='{"source": "weather_year_2012"}',
)
```

A series also records a `time_reference` — how its timestamps were spelled — inferred from the
`datetime` it was built with: `timezone.utc` gives `"utc"`, a fixed-offset `tzinfo` gives
`"-07:00"`, a `ZoneInfo` gives its name, and a **naive** datetime gives `"zoneless"`. A naive
datetime is accepted (it names a wall clock, not an instant) precisely because the read hands one
back — naive and aware datetimes are never equal in Python, so returning the other kind would break
every `==` a caller writes.

None of them is part of the key or of either content hash, so two adds that differ only in a
descriptor are a duplicate. See
[Optional Descriptors](../explanation/data-model.md#optional-descriptors) and
[Time references](../explanation/time-references.md).

### Add many series at once

`add_time_series_bulk` takes a list of dicts mirroring `add_time_series`'s keyword arguments and
commits them in one catalog transaction, taking the block-sized HDF5 write path. It is the way to
load a system: an order of magnitude faster than a loop of single adds, and same-shaped series land
in the same packed dataset.

```python
ids = store.add_time_series_bulk([
    {"owner_id": i, "owner_type": "Generator", "owner_category": OwnerCategory.Component,
     "time_series": series[i], "units": "MW"}
    for i in range(len(series))
])   # one catalog id per item, in input order; all-or-nothing
```

### Transactions

Several operations that must take effect together — replacing a series is an add plus a remove — go
inside a transaction. Removals are reversible only there; outside one the array bytes are reclaimed
immediately.

```python
with store.transaction():
    new_id = store.add_time_series(owner_id=42, owner_type="Generator",
                                   owner_category=OwnerCategory.Component,
                                   time_series=updated)
    store.remove_by_ids([old_id])
# committed on a clean exit, rolled back if the block raised
```

Blocks nest (each level is a savepoint), and the store holds the SQLite write lock until the
outermost one ends. A transaction does not batch: use `add_time_series_bulk` inside it for the
writes themselves. `begin_transaction` / `commit_transaction` / `rollback_transaction` are the
explicit form.

## Read a Series

```python
got = store.read_by_id(series_id)
assert np.array_equal(np.asarray(got.data), np.asarray(ts.data))
print(got.length, got.initial_timestamp, got.resolution)
```

Slice on the time axis with a `(start, end)` tuple of datetimes (`end` exclusive). A range **clips**
to what is there:

```python
(window,) = store.read_by_ids_range(
    [series_id],
    (
        datetime(2024, 1, 1, 6, tzinfo=timezone.utc),
        datetime(2024, 1, 1, 12, tzinfo=timezone.utc),
    ),
)
```

`read_by_id` takes the other kind of slice: `start_time` plus a `len` of timesteps or a `count` of
windows, **checked** rather than clipped, so an over-long request raises rather than quietly
returning less.

To read **many whole series at once** — e.g. loading everything for a plot — `read_by_ids` takes a
list of ids and returns the typed series objects in the same order. Packed `SingleTimeSeries` are
read in one decompress-once pass per dataset, which is much faster than a `read_by_id` each:

```python
series = store.read_by_ids(ids)
window = store.read_by_ids_range(ids, (start, end))   # the same clip on every series
```

### Datetimes and precision

Every `datetime` must be timezone-aware (any zone; converted to UTC on the way in, UTC on the way
out), and a naive one raises `InvalidParameterError`. A **stored** instant — an initial timestamp, a
`NonSequentialTimeSeries` timestamp — must also be a whole number of milliseconds, so quantize
`datetime.now(timezone.utc)` before storing it; query bounds such as `time_range` are unconstrained.
See [Datetimes](../reference/python-api.md#datetimes).

## Per-Timestamp Reads (Simulation Loop)

`read_by_id` hands back a whole series or forecast. Simulations instead walk the timeline and, at
each timestamp, want the value of _every_ series at that instant. For that, build a **reader** once
and drive it in a loop — it pins one resolution and reuses its output buffers, so the loop allocates
almost nothing. `StaticReader` serves `SingleTimeSeries`; `ForecastReader` serves forecasts. (Full
signatures: [Python API reference](../reference/python-api.md#readers).)

### Static series

Series are grouped by `(dtype, element_shape)`; each group's `group_values` is one dense
`(num_columns, *element_shape)` array whose columns line up with that group's `ids`. All matched
series must share one grid (`initial_timestamp` + `length`), validated at build.

```python
reader = store.build_static_reader(timedelta(hours=1))
grid = reader.grid()               # {"initial_timestamp", "resolution", "length", "time_series_type"}
groups = reader.groups()           # each: {"dtype", "element_type", "element_shape", "ids"}
for ts in reader.timestamps():
    store.static_read(reader, ts)
    for i, g in enumerate(groups):
        vals = reader.group_values(i)   # (num_columns, *element_shape); column j ↔ g["ids"][j]
```

### Forecasts

`entry_values(i)` returns the window backing `entries()[i]`, shaped `(horizon, *element_shape)` for
`Deterministic`/`DeterministicSingleTimeSeries`, `(num_percentiles, horizon, *element_shape)` for
`Probabilistic`, and `(scenario_count, horizon, *element_shape)` for `Scenarios`. A `Deterministic`
reader is abstract — it also includes any `DeterministicSingleTimeSeries` (read into identical
windows).

```python
reader = store.build_forecast_reader(TimeSeriesType.Deterministic, timedelta(hours=1))
tl = reader.timeline()             # {"initial_timestamp", "resolution", "interval", "count", ...}
entries = reader.entries()         # list[int]: catalog ids, parallel to entry_values
for ts in reader.timestamps():
    store.forecast_read(reader, ts)
    for i, entry_id in enumerate(entries):
        window = reader.entry_values(i)   # the window for that id's series
```

### Shared forecasts are read once

Forecasts that share a backing array (deduplicated identical data, or several
`DeterministicSingleTimeSeries` over one `SingleTimeSeries`) collapse to a single **window slot**.
`forecast_read` reads each slot from the `.h5` file once per timestamp, so a forecast shared by 10
components costs one read, not ten. `reader.num_slots()` is the physical read count, and
`reader.entry_slot(i)` says which slot an entry uses — group by slot to materialize each unique
window only once on the Python side too:

```python
store.forecast_read(reader, ts)
windows: dict[int, np.ndarray] = {}
for i, key in enumerate(entries):
    window = windows.setdefault(reader.entry_slot(i), reader.entry_values(i))
```

## Query Metadata

`list_metadata` returns a list of plain dicts, filtered by any combination of arguments (the
`features` argument is a subset match):

```python
for m in store.list_metadata(
    owner_id=42,
    owner_category=OwnerCategory.Component,
    time_series_type=TimeSeriesType.SingleTimeSeries,
):
    print(m["name"], m["resolution"], m["units"], m["features"])

# The owner is the (owner_id, owner_category) pair.
rows = store.list_metadata(owner_id=42, owner_category=OwnerCategory.Component)
ids = [r["id"] for r in rows]
exists = store.association_exists(ids[0])
resolutions = store.get_resolutions()          # list[str] (ISO 8601 durations)
counts = store.get_time_series_counts()        # dict
```

## Remove and Maintain

```python
store.remove_by_ids([series_id])   # one series, or many in one transaction
# The owner is the (owner_id, owner_category) pair.
n = store.clear_time_series(owner_id=42, owner_category=OwnerCategory.Component)  # one owner; returns count
store.clear_time_series()                                  # remove everything

# Reassign every series from one owner to another; returns the number moved.
moved = store.replace_owner(42, 43, OwnerCategory.Component)

report = store.compact()            # rewrites the .h5 from the live set; the report includes
                                   #  "slots_reclaimed", "datasets_dropped",
                                   #  "feature_sets_reclaimed", "timestamp_sets_reclaimed",
                                   #  "bytes_reclaimed"
integrity = store.verify_integrity()   # {"ok": True, "errors": []} when every array and time
                                   # axis the catalog names matches its recorded hash
```

## Associations

Two catalog tables record relationships between entities the store does not otherwise model, wholly
independently of time series: which supplemental attributes are attached to which components, and
directed parent/child edges between components. Removing a time series never touches either, and
vice versa — see
[Associations Between Entities](../explanation/data-model.md#associations-between-entities).

Filter arguments are keyword-only, all optional, and ANDed; passing none matches everything.

```python
from infrastore import (
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

Neither table is reachable over gRPC or the `infrastore` CLI.

## Persist to Disk

```python
store.flush()   # sync buffered writes; afterwards system.h5 + system.h5.sqlite can be copied
```

Keep the two files together — the `.h5` and `.h5.sqlite` pair is a single logical store.

To change a store you did not build in this process, **open a copy**: `Store.open` defaults to
read-write, and HDF5 has no journal, so an interrupted in-place write is unrecoverable.

```python
store = Store.open_copy(src, scratch / "time_series.h5")   # src is never opened for writing
...
store.persist_to(src)                                       # one atomic rename replaces it
```

`Store.open(path, read_only=True)` is the right call when nothing will be written.

### Where the Catalog Lives

By default the catalog _is_ `system.h5.sqlite`, and every commit is durable. Passing
`catalog="memory"` keeps it in RAM instead, so it reaches disk only when you call `persist_to()`:

```python
# Build in a scratch directory; nothing is durable until the explicit save.
store = Store.create(scratch / "time_series.h5", catalog="memory")
store.add_time_series(...)
store.persist_to(destination)     # writes both halves as a matched pair
store.persist_catalog()           # or: land only the .sqlite half beside the arrays already at path
```

Arrays still stream to the HDF5 file, so this does not require the data to fit in memory. It suits
building a store beside volatile in-process state — a crash loses that state anyway, so journaling
the scratch catalog buys nothing. Read `store.catalog` to see which mode a store is in.

`Store.open(path, catalog="memory")` loads an existing catalog into RAM the same way. Note that the
HDF5 half is still opened **in place**, so mutations land in the original file; open a copy if you
mean to leave the source untouched until an explicit save.

`persist_to()` stages both halves and renames them into place, and stamps the pair so that a save
interrupted between the two renames is caught on the next open rather than read as a valid store. It
does replace the destination, though, so a failed save may have destroyed what was there — recover
by calling `persist_to()` again on the still-live store rather than assuming the target survived.

## Error Handling

The store's own exceptions inherit from `TimeSeriesError`, so you can catch broadly or narrowly:

```python
from infrastore import NotFoundError, DuplicateTimeSeriesError, TimeSeriesError

try:
    store.add_time_series(...)
except DuplicateTimeSeriesError:
    ...                       # key already exists
except TimeSeriesError as e:
    ...                       # anything else from the store
```

Argument validation stays inside the hierarchy: a malformed ISO 8601 duration string, a naive
`datetime`, a sub-millisecond stored timestamp, and an unsupported NumPy dtype all raise
`InvalidParameterError`. The one exception is a period argument that is neither a `timedelta` nor a
`str`, which raises a plain `TypeError` that `except TimeSeriesError` will not catch.

One gotcha: because Python's `bool` is a subclass of `int`, the binding deliberately checks `bool`
first, so `True`/`False` feature values are stored as booleans (not as `1`/`0` integers).

## A Complete Round-Trip

```python
from datetime import datetime, timedelta, timezone
import numpy as np
from infrastore import Store, SingleTimeSeries, OwnerCategory

store = Store.create(in_memory=True)
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc),
    timedelta(hours=1),
    np.arange(24, dtype=np.float64) + 100,
    "load",
)
series_id = store.add_time_series(
    owner_id=42, owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,
    features={"model_year": 2030}, units="MW",
)
got = store.read_by_id(series_id)
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
RUST_LOG=infrastore_core=debug python myscript.py
```

**Programmatically** — call `init_tracing` with a filter directive string:

```python
from infrastore import init_tracing

init_tracing("infrastore_core=debug")

store = Store.create(in_memory=True)
store.add_time_series(...)   # spans appear on stderr
```

`init_tracing` is a no-op if a subscriber is already registered (including the automatic one from
`RUST_LOG`). The filter syntax is the same as `RUST_LOG`: comma-separated `target=level` pairs, or a
bare level such as `"debug"` to match everything. Useful targets:

| Target            | What it covers                                             |
| ----------------- | ---------------------------------------------------------- |
| `infrastore_core` | All store operations — `add`, `get`, `remove` and HDF5 I/O |
