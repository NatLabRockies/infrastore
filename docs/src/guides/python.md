# Python Developer Guide

This guide covers building on the `infrastore` PyO3 module, from installing the wheel to the calls a
consumer package makes. For exact signatures and return shapes, see the
[Python API reference](../reference/python-api.md).

For complete programs rather than isolated snippets, use the repository's
[runnable Python examples](https://github.com/NatLabRockies/infrastore/tree/main/examples/python).
They cover static, non-sequential, deterministic, probabilistic, and scenario data; fixed tuples;
every function-valued element type; feature-based selection; and conversion to Polars data frames.

## Install

Python 3.11 or newer. The wheels are prebuilt and statically linked, so a consumer package such as
infrasys needs nothing else:

```sh
pip install infrastore
```

The wheel is built against the **`abi3-py311`** stable ABI, so one wheel works on CPython 3.11 and
every newer 3.x without recompiling.

`to_arrow()` needs pyarrow, which is not installed by default — it is several times the size of
everything else here, and nothing but that one method uses it:

```sh
pip install 'infrastore[arrow]'
```

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
`NonSequentialTimeSeries`, and `PersistentTimeSeries`; the forecast classes `Deterministic`,
`Probabilistic`, and `Scenarios`; the readers `StaticReader` and `ForecastReader`; the association
records `SupplementalAttributeAssociation` and `ParentChildAssociation`; the `TimeSeriesType` and
`OwnerCategory` enums; the `init_tracing`, `encode_element_values`, and `decode_element_values`
functions; `__version__`; and an exception hierarchy rooted at `TimeSeriesError`.

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
`NonSequentialTimeSeries(timestamps, data, name)` for explicitly timestamped series, and
`PersistentTimeSeries(timestamps, data, name)` for a sparse step function whose value holds forward
between breakpoints (worked through in [Step Functions](#step-functions-persistenttimeseries)).

## Add a Series

```python
series_id = store.add_time_series(
    owner_id=42,
    owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,   # name and descriptors come from ts
    features={"model_year": 2030, "scenario": "high"},
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

Beyond `units`, a series can carry `quantity_kind` (what the values measure — `"ActivePower"`; the
one record of what per-unit values mean), `unit_system` (`"natural_units"` or `"component_base"`;
unset means _unspecified_, not natural units), `component_field` (the field on the owning component
these values vary — `"max_active_power"`; also a filter), and `application_data` (an opaque string
the store returns verbatim — the package-owned slot). All of them are set **on the series object**,
not on the add, and each is a read-only property there:

```python
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc), timedelta(hours=1), values, "load",
    units="MW", quantity_kind="ActivePower", unit_system="natural_units",
    component_field="max_active_power",
    application_data='{"source": "weather_year_2012"}',
)
series_id = store.add_time_series(
    owner_id=42, owner_type="Generator", owner_category=OwnerCategory.Component,
    time_series=ts,
)
assert store.read_by_id(series_id).quantity_kind == "ActivePower"
```

Keeping them on the object is what makes a read-then-add lossless: a series read from one store can
be added to another unchanged, with no descriptor to re-supply and none the write could silently
replace.

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
commits them in one catalog transaction, taking the block-sized HDF5 write path, so same-shaped
series land in the same packed dataset.

An item carries exactly the keys `add_time_series` takes as parameters — `owner_id`, `owner_type`,
`owner_category`, `time_series`, and optionally `features` — and any other key raises, as the
misspelled keyword it almost always is. Everything that _describes_ the values (`units`,
`quantity_kind`, `unit_system`, `component_field`, `application_data`, `element_type`,
`time_reference`) rides on the `time_series` object, exactly as it does on the single-series path.

```python
ids = store.add_time_series_bulk([
    {"owner_id": i, "owner_type": "Generator", "owner_category": OwnerCategory.Component,
     "time_series": series[i]}
    for i in range(len(series))
])   # one catalog id per item, in input order; all-or-nothing
```

This is an order of magnitude faster than a bare loop of single adds, which pays one catalog
transaction and one HDF5 flush per series. It is **not** faster than that same loop inside a
[transaction](#transactions), which buffers and writes the identical datasets. What separates the
two is one thing: this call writes the batch as a single block whatever its size, because you are
already holding it, where the transaction holds its buffered adds to
[`write_buffer_bytes`](#how-wide-a-dataset-a-span-writes) and spills past it. Reach for this when
the whole cohort is in hand as a list; reach for the loop when you would rather build the series one
at a time than materialize every one of them first, and raise the budget if you want the single
dataset back.

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
outermost one ends. `begin_transaction` / `commit_transaction` / `rollback_transaction` are the
explicit form.

A transaction is also where a **run of single adds** belongs. Nothing it writes is durable until the
outermost commit, so packed adds inside one are buffered per shape group and written as one block at
the commit — the datasets `add_time_series_bulk` of the same series would produce, without having to
hold the batch yourself. Two things qualify it. The buffer **spills a block early** — an extra
dataset, nothing else, the same split a batch that wide gets — once a group reaches the narrower of
two widths, or once the unwritten arrays across every group cross 128 MiB:

- the columns one chunk row holds: 131,072 for a scalar `float64`;
- 128 MiB divided by one column's bytes, which for anything but a short series is the ceiling that
  actually binds — a 30,500-step `float64` series is 244 KB a column, so its group spills at 550.

And a span holding a **single** array fills a shared-pool slot rather than claiming a dataset one
column wide.

#### How wide a dataset a span writes

The 128 MiB is a default, not a law. `write_buffer_bytes` sets it, and through it how wide a dataset
a run of single adds can produce:

```python
store.write_buffer_bytes = 1 << 30      # 1 GiB
with store.transaction():
    for s in series:
        store.add_time_series(owner_id=..., owner_type="Generator",
                              owner_category=OwnerCategory.Component, time_series=s)
# one dataset, however many series that was
```

Raised far enough, the loop writes exactly what `add_time_series_bulk` of the same series writes —
and the memory it costs is the memory the bulk call's caller was holding anyway. Measured on 2,000
hourly year-long `float64` series: at the default they are ~0.26 s as one bulk call against ~0.31 s
as a loop, one `(8760, 2000)` dataset against a `(8760, 1915)` and a `(8760, 85)`; raise the budget
and the loop lands the single dataset too.

The figure belongs to the `Store` object, not to the artifact — nothing is persisted, and a store
reopened elsewhere is back to 128 MiB. It is a budget for the writing process, so a machine that
cannot afford another machine's choice does not inherit it. Lowering it mid-transaction writes out
whatever the buffer already holds beyond the new figure; zero raises `InvalidParameterError`, since
a pool's width floors at one column and a zero budget would mean a dataset per array rather than no
buffering at all. The chunk-row ceiling above it does not move.

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

### As a table

A read hands back the values as a numpy array with the timeline beside it, not fused into it. When
you want the two together — to plot, to write Parquet, to hand to pandas or polars — `to_arrow()`
builds a two-column `pyarrow.Table` of `timestamp` and `value`:

```python
table = store.read_by_id(series_id).to_arrow()
table.to_pandas()
```

It works on all three static types, and needs the [`arrow` extra](#install). The timestamp column is
typed in the series' own spelling — `timestamp[ms, tz=America/Denver]` for a zoned series, an
unzoned `timestamp[ms]` for a zoneless one — and the descriptive attributes ride in
`table.schema.metadata`. A `SingleTimeSeries` grid is materialized calendar-aware, so a monthly
series lands on month ends rather than on a multiple of 30 days. See
[`to_arrow()`](../reference/python-api.md#to_arrow).

Without pyarrow, `timestamps` is the same timeline as a plain list of datetimes:

```python
got = store.read_by_id(series_id)
list(zip(got.timestamps, got.data))
```

A `Deterministic` converts to **one table per window** instead, keyed by issue time:

```python
windows = store.read_by_id(forecast_id).to_arrow_windows()
windows[datetime(2024, 1, 2, tzinfo=timezone.utc)]   # that window's forecast
```

Each value looks exactly like a static series' table. It is a dict rather than one table because a
forecast has two grids that overlap — windows step by `interval`, rows inside a window by
`resolution` — so a day-ahead forecast reissued hourly shares 23 of every 24 instants between
neighbouring windows. The dict iterates chronologically. See
[`to_arrow_windows()`](../reference/python-api.md#to_arrow_windows).

### Back from a table

`from_arrow` is the inverse, on the same three types — and it reads a table anything wrote, not just
one `to_arrow()` produced:

```python
import pyarrow.parquet as pq

series = SingleTimeSeries.from_arrow(pq.read_table("load.parquet"))
store.add_time_series(42, "Generator", OwnerCategory.Component, series)
```

A table `to_arrow()` wrote round-trips with no arguments, because the metadata is the descriptor. A
**foreign** table — from a dataframe, or a Parquet file someone else wrote — needs a `name=` at
minimum, since a name is part of a series' identity; everything else is inferred from the Arrow
schema. Note what a table does not carry: the owner and the catalog id, because `to_arrow()` is a
method on a value object and a series built here is not filed anywhere. You supply the owner to
`add_time_series`, as you would for any other series.

The inference rules and the four things that are refused rather than coerced (nulls, sub-millisecond
timestamps, rows that leave a declared grid, decoded `struct`/`list` value columns) are in the
[reference](../reference/python-api.md#from_arrow).

`to_arrow()` and `from_arrow()` are **per-series, in-memory conveniences** -- one series, one table.
They are not the CLI's file format: `infrastore export -f parquet` writes
[a values/series file pair per partition](../reference/parquet-format.md), each distinct array once
and one catalog row per series, which `from_arrow` does not read. To move a whole store through
Parquet, use the CLI at both ends.

### Datetimes and precision

Every `datetime` must be timezone-aware (any zone; converted to UTC on the way in, UTC on the way
out), and a naive one raises `InvalidParameterError`. A **stored** instant — an initial timestamp, a
`NonSequentialTimeSeries` timestamp or `PersistentTimeSeries` breakpoint — must also be a whole
number of milliseconds, so quantize `datetime.now(timezone.utc)` before storing it; query bounds
such as `time_range` are unconstrained. See [Datetimes](../reference/python-api.md#datetimes).

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

#### When the series do not share a grid

Most real systems do not meet that requirement — a year of load beside a week of an outage schedule,
or one component logged from an hour later than the rest — and the build then raises, naming the
series that diverges. Give the reader a span instead of letting it inherit one:

```python
reader = store.build_static_reader(
    timedelta(hours=1),
    window_start=datetime(2024, 1, 1, 7, tzinfo=timezone.utc),
    window_length=8760,   # optional: without it, as far as *every* matched series reaches
)
```

Each column then reads at an offset of its own, so ragged series sweep together. The span is
checked, not clamped: a matched series that does not cover it raises `InvalidParameterError` naming
that series rather than dropping its column, and the anchor must land on each series' own step
boundaries. See [reader windows](../reference/python-api.md#reader-windows).

Sometimes the odd series out should not take part at all — a stray day of data beside a year of it
is a different component, not a shorter view of the same sweep. Then filter to one grid instead,
with `initial_timestamp` and `length`:

```python
reader = store.build_static_reader(
    timedelta(hours=1),
    initial_timestamp=datetime(2024, 1, 1, 7, tzinfo=timezone.utc),
    length=8784,
)
```

The window sweeps a span across whatever matched; the filter matches only the series already on that
grid. `static_summary()` shows which grids a store holds, and the filter reaches every other
filter-taking call too — `list_metadata`, `remove_by_filter`, and the rest. See
[selecting one grid](../reference/python-api.md#selecting-one-grid).

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

## Step Functions (`PersistentTimeSeries`)

A `PersistentTimeSeries` is a sparse **step function**: a strictly increasing vector of
_breakpoints_ plus one value each, where the value at an arbitrary instant is the one belonging to
the greatest breakpoint at or before it. The motivating data is a monthly fuel or gas price curve —
a dozen breakpoints a simulation reads at timestamps that almost never land on one. The same curve
stored as a `NonSequentialTimeSeries` would raise at nearly every step, because an irregular series
has **no** value between its timestamps; that difference in read semantics is the whole reason this
is a separate type. See
[Time series types](../explanation/time-series-types.md#persistenttimeseries) for the model.

### Build and add

Construction is identical to `NonSequentialTimeSeries` — same arguments, same validation, same
spelling inference from `tzinfo`:

```python
from infrastore import PersistentTimeSeries

breakpoints = [datetime(2024, m, 1, tzinfo=timezone.utc) for m in (1, 4, 7, 10)]
prices = PersistentTimeSeries(
    breakpoints,
    np.array([3.5, 4.25, 5.0, 4.75]),
    "gas_price",
    units="USD/MMBtu",
    component_field="fuel_cost",
    # Whether a curve is expanded to a full series or collapsed to one scalar is
    # your application's policy, and rides here where the store never reads it.
    application_data='{"as_time_series": false, "force_scalar_mode": "midpoint"}',
)

price_id = store.add_time_series(
    owner_id=7,
    owner_type="ThermalStandard",
    owner_category=OwnerCategory.Component,
    time_series=prices,
)
```

`read_by_id(price_id)` hands back a `PersistentTimeSeries` whose `timestamps` are the breakpoints
and whose `data` holds one value each — the same dtype and shape rules as every other static series,
multi-dimensional per-breakpoint values included.

### Read a window

A range read slices on the step function's own terms: the result begins at the breakpoint _in force
at_ `start`, so it always defines a value at the start of the window you asked for.

```python
(window,) = store.read_by_ids_range(
    [price_id],
    (datetime(2024, 4, 10, tzinfo=timezone.utc), datetime(2024, 9, 1, tzinfo=timezone.utc)),
)
window.timestamps   # [2024-04-01, 2024-07-01] — the April step, not the first one inside the window
window.data         # array([4.25, 5.  ])
```

Past the last breakpoint the last value holds forever, so a window opening after the end comes back
with that one row. **Before** the first breakpoint a step function is undefined: a non-empty window
starting there raises `InvalidParameterError` rather than clamping. (A zero-width range,
`end == start`, selects nothing — here as for every type.) `read_by_id`'s `start_time` + `len`
window is _checked_ rather than sliced, so it must name one of the breakpoints; reach for
`read_by_ids_range` when the instant is arbitrary.

### Sweep step functions in the simulation loop

A `StaticReader` filtered to the type is the per-timestamp path, and it is the one place the
[one-timeline-per-reader](../explanation/readers.md#one-timeline-per-reader) rule bends: a step
function has a value at every instant from its own first breakpoint on, so the columns need **not**
share a breakpoint vector. Per-fuel curves whose breakpoints do not line up still build one reader.

```python
reader = store.build_static_reader(
    time_series_type=TimeSeriesType.PersistentTimeSeries,   # no resolution — passing one raises
    component_field="fuel_cost",
)
grid = reader.grid()                   # grid["resolution"] is None: a step function has no step
groups = reader.groups()
for at in reader.timestamps():         # the sorted union of every column's breakpoints
    store.static_read(reader, at)
    for i, g in enumerate(groups):
        vals = reader.group_values(i)  # the value in force at `at`; column j ↔ g["ids"][j]
```

`timestamps()` is the **union** of the columns' breakpoints, so a position on it is not a storage
row for any one column — each column independently reports the value in force there. There is still
no presence mask: reading at an instant before some column's first breakpoint raises
`InvalidParameterError` naming that column's association id. Either filter the reader down to
columns that start early enough, or begin the sweep at the latest first breakpoint among them.

The sweep need not follow that union axis at all. `static_read` accepts any instant every column
defines a value at, so driving this reader at your `SingleTimeSeries` grid's timestamps — the
simulation's own clock — works and is usually what an application wants:

```python
for at in load_reader.timestamps():        # the hourly grid the simulation runs on
    store.static_read(load_reader, at)
    store.static_read(reader, at)          # each fuel price, held forward to this hour
```

### These rows do not travel in an OpenAPI document

`PersistentTimeSeries` is an infrastore-local extension, and the vendored wire contract is a `oneOf`
over six canonical Sienna types with no schema for a seventh. So
`export_time_series_associations_openapi` **omits** persistent rows — a mixed store still exports
its six-type rows — a filter naming the type raises `InvalidParameterError` rather than answering
with an empty array, and an import refuses a document that carries one. The series themselves are
unaffected: they live in the artifact, which holds them in full. Ask the catalog what a document
leaves behind:

```python
left_behind = store.list_metadata(time_series_type=TimeSeriesType.PersistentTimeSeries)
```

## Custom Element Types

By default an array's elements are plain numbers of its dtype. An `element_type` says otherwise:
what the trailing per-step dimension of the array actually _means_. It is metadata, not a different
storage format — the array is still the same typed HDF5 dataset — but it is what lets a reader turn
the raw floats back into the values you meant. See [Element types](../reference/element-types.md)
for the full grammar and the byte layout each kind produces; this section works through each one
from Python.

### `from_values` and `decoded_values`

Every series type has a `from_values` classmethod that takes the values themselves. It encodes them
and records the element type they imply, so the two cannot disagree:

```python
curves = [
    [{"x": 0.0, "y": 1.0}, {"x": 1.0, "y": 3.0}],
    [{"x": 0.0, "y": 2.0}],
]
ts = SingleTimeSeries.from_values(
    datetime(2024, 1, 1, tzinfo=timezone.utc), timedelta(hours=1), curves, "cost_curve",
)
assert ts.element_type == "piecewise_linear"     # nobody declared it

series_id = store.add_time_series(
    owner_id=42, owner_type="Generator", owner_category=OwnerCategory.Component, time_series=ts,
)
assert store.read_by_id(series_id).decoded_values() == curves
```

`decoded_values()` is the read-side half: the element type and the number of leading axes both come
off the series, so there is nothing left to pass. It returns `None` for a scalar element type and
for any array whose dtype is not `float64` — there the stored elements already are the values, and
`.data` is the answer.

Which element type a payload implies is read off the shape of a row; the five shapes are disjoint:

| `values` entry                                       | element type         |
| ---------------------------------------------------- | -------------------- |
| `{"proportional": …, "constant": …}`                 | `linear_function`    |
| `{"quadratic": …, "proportional": …, "constant": …}` | `quadratic_function` |
| `list[{"x": …, "y": …}]`                             | `piecewise_linear`   |
| `{"x": list, "y": list}`                             | `piecewise_step`     |
| `list[float]` of length `N`                          | `tuple(N,f64)`       |

`element_type=` is still accepted on `from_values`, as an assertion rather than an override: it
raises `InvalidParameterError` if it disagrees with the values. Where the values name nothing it is
the only thing to go on — an empty `values`, or rows that are all empty and read equally as a curve
with no points or a tuple with no fields.

Underneath sit `encode_element_values(values, element_type, leading_dims)` and
`decode_element_values(array, element_type, leading_dims)`, which the rest of this section uses to
show what each element type packs into. Reach for them directly when there is no series to hang the
values on — decoding an array that arrived on its own — or for the one series `from_values` cannot
name: an empty `tuple(N,f64)`, whose arity lives in rows it does not have.

### Composite values: `tuple(N,dtype)`

A `tuple(N,dtype)` is bytes-identical to a plain array shaped `(length, N)` — declaring it changes
nothing about what is stored, only how a reader should group the trailing `N` values: as one
composite value (three cost-curve coefficients), not `N` independent samples. Build it like any
other multi-dimensional series and declare the type on the constructor:

```python
coeffs = np.array([[1.0, 0.5, 12.0], [1.1, 0.4, 11.5]])  # (length=2, N=3)
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc), timedelta(hours=1), coeffs, "cost_coeffs",
    element_type="tuple(3,f64)",
)
```

This is the one element type where the constructor stays the natural call: there is no packing to
build, so an array you already hold in numpy needs no encoding step. `from_values` accepts the same
values as a list of `list[float]` rows, and is the better fit when that is the shape you have.

`decoded_values()` and `decode_element_values` only unpack `f64` arrays — for any other dtype
(`tuple(3,i32)`, say) they return `None`, because there is nothing to unpack: the stored rows
already are the tuples, and you read them straight off `.data`. `encode_element_values` is a
convenience for the `f64` case only; it also always builds `f64`, so neither it nor `from_values`
can produce a tuple of any other dtype.

### Fixed-width coefficients: `linear_function`, `quadratic_function`

These give every timestep a small, fixed number of function coefficients — a proportional and a
constant term for a line, plus a quadratic term for a parabola — packed as `f64` regardless of the
rest of the series. The values are a list of per-timestep dicts, matching keys to the function's
coefficients:

```python
from infrastore import encode_element_values, decode_element_values

curves = [
    {"proportional": 1.0, "constant": 2.0},
    {"proportional": 1.2, "constant": 1.8},
]
array = encode_element_values(curves, "linear_function")   # shape (2, 2), f64
assert decode_element_values(array, "linear_function") == curves
```

Or, without naming the type or holding the array at all:

```python
ts = SingleTimeSeries.from_values(
    datetime(2024, 1, 1, tzinfo=timezone.utc), timedelta(hours=1), curves, "marginal_cost",
)
series_id = store.add_time_series(
    owner_id=7, owner_type="ThermalStandard", owner_category=OwnerCategory.Component,
    time_series=ts,
)
assert store.read_by_id(series_id).decoded_values() == curves
```

`quadratic_function` is the same shape, with a `"quadratic"` key added and row width `3` instead of
`2`. Both raise if a row doesn't match the required width exactly — there is no padding for these
two, because every timestep genuinely has the same number of coefficients.

### Ragged curves: `piecewise_linear`, `piecewise_step`

These are the element types built for a **variable** number of points per timestep — a case covered
end to end in the runnable
[`single_custom_elements.py`](https://github.com/NatLabRockies/infrastore/blob/main/examples/python/single_custom_elements.py)
example. `encode_element_values` finds the widest row across the whole array, then packs every
timestep as a leading count `n` followed by its points, zero-padded out to that common width;
decoding reads `n` back off each row and returns exactly that many points, ignoring the padding.

A `piecewise_linear` timestep is a list of `{"x", "y"}` knots:

```python
curves = [
    [{"x": 0.0, "y": 1.0}, {"x": 1.0, "y": 3.0}, {"x": 2.0, "y": 5.0}],                      # 3 points
    [{"x": 0.0, "y": 2.0}, {"x": 1.0, "y": 4.0}, {"x": 2.0, "y": 6.0}, {"x": 3.0, "y": 8.0}], # 4 points
]
ts = SingleTimeSeries.from_values(
    datetime(2024, 1, 1, tzinfo=timezone.utc), timedelta(hours=1), curves, "cost_curve",
)
assert ts.data.shape == (2, 1 + 2 * 4)   # widest row wins; the 3-point row is padded
series_id = store.add_time_series(
    owner_id=42, owner_type="Generator", owner_category=OwnerCategory.Component, time_series=ts,
)

back = store.read_by_id(series_id)
assert back.decoded_values() == curves   # 3- and 4-point rows both exact
```

A `piecewise_step` timestep decodes to a **different** shape — one dict of parallel arrays rather
than a list of points, since a step function has one fewer `y` than `x`: each `y` is the value
_between_ two adjacent `x`'s, so `n` coordinates bound `n - 1` steps and the last coordinate is the
right-hand end of the curve rather than the start of an open final step. Nothing is held forward
past it — that is a `PersistentTimeSeries`, which is a time series type rather than an element type:

```python
steps = [
    {"x": [0.0, 1.0, 2.5], "y": [10.0, 20.0]},   # 3 x's, 2 steps
    {"x": [0.0, 5.0], "y": [7.5]},               # 2 x's, 1 step
]
array = encode_element_values(steps, "piecewise_step")
```

The padded width is fixed by whatever `encode_element_values` saw in that one call. Add a timestep
with more points later than any seen so far, and the wider row width makes it a different packed
HDF5 dataset (`element_shape` differs), not an in-place resize of the array you already wrote — the
same packing rule every other same-shaped-series pooling in this store follows.

Forecasts (`Deterministic`, `Probabilistic`, `Scenarios`) use these same element types over their
extra leading axes. This is where `from_values` saves the most: the leading dimensions come from
arguments the forecast constructor already takes, so nothing is computed by hand.

```python
# H = horizon / resolution = 2, so [H, count] = [2, 2] wants four curves, entry
# `i * count + j` being window `j`'s step `i`.
forecast = Deterministic.from_values(
    start, timedelta(hours=1), timedelta(hours=2), timedelta(hours=1), 2, curves * 2,
    "cost_curve",
)
assert forecast.data.shape == (2, 2, 9)
```

`Scenarios.from_values` takes `scenario_count` explicitly, where its constructor reads it off the
array's first axis — there is no array yet to read it from. Going through `encode_element_values`
instead means passing `leading_dims=[horizon, count]` (or `[percentiles, horizon, count]` /
`[scenarios, horizon, count]`) yourself, in place of the default single-axis case. See
[Forecasts](#forecasts) above for the shapes those types read back as.

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

### What is in here?

Before querying anything in particular, `show()` prints the shape of the whole store — the
time-series associations by type, the arrays behind them, the owners, and both association catalogs:

```python
store.show()
# Store: system.h5 (read-write)
# Time series: 128 associations over 128 distinct arrays
#   SingleTimeSeries      100
#   PersistentTimeSeries    8
#   Deterministic          20
# Owners with time series: 108 components, 0 supplemental attributes
# Supplemental attribute attachments: 12
# Parent/child edges: 5
```

It is aggregate catalog queries only, so it stays fast on a large store, and it takes `file=` like
`print` does. For the numbers themselves rather than the rendering, use `counts_by_type()`,
`time_series_counts_detailed()`, `num_distinct_arrays()`, and the `count_*` methods — see
[`show()`](../reference/python-api.md#show).

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
    units="MW",
)
series_id = store.add_time_series(
    owner_id=42, owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,
    features={"model_year": 2030},
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
