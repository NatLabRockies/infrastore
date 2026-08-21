# Python API

The PyO3 binding is importable as the `infrastore` module (package `infrastore`). It is built as an
`abi3-py311` wheel, so one build runs on CPython 3.11 and newer.

```python
from infrastore import (
    Store, SingleTimeSeries, NonSequentialTimeSeries, TimeSeriesKey,
    Deterministic, Probabilistic, Scenarios,
    TimeSeriesType, OwnerCategory,
    SupplementalAttributeAssociation, ParentChildAssociation,
    TimeSeriesError, NotFoundError, DuplicateTimeSeriesError,
    DuplicateAssociationError, InvalidParameterError, IntegrityError, ReadOnlyStoreError,
)
```

`infrastore.__version__` reports the wheel version.

> **Array dtypes.** The binding accepts and returns NumPy arrays of `float64`, `float32`, the signed
> and unsigned integer widths (`int64`/`int32`/`int16`/`int8`/`uint64`/`uint32`/`uint16`/`uint8`),
> or `bool`; whatever dtype is given round-trips unchanged. What those elements _mean_ is the
> association's `element_type` (see [Element types](./element-types.md)), declared with the
> `element_type=` keyword on `add_time_series` and decoded with `decode_element_values`.
> Multi-dimensional arrays (a per-step element shape) are supported via the NumPy array's shape.

## Datetimes

Every `datetime` argument — an initial timestamp, a `NonSequentialTimeSeries` timestamp vector, a
`time_range` bound, a reader's `when` — must be **timezone-aware**, and any zone will do:
`datetime.timezone.utc`, a `ZoneInfo`, or a fixed offset. It is converted to UTC on the way in, so
two aware datetimes naming the same instant are the same instant to the store, and every `datetime`
read back is UTC. A naive datetime names no instant and raises `InvalidParameterError`.

A `datetime` that is **stored** — an initial timestamp, or an entry of a `NonSequentialTimeSeries`
timestamp vector — must also be a whole number of milliseconds; `microsecond` must be a multiple of
1000. A finer instant raises `InvalidParameterError` rather than being silently truncated, because
it cannot survive every binding intact (see
[timestamp precision](../explanation/data-model.md#timestamp-precision)). Note that
`datetime.now(timezone.utc)` carries microseconds: quantize it, e.g.
`now.replace(microsecond=now.microsecond // 1000 * 1000)`. A `datetime` used only as a _query_ bound
— a `time_range` end, a reader's `when` — is unconstrained.

## `Store`

### Constructors

```python
@classmethod
def create(
    cls,
    path: str | None = None,
    in_memory: bool = False,
    compression: str = "deflate",   # "deflate" or "none"
    compression_level: int = 3,     # 0–9, DEFLATE only
    shuffle: bool = True,           # byte-shuffle filter, DEFLATE only
    catalog: str | None = None,     # "attached" or "memory"; None matches the backend
    overwrite: bool = False,        # discard an artifact already at `path`
) -> Store: ...

@classmethod
def open(
    cls, path: str, read_only: bool = False, catalog: str = "attached"
) -> Store: ...

@classmethod
def open_copy(cls, src: str, dest: str, catalog: str = "attached") -> Store: ...
```

- `create(in_memory=True)` — in-memory store; `path` and compression arguments are ignored.
- `create(path=...)` — writes `path` (HDF5) and `path + ".sqlite"` (metadata).
- `create(path=..., compression="none")` — store arrays uncompressed; `compression="deflate"` with a
  `compression_level` / `shuffle` of your choice tunes the filter. The policy persists with the
  store and is reused on later appends. An unknown `compression` or out-of-range level raises
  `InvalidParameterError`.
- `catalog="attached"` makes the catalog the `.sqlite` file, where every commit is durable;
  `catalog="memory"` holds it in RAM so it reaches disk only through `persist_to()`. Arrays stream
  to the HDF5 file either way. The default (`None`) matches the backend — `"memory"` when
  `in_memory=True`, else `"attached"` — so existing call sites are unchanged. An unknown `catalog`
  raises `InvalidParameterError`. See
  [Where the Catalog Lives](../explanation/storage-model.md#where-the-catalog-lives).
- `create(path=...)` raises `StoreExistsError` if `path` or `path + ".sqlite"` already holds a
  store. Creating there would discard the arrays while keeping the catalog, leaving a store that
  reopens cleanly with every array missing — see
  [protecting a saved artifact](../explanation/storage-model.md#protecting-a-saved-artifact).
  `overwrite=True` discards both halves on purpose; it is rejected for `in_memory=True`, which has
  no artifact to replace.
- `open(path, read_only=True)` — read-only open; writes raise `ReadOnlyStoreError`.
- `open(path, catalog="memory")` — loads the catalog into RAM; the HDF5 half is still opened in
  place. `store.catalog` reports the mode.
- `open_copy(src, dest)` — copies both halves to `dest` and opens the copy read-write, leaving `src`
  untouched. **This is the safe way to load a store you intend to change.** `open()` defaults to
  read-write, and mutations then land in that file directly; HDF5 has no journal and no repair tool,
  so an interrupted write is unrecoverable. Change the copy and `persist_to(src)` — one atomic
  rename replaces the original. Raises `StoreExistsError` if `dest` already holds a store.

The store is also a context manager: `with Store.create(...) as store:` closes it on exit.
`store.close()` drops the underlying handle and releases its files; subsequent operations raise
`TimeSeriesError` (it is idempotent). `repr(store)` shows the path (or `in-memory`), the read-only
flag, and `closed` once closed.

### Property

```python
store.read_only -> bool
```

### Methods

```python
def add_time_series(
    self,
    owner_id: int,
    owner_type: str,
    owner_category: OwnerCategory,
    time_series: SingleTimeSeries | NonSequentialTimeSeries
        | Deterministic | Probabilistic | Scenarios,
    features: dict[str, int | float | bool | str] | None = None,
    units: str | None = None,
    element_type: str | None = None,
    application_data: str | None = None,
    quantity_kind: str | None = None,
    unit_system: str | None = None,   # "natural_units" | "component_base"
    component_field: str | None = None,  # e.g. "max_active_power"
) -> TimeSeriesKey: ...
# An unrecognized `unit_system` raises InvalidParameterError rather than
# degrading to unspecified; omitting it leaves the basis unspecified, which is
# not the same as declaring natural units.
# `name` comes from the time_series object
# (e.g. SingleTimeSeries(..., name=...)), not from this call.
# A `features` key that shadows a time-series or key field (`name`, `resolution`,
# `owner_id`, ...) raises InvalidParameterError.

def add_time_series_bulk(self, items: list[dict]) -> list[TimeSeriesKey]: ...
# Each item dict mirrors add_time_series's parameters: required `owner_id`,
# `owner_type`, `owner_category`, `time_series`; optional `features`, `units`,
# `element_type`, `application_data`, `quantity_kind`, `unit_system`,
# `component_field`.
# All items commit in ONE metadata transaction (all-or-nothing), which is much
# faster than looping over add_time_series. Keys are returned in input order.

def transform_single_time_series(self, horizon: timedelta | str, interval: timedelta | str) -> int: ...

def get_time_series(
    self,
    key: TimeSeriesKey,
    time_range: tuple[datetime, datetime] | None = None,
) -> SingleTimeSeries | NonSequentialTimeSeries | Deterministic | Probabilistic | Scenarios: ...

def bulk_read(
    self,
    keys: list[TimeSeriesKey],
    *,
    time_range: tuple[datetime, datetime] | None = None,
) -> list[SingleTimeSeries | NonSequentialTimeSeries | Deterministic | Probabilistic | Scenarios]: ...
# `time_range` applies the same window to every key (default: each series in full).
# Results are returned in the same order as `keys`; an empty list of keys returns an empty list.

def remove_time_series(self, key: TimeSeriesKey) -> None: ...
def clear_time_series(
    self,
    owner_id: int | None = None,
    owner_category: OwnerCategory | None = None,
) -> int: ...
# Pass both owner_id and owner_category to clear one owner's series (the owner is
# the (owner_id, owner_category) pair); pass neither to clear the whole store.

def replace_owner(
    self,
    old_owner: int,
    new_owner: int,
    owner_category: OwnerCategory,
) -> int: ...
# Reassign every series owned by (old_owner, owner_category) to
# (new_owner, owner_category). Returns the number of associations moved.

def list_time_series(
    self,
    *,
    owner_id: int | None = None,
    owner_category: OwnerCategory | None = None,
    owner_type: str | None = None,
    time_series_type: TimeSeriesType | str | None = None,
    name: str | None = None,
    name_glob: str | None = None,   # SQLite GLOB pattern; ANDed with `name`
    component_field: str | None = None,  # exact, case-sensitive
    resolution: timedelta | str | None = None,
    interval: timedelta | str | None = None,
    features: dict[str, int | float | bool | str] | None = None,
) -> list[dict]: ...
# `component_field` selects every series that varies that field on its owner. A
# series that declares none matches no value, so it cannot select those rows.

def list_array_groups(self, *, ...) -> list[dict]: ...
# Same keyword-only filter arguments as list_time_series; so do list_keys,
# list_names, list_owner_types, and remove_by_filter.
# `time_series_type` is a TimeSeriesType. TimeSeriesType.Deterministic matches
# both Deterministic and DeterministicSingleTimeSeries rows. Every filter
# surface takes it, including has_any_time_series, get_resolutions,
# get_intervals, list_owner_ids, and build_forecast_reader.

def get_time_series_keys(
    self,
    owner_id: int,
    owner_category: OwnerCategory,
) -> list[TimeSeriesKey]: ...
def has_time_series(self, key: TimeSeriesKey) -> bool: ...
def has_any_time_series(self, *, ...) -> bool: ...
# Existence without listing ("does this owner have any time series?"); same
# keyword-only filter arguments as list_time_series. Index-probe fast.
def get_resolutions(self, time_series_type: TimeSeriesType | None = None) -> list[str]: ...
# resolutions are returned as ISO 8601 duration strings, e.g. "PT1H"
def get_time_series_counts(self) -> dict: ...
def get_forecast_parameters(self, *, resolution: str | None = None,
                            interval: str | None = None) -> dict: ...
def get_compression(self) -> dict: ...
def compact(self) -> dict: ...
def verify_integrity(self) -> dict: ...
# {"ok": bool, "errors": list[str]}
def flush(self) -> None: ...
def persist_to(self, path: str) -> None: ...
def persist_catalog(self) -> None: ...
# Writes an in-memory catalog to this store's own <path>.sqlite, stamped to
# match the HDF5 file already beside it. Unlike persist_to, copies no arrays:
# they are already in place. A checkpoint, not a mode switch — the catalog
# stays in RAM. For catalog="attached" this is flush().

# -- transactions --
# Span several operations so they all take effect or none do. Removals are
# reversible only inside a transaction. Blocks nest; the write lock is held
# until the outermost one ends.
def transaction(self) -> Transaction: ...   # context manager: commit on exit, roll back on raise
def begin_transaction(self) -> None: ...
def commit_transaction(self) -> None: ...   # InvalidParameterError if none is open
def rollback_transaction(self) -> None: ... # InvalidParameterError if none is open
in_transaction: bool                        # property
```

```python
with store.transaction():
    store.add_time_series(...)
    store.remove_time_series(old_key)
# both applied, or neither -- including the removal
```

> **Keyword-only arguments.** Every optional argument in the binding is keyword-only (the `*`
> marker): filter kwargs, `features=`/`units=`/`application_data=` on the add paths, `time_range=`
> on the read paths, and so on. Positional use raises `TypeError`. The wheel ships a
> `infrastore.pyi` stub, so IDEs and type checkers see the full signatures.

#### Return shapes

- **`add_time_series`** accepts a `SingleTimeSeries`, a `NonSequentialTimeSeries`, or a dense
  forecast object (`Deterministic` / `Probabilistic` / `Scenarios`) — see [Forecasts](#forecasts).
  **`transform_single_time_series`** derives a `DeterministicSingleTimeSeries` from every stored
  `SingleTimeSeries` and returns the count transformed. **`get_time_series`** returns whichever
  matches the stored type.
- **`bulk_read`** returns one typed object per key, in the same order as `keys` (an empty key list
  returns an empty list). It is the bulk counterpart to `get_time_series`: packed `SingleTimeSeries`
  are read in one decompress-once pass per dataset instead of one read per key. Pass the
  keyword-only `time_range=(start, end)` to apply the same window to every key; by default each
  series comes back in full.
- **`list_time_series`** returns a list of dicts, each with the keys: `owner_id`, `owner_type`,
  `owner_category`, `time_series_type`, `name`, `data_hash` (hex string), `length`, `resolution`
  (ISO 8601 duration string, e.g. `PT1H`, or `None`), `timestamps`, `features`, `units`,
  `quantity_kind`, `unit_system` (`"natural_units"` / `"component_base"` / `None`),
  `component_field`, `application_data`. `timestamps` is a list of RFC 3339 strings for
  non-sequential series and `None` otherwise. The `features` filter is a subset match — rows must
  contain at least the given pairs.
- **`list_array_groups`** accepts the same filters as `list_time_series` and groups the matching
  series by their underlying stored array. It returns a list of dicts, each with `data_hash` (hex
  string) and `keys` (a list of `TimeSeriesKey`s that resolve to that array). Keys sharing one dict
  share one deduplicated array.
- **`get_time_series_counts`** returns
  `{"components_with_time_series": int, "static_time_series": int, "forecasts": int}`.
- **`get_forecast_parameters`** returns
  `{"horizon": str, "interval": str, "count": int, "resolution": str, "initial_timestamp": str}`,
  where `horizon`, `interval`, and `resolution` are ISO 8601 duration strings (e.g. `"PT1H"`) and
  `initial_timestamp` is an RFC 3339 string. Every value is `None` when the store holds no
  forecasts. The keyword-only `resolution` / `interval` arguments scope the query to forecasts
  matching that grid.
- **`get_compression`** returns `{"compression": "deflate" | "none", "level": int, "shuffle": bool}`
  — the policy the store was created with (restored from the file on open; `"none"` for in-memory).
- **`compact`** returns
  `{"slots_reclaimed": int, "datasets_dropped": int, "feature_sets_reclaimed": int,
  "timestamp_sets_reclaimed": int, "bytes_reclaimed": int}`.
  `feature_sets_reclaimed` counts content-addressed feature rows that no association referenced any
  more; see the [file format](file-format.md#feature_sets). On an on-disk store the call rewrites
  the `.h5` file from the live set and replaces it, which is what makes `bytes_reclaimed` nonzero —
  nothing else may have the store open while it runs.
- **`verify_integrity`** returns `{"ok": bool, "errors": list[str]}`; `ok` is `True` when the error
  list is empty. It checks stored arrays against their recorded hashes and does not inspect the
  SQLite catalog, so `ok` is not a statement about the store as a whole — see
  [content addressing](../explanation/content-addressing.md#what-it-does-not-cover).
- **`get_time_series`** with `time_range=(start, end)` slices on the time axis; `end` is exclusive.

## `SingleTimeSeries`

```python
SingleTimeSeries(
    initial_timestamp: datetime,
    resolution: timedelta,
    data: numpy.ndarray,   # shape (length,) or (length, k1, ...)
    name: str,
)
```

Read-only properties: `initial_timestamp -> datetime`, `resolution -> str` (ISO 8601 duration, e.g.
`PT1H`), `length -> int`, `data -> numpy.ndarray`, `name -> str`. The constructor accepts either a
`timedelta` or an ISO 8601 duration string for `resolution`; the getter always returns the ISO
string. `name` is a required association attribute (the same array may be stored under different
names). It is read off the object by `add_time_series` and populated on `get_time_series`. The
array's `element_type` and per-step element shape are preserved through a round-trip.

## `NonSequentialTimeSeries`

```python
NonSequentialTimeSeries(
    timestamps: list[datetime],
    data: numpy.ndarray,
    name: str,
)
```

Read-only properties: `timestamps`, `length`, `data`, and `name`. Timestamps must be timezone-aware,
strictly increasing, and match the first data dimension. `get_time_series` returns this class for a
non-sequential key.

## `TimeSeriesKey`

Returned by `add_time_series`, `add_time_series_bulk`, and `get_time_series_keys`, and in the `keys`
list of every `list_array_groups` row; not constructed directly. Read-only properties:

```python
key.owner_id          -> int
key.owner_category    -> OwnerCategory
key.time_series_type  -> TimeSeriesType
key.name              -> str
key.resolution        -> str | None   # ISO 8601 duration, e.g. "PT1H"
key.interval          -> str | None   # ISO 8601 duration
key.features          -> dict[str, int | float | bool | str]
```

Feature names that would shadow one of these key fields, or a field of a time-series object, are
rejected when the series is added — see
[reserved feature names](../explanation/data-model.md#reserved-feature-names).

## Enums

```python
TimeSeriesType.SingleTimeSeries
TimeSeriesType.NonSequentialTimeSeries
TimeSeriesType.Deterministic
TimeSeriesType.DeterministicSingleTimeSeries
TimeSeriesType.Probabilistic
TimeSeriesType.Scenarios

OwnerCategory.Component
OwnerCategory.SupplementalAttribute
```

`TimeSeriesType` names a _stored_ type, and is also what a query asks for. Every member matches only
itself with one exception: **`TimeSeriesType.Deterministic` also matches a stored
`DeterministicSingleTimeSeries`**, which is what a caller asking "does this owner have a
deterministic forecast?" wants — whether the forecast was added densely or derived by
`transform_single_time_series` is a storage detail. Returned rows and keys still carry the concrete
stored type, and `TimeSeriesType.DeterministicSingleTimeSeries` narrows to the derived form for
callers auditing which forecasts are synthetic.

A member's **name** is accepted anywhere a `TimeSeriesType` is — `time_series_type="Deterministic"`
selects exactly what `TimeSeriesType.Deterministic` does. It is the spelling a metadata row reports,
so a value read out of one can be handed straight back. The match is case-sensitive: an unrecognized
string raises `InvalidParameterError` naming the valid ones, and a value that is neither a
`TimeSeriesType` nor a string raises `TypeError`.

## Forecasts

Dense forecasts are constructed as `Deterministic`, `Probabilistic`, or `Scenarios` objects and then
passed to [`add_time_series`](#methods). They are read back through `get_time_series`, which returns
the matching object depending on the stored type (a `DeterministicSingleTimeSeries` is synthesized
into a `Deterministic` on read). A `DeterministicSingleTimeSeries` is not added directly — derive
one from stored `SingleTimeSeries` with [`transform_single_time_series`](#methods).
[`get_time_series_counts`](#timeseriesstore) reports the forecast total under `forecasts`.

```python
ts = Deterministic(initial_timestamp, resolution, horizon, interval, count, data, "load_fc")
key = store.add_time_series(42, "Generator", OwnerCategory.Component, ts, units="MW")
```

`data` is a NumPy array in the canonical shape for the forecast type, where `H` is
`horizon / resolution`. As with `SingleTimeSeries`, every period argument (`resolution`, `horizon`,
`interval`) accepts either a `timedelta` or an ISO 8601 duration string — the string form is
required for calendar periods such as `"P1M"` — and the getters always return the ISO string. Every
forecast also takes a required `name` (after `data`), exposed as a read-only property:

| Type            | `data` shape                       | extra constructor arg                 |
| --------------- | ---------------------------------- | ------------------------------------- |
| `Deterministic` | `[H, count, *element_shape]`       | —                                     |
| `Probabilistic` | `[len(percentiles), H, count, *E]` | `percentiles`                         |
| `Scenarios`     | `[scenario_count, H, count, *E]`   | `scenario_count` is taken from `data` |

### `Deterministic`

```python
Deterministic(
    initial_timestamp: datetime,
    resolution: timedelta | str,
    horizon: timedelta | str,
    interval: timedelta | str,
    count: int,
    data: numpy.ndarray,
    name: str,
)
```

Read-only properties:

```python
forecast.initial_timestamp -> datetime
forecast.resolution        -> str   # ISO 8601 duration, e.g. "PT1H"
forecast.horizon           -> str   # ISO 8601 duration
forecast.interval          -> str   # ISO 8601 duration
forecast.count             -> int
forecast.data              -> numpy.ndarray
forecast.name              -> str
```

### `Probabilistic`

```python
Probabilistic(
    initial_timestamp: datetime,
    resolution: timedelta | str,
    horizon: timedelta | str,
    interval: timedelta | str,
    count: int,
    percentiles: list[float],
    data: numpy.ndarray,
    name: str,
)
```

Same properties as `Deterministic`, plus:

```python
forecast.percentiles -> list[float]
```

### `Scenarios`

```python
Scenarios(
    initial_timestamp: datetime,
    resolution: timedelta | str,
    horizon: timedelta | str,
    interval: timedelta | str,
    count: int,
    data: numpy.ndarray,   # leading axis is scenario_count
    name: str,
)
```

Same properties as `Deterministic`, plus:

```python
forecast.scenario_count -> int
```

## Readers

`get_time_series` returns one whole series or forecast. For the simulation access pattern — _walk
every timestamp and, at each, read the value of every matching series_ — use a **reader** instead. A
reader is built once over a filter, pins one timeline, and reuses its output buffers so a tight loop
allocates almost nothing. There are two: `StaticReader` for the static types, and `ForecastReader`
for forecasts. Both share the lifecycle: build → inspect the layout once → `*_read(when)` in a loop
→ pull values per group/entry.

The builders and drivers live on `Store`:

```python
def build_static_reader(
    self,
    resolution: timedelta | str | None = None,
    *,
    time_series_type: TimeSeriesType | None = None,   # default: SingleTimeSeries
    owner_id: int | None = None,
    owner_category: OwnerCategory | None = None,
    owner_type: str | None = None,
    name: str | None = None,
    name_glob: str | None = None,
    features: dict[str, int | float | bool | str] | None = None,
) -> StaticReader: ...
def static_read(self, reader: StaticReader, when: datetime) -> None: ...

def build_forecast_reader(
    self,
    time_series_type: TimeSeriesType,
    resolution: timedelta | str,
    *,
    owner_id: int | None = None,
    owner_category: OwnerCategory | None = None,
    owner_type: str | None = None,
    name: str | None = None,
    name_glob: str | None = None,
    features: dict[str, int | float | bool | str] | None = None,
) -> ForecastReader: ...
def forecast_read(self, reader: ForecastReader, when: datetime) -> None: ...
```

`resolution` is required on `build_forecast_reader`, and on `build_static_reader` for
`SingleTimeSeries` (one resolution per reader). It must be **omitted** for
`time_series_type=TimeSeriesType.NonSequentialTimeSeries`: an irregular series has no resolution, so
its timeline is the timestamp vector its cohort shares instead. `static_read` / `forecast_read` fill
the reader's buffers in place and return `None`; passing a `when` that is off the reader's timeline
raises `InvalidParameterError`.

### `StaticReader`

Reads the value of every matching static series at one timestamp. Results are **columnar**: series
are partitioned into `(dtype, element_shape)` groups, and each group's values come back as one dense
`(num_columns, *element_shape)` numpy array.

```python
class StaticReader:
    def grid(self) -> dict: ...     # {"time_series_type": str, "initial_timestamp": rfc3339 str, "resolution": ISO str | None, "length": int}
    def groups(self) -> list[dict]: ...  # each: {"dtype": str, "element_type": str, "element_shape": list[int], "keys": list[TimeSeriesKey]}
    def timestamps(self) -> list[datetime]: ...   # every timestamp on the timeline, in order
    def group_values(self, index: int) -> numpy.ndarray: ...  # last read of group `index`
```

All matched series must share one timeline — one grid (`initial_timestamp` + `length`) for
`SingleTimeSeries`, one timestamp vector for `NonSequentialTimeSeries`. The build validates this and
raises on divergence, so there is no presence mask — every column has a value at every valid
timestamp. `grid()["resolution"]` is `None` for an irregular reader; `timestamps()` is the timeline
either way, so a read loop written against it works unchanged for both. `group_values(i)` returns a
`(num_columns, *element_shape)` array whose column `j` corresponds to `groups()[i]["keys"][j]`; it
is empty until the first `static_read`.

```python
# For irregular series: build_static_reader(time_series_type=TimeSeriesType.NonSequentialTimeSeries)
reader = store.build_static_reader(timedelta(hours=1))
grid = reader.grid()
groups = reader.groups()
start = datetime.fromisoformat(grid["initial_timestamp"])
for ts in reader.timestamps():
    store.static_read(reader, ts)
    for i, g in enumerate(groups):
        vals = reader.group_values(i)   # column j ↔ g["keys"][j]
```

### `ForecastReader`

Reads the forecast _window_ at one timestamp for every matching forecast of one type. The build
filter must name a forecast type and pin a resolution; a `Deterministic` reader is abstract and also
includes `DeterministicSingleTimeSeries` (read into identical `(horizon, *element_shape)` windows).
All matched forecasts must share one window timeline (`initial_timestamp` + `interval` + `count`).

`time_series_type` must be one of the forecast types — `Deterministic`,
`DeterministicSingleTimeSeries`, `Probabilistic`, or `Scenarios`; any other raises
`InvalidParameterError`. A `Deterministic` reader also covers stored `DeterministicSingleTimeSeries`
forecasts, matching the read request rule.

```python
class ForecastReader:
    def timeline(self) -> dict: ...   # {"initial_timestamp": rfc3339 str, "resolution": ISO str, "interval": ISO str, "count": int, "time_series_type": str}
    def entries(self) -> list[TimeSeriesKey]: ...   # per-entry keys, in order (parallel to entry_values)
    def timestamps(self) -> list[datetime]: ...     # every window-start timestamp, in order
    def entry_values(self, index: int) -> numpy.ndarray: ...  # last read of entry `index`
    def num_slots(self) -> int: ...          # deduplicated window slots (physical reads per forecast_read)
    def entry_slot(self, index: int) -> int: ...  # 0-based slot backing entry `index`
```

Valid read timestamps are `initial_timestamp + k·interval` for `k in range(count)` (each names the
window forecast _from_ that instant). `entry_values(i)` returns the window backing `entries()[i]`,
shaped `(horizon, *element_shape)` for `Deterministic` / `DeterministicSingleTimeSeries`,
`(num_percentiles, horizon, *element_shape)` for `Probabilistic`, and
`(scenario_count, horizon, *element_shape)` for `Scenarios`; it is empty until the first
`forecast_read`.

```python
reader = store.build_forecast_reader(TimeSeriesType.Deterministic, timedelta(hours=1))
tl = reader.timeline()
entries = reader.entries()
for ts in reader.timestamps():
    store.forecast_read(reader, ts)
    for i, key in enumerate(entries):
        window = reader.entry_values(i)   # window for key's owner
```

**Window-read deduplication.** Forecasts that share one backing array and read plan — deduplicated
identical data, or several `DeterministicSingleTimeSeries` over one `SingleTimeSeries` — collapse to
a single _window slot_. `forecast_read` performs one backend (`.h5`) read per slot, not per entry,
so a forecast shared by N owners is read once per timestamp. `num_slots()` is that physical read
count (`<= len(entries())`), and `entry_slot(i)` (0-based) identifies the slot backing entry `i`;
entries that share data report the same slot. Group by slot to also materialize each unique window
only once on the Python side:

```python
store.forecast_read(reader, ts)
windows: dict[int, numpy.ndarray] = {}
for i, key in enumerate(entries):
    window = windows.setdefault(reader.entry_slot(i), reader.entry_values(i))
```

## Associations

Two catalogs of relationships between entities the store does not otherwise model. Both are
independent of time series: removing a time series never removes an association, and vice versa
(there are no foreign keys and no cascade — both endpoints live in the caller's object graph, so a
cascade could never fire), so a caller that wants both makes both calls.

Every query in both families takes the same keyword-only filter arguments as its family's `has_*`
method. All are optional and ANDed; with none set they match every row, which is what makes a
no-filter export and an `add_*` import a round trip. The `*_types` arguments are lists of
**concrete** type names, matched as SQL `IN (…)`: expanding an abstract type into its subtypes stays
in Python, where the type hierarchy lives, and an empty list matches nothing — unlike omitting the
argument, which matches everything. Every `remove_*` returns the number removed; removing nothing
returns `0` rather than raising.

### Supplemental-attribute associations

Which supplemental attributes are attached to which components. One attribute may be attached to
many components.

```python
SupplementalAttributeAssociation(
    component_id: int,
    component_type: str,
    attribute_id: int,
    attribute_type: str,
)
```

Read-only properties: `component_id`, `component_type`, `attribute_id`, `attribute_type`. The object
is hashable and compares structurally, so attachments work in sets and as dict keys. In the
**catalog**, though, identity is only the `(component_id, attribute_id)` pair — the type names are
denormalized labels carried for filtering — so re-attaching the same pair under different type names
raises `DuplicateAssociationError`.

```python
def add_supplemental_attribute_association(
    self, association: SupplementalAttributeAssociation
) -> None: ...
def add_supplemental_attribute_associations(
    self, associations: list[SupplementalAttributeAssociation]
) -> int: ...
# All-or-nothing: a duplicate anywhere in the batch rolls the whole batch back.
# Returns the number inserted; the import half of the round trip whose export is
# list_supplemental_attribute_associations() with no filter.

def has_supplemental_attribute_association(
    self,
    *,
    component_id: int | None = None,
    component_types: list[str] | None = None,
    attribute_id: int | None = None,
    attribute_types: list[str] | None = None,
) -> bool: ...

def list_supplemental_attribute_associations(
    self, *, ...
) -> list[SupplementalAttributeAssociation]: ...
def list_supplemental_attribute_ids(self, *, ...) -> list[int]: ...
def list_components_with_attributes(self, *, ...) -> list[int]: ...
def remove_supplemental_attribute_associations(self, *, ...) -> int: ...
def count_supplemental_attribute_associations(self, *, ...) -> int: ...
def count_supplemental_attributes(self, *, ...) -> int: ...
def count_components_with_attributes(self, *, ...) -> int: ...
# Every `...` above is the same keyword-only filter as has_supplemental_attribute_association.

def replace_supplemental_attribute_component_id(self, old_id: int, new_id: int) -> int: ...

def supplemental_attribute_counts_by_type(self) -> list[tuple[str, int]]: ...
def supplemental_attribute_summary(self) -> list[dict]: ...
```

- **`list_supplemental_attribute_associations`** returns rows in insertion order, so exporting with
  no filter and importing the result with `add_supplemental_attribute_associations` is a round trip.
- **`list_supplemental_attribute_ids`** returns the distinct attribute ids of the matching rows,
  ascending — the attributes attached to component `c` with `component_id=c`.
  **`list_components_with_attributes`** is the other end: the components carrying attribute `a` with
  `attribute_id=a`. **`count_supplemental_attributes`** and **`count_components_with_attributes`**
  are those two queries counted, and **`count_supplemental_attribute_associations`** counts the
  matching rows themselves.
- **`replace_supplemental_attribute_component_id`** moves every attachment from component `old_id`
  to `new_id`, returning the rows updated, and raises `DuplicateAssociationError` if `new_id`
  already carries one of the attributes being moved.
- **`supplemental_attribute_counts_by_type`** returns `[(attribute_type, count), …]` ordered by
  type; **`supplemental_attribute_summary`** returns one dict per distinct pair with keys
  `component_type`, `attribute_type`, `count`, ordered by attribute type then component type.

```python
from infrastore import SupplementalAttributeAssociation, Store

store = Store.create(in_memory=True)
store.add_supplemental_attribute_association(
    SupplementalAttributeAssociation(1, "Generator", 100, "GeographicInfo")
)
store.add_supplemental_attribute_association(
    SupplementalAttributeAssociation(2, "Load", 100, "GeographicInfo")
)

store.list_supplemental_attribute_ids(component_id=1)     # -> [100]
store.list_components_with_attributes(attribute_id=100)   # -> [1, 2]

store.remove_supplemental_attribute_associations(component_id=1)
# -> 1; any time series of component 1 are untouched
```

### Parent/child associations

Directed edges between components — a generator (parent) wired to a bus (child), say. Both endpoints
are always components; an attribute cannot appear here.

```python
ParentChildAssociation(
    parent_id: int,
    parent_type: str,
    child_id: int,
    child_type: str,
)
```

Read-only properties: `parent_id`, `parent_type`, `child_id`, `child_type`; hashable and
structurally comparable like the attachment object. In the **catalog**, identity is the _ordered_
`(parent_id, child_id)` pair, so the reversed pair is a different edge, while repeating the same
ordered pair under different type names raises `DuplicateAssociationError`. There is no
relationship-kind column, so one ordered pair may be related at most once.

This family is deliberately narrower than the supplemental one — no counts-by-type and no grouped
summary — because there is no consumer for them yet; both are additive if one appears.

```python
def add_parent_child_association(self, association: ParentChildAssociation) -> None: ...
def add_parent_child_associations(self, associations: list[ParentChildAssociation]) -> int: ...
# All-or-nothing, like the supplemental bulk add; returns the number inserted.

def has_parent_child_association(
    self,
    *,
    parent_id: int | None = None,
    parent_types: list[str] | None = None,
    child_id: int | None = None,
    child_types: list[str] | None = None,
) -> bool: ...

def list_parent_child_associations(self, *, ...) -> list[ParentChildAssociation]: ...
def list_children(self, *, ...) -> list[int]: ...
def list_parents(self, *, ...) -> list[int]: ...
def remove_parent_child_associations(self, *, ...) -> int: ...
def count_parent_child_associations(self, *, ...) -> int: ...
# Every `...` above is the same keyword-only filter as has_parent_child_association.

def replace_parent_child_component_id(self, old_id: int, new_id: int) -> int: ...
```

- **`list_parent_child_associations`** returns rows in insertion order, so a no-filter export and an
  `add_parent_child_associations` import round-trip.
- **`list_children`** returns the distinct child ids of the matching edges, ascending — the children
  of component `p` with `parent_id=p`; **`list_parents`** is the other end, the parents of component
  `c` with `child_id=c`.
- **`replace_parent_child_component_id`** rewrites `old_id` to `new_id` on **both** ends of every
  edge, returning the rows updated, and raises `DuplicateAssociationError` if the rewrite would
  duplicate an edge `new_id` already has.

```python
from infrastore import ParentChildAssociation, Store

store = Store.create(in_memory=True)
store.add_parent_child_association(ParentChildAssociation(1, "Generator", 7, "Bus"))
# The reversed pair is a different edge, not a duplicate.
store.add_parent_child_association(ParentChildAssociation(7, "Bus", 1, "Generator"))

store.list_children(parent_id=1)   # -> [7]
store.list_parents(child_id=7)     # -> [1]

store.remove_parent_child_associations(parent_types=["Bus"])   # -> 1
```

Neither association catalog is exposed over the [gRPC server](grpc-api.md) or the
[`infrastore` CLI](cli.md).

## Exceptions

All inherit from `TimeSeriesError`:

| Exception                   | Raised when                                           |
| --------------------------- | ----------------------------------------------------- |
| `NotFoundError`             | A key or array does not exist                         |
| `DuplicateTimeSeriesError`  | Adding a series whose key already exists              |
| `DuplicateAssociationError` | Re-adding an attachment or edge that already exists   |
| `InvalidParameterError`     | Bad arguments (bad feature type, malformed period, …) |
| `IntegrityError`            | On-disk inconsistency detected                        |
| `ReadOnlyStoreError`        | A write on a read-only store                          |
| `IoError`                   | Filesystem I/O failure                                |
| `ConnectionError`           | Connection failure (module-scoped, not the builtin)   |
| `IncompatibleFormatError`   | Store written in an incompatible on-disk format       |
| `IncompatibleForecastError` | Forecast parameters clash with existing forecasts     |
| `StorageError`              | SQLite catalog or serialization failure               |
| `StoreExistsError`          | Creating a store where one already exists             |
| `MismatchedArtifactError`   | The `.h5` and `.sqlite` halves came from two saves    |

A malformed ISO 8601 period string raises `InvalidParameterError` (inside the hierarchy). Only a
period argument that is neither a `timedelta` nor a `str` raises a plain `TypeError`, which
`except TimeSeriesError` will not catch.

Feature-value typing note: because `bool` is a subtype of `int` in Python, the binding checks `bool`
first, so `True`/`False` features are stored as booleans, not integers.

## `init_tracing`

```python
def init_tracing(filter: str) -> None: ...
```

Initialize the Rust tracing subscriber with the given
[`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
directive string. Examples:

```python
init_tracing("debug")                            # all targets at DEBUG
init_tracing("infrastore_core=debug")     # store core only
init_tracing("warn,infrastore_core=trace") # warn globally, trace the core
```

Silently no-ops if a subscriber is already registered (including the one auto-initialized from
`RUST_LOG` at module import). See the
[Python developer guide](../guides/python.md#diagnostics-and-tracing) for usage examples.
