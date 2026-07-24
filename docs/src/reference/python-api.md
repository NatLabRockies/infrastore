# Python API

The PyO3 binding is importable as the `castore` module (package `castore`). It is built as an
`abi3-py310` wheel, so one build runs on CPython 3.10 and newer.

```python
from castore import (
    Store, SingleTimeSeries, NonSequentialTimeSeries, TimeSeriesKey,
    Deterministic, Probabilistic, Scenarios,
    TimeSeriesType, OwnerCategory,
    SupplementalAttributeAssociation, ParentChildAssociation,
    TimeSeriesError, NotFoundError, DuplicateTimeSeriesError,
    DuplicateAssociationError, InvalidParameterError, IntegrityError, ReadOnlyStoreError,
)
```

`castore.__version__` reports the wheel version.

> **Array dtypes.** The binding accepts and returns NumPy arrays of `float64`, `float32`, `int64`,
> `int32`, `uint64`, or `bool`; whatever dtype is given round-trips unchanged. Multi-dimensional
> arrays (a per-step element shape) are supported via the NumPy array's shape.

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
) -> Store: ...

@classmethod
def open(cls, path: str, read_only: bool = False) -> Store: ...
```

- `create(in_memory=True)` — in-memory store; `path` and compression arguments are ignored.
- `create(path=...)` — writes `path` (NetCDF) and `path + ".sqlite"` (metadata).
- `create(path=..., compression="none")` — store arrays uncompressed; `compression="deflate"` with a
  `compression_level` / `shuffle` of your choice tunes the filter. The policy persists with the
  store and is reused on later appends. An unknown `compression` or out-of-range level raises
  `InvalidParameterError`.
- `open(path, read_only=True)` — read-only open; writes raise `ReadOnlyStoreError`.

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
) -> TimeSeriesKey: ...
# `name` comes from the time_series object
# (e.g. SingleTimeSeries(..., name=...)), not from this call.

def add_time_series_bulk(self, items: list[dict]) -> list[TimeSeriesKey]: ...
# Each item dict mirrors add_time_series's parameters: required `owner_id`,
# `owner_type`, `owner_category`, `time_series`; optional `features`, `units`.
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
    time_series_type: TimeSeriesType | None = None,
    name: str | None = None,
    name_glob: str | None = None,   # SQLite GLOB pattern; ANDed with `name`
    resolution: timedelta | str | None = None,
    interval: timedelta | str | None = None,
    features: dict[str, int | float | bool | str] | None = None,
) -> list[dict]: ...

def list_array_groups(self, *, ...) -> list[dict]: ...
# Same keyword-only filter arguments as list_time_series; so do list_keys,
# list_names, list_owner_types, and remove_by_filter.

def get_time_series_keys(
    self,
    owner_id: int,
    owner_category: OwnerCategory,
) -> list[TimeSeriesKey]: ...
def has_time_series(self, key: TimeSeriesKey) -> bool: ...
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
```

> **Keyword-only arguments.** Every optional argument in the binding is keyword-only (the `*`
> marker): filter kwargs, `features=`/`units=`/`ext=` on the add paths, `time_range=` on the read
> paths, and so on. Positional use raises `TypeError`. The wheel ships a `castore.pyi` stub, so IDEs
> and type checkers see the full signatures.

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
  (ISO 8601 duration string, e.g. `PT1H`, or `None`), `timestamps`, `features`, `units`.
  `timestamps` is a list of RFC 3339 strings for non-sequential series and `None` otherwise. The
  `features` filter is a subset match — rows must contain at least the given pairs.
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
  `{"slots_reclaimed": int, "datasets_dropped": int, "feature_sets_reclaimed":
  int}`.
  `feature_sets_reclaimed` counts content-addressed feature rows that no association referenced any
  more; see the [file format](file-format.md#feature_sets).
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
array's dtype (one of `float64`, `float32`, `int64`, `int32`, `uint64`, `bool`) and per-step element
shape are preserved through a round-trip.

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
reader is built once over a filter, pins one resolution, and reuses its output buffers so a tight
loop allocates almost nothing. There are two: `StaticReader` for `SingleTimeSeries`, and
`ForecastReader` for forecasts. Both share the lifecycle: build → inspect the layout once →
`*_read(when)` in a loop → pull values per group/entry.

The builders and drivers live on `Store`:

```python
def build_static_reader(
    self,
    resolution: timedelta | str,
    *,
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

`resolution` is required on both builders (one resolution per reader). `static_read` /
`forecast_read` fill the reader's buffers in place and return `None`; passing a `when` that is off
the reader's grid or timeline raises `InvalidParameterError`.

### `StaticReader`

Reads the value of every matching `SingleTimeSeries` at one timestamp. Results are **columnar**:
series are partitioned into `(dtype, element_shape)` groups, and each group's values come back as
one dense `(num_columns, *element_shape)` numpy array.

```python
class StaticReader:
    def grid(self) -> dict: ...     # {"initial_timestamp": rfc3339 str, "resolution": ISO str, "length": int}
    def groups(self) -> list[dict]: ...  # each: {"dtype": str, "element_shape": list[int], "keys": list[TimeSeriesKey]}
    def timestamps(self) -> list[datetime]: ...   # every timestamp on the grid, in order
    def group_values(self, index: int) -> numpy.ndarray: ...  # last read of group `index`
```

All matched series must share one grid (`initial_timestamp` + `length`); the build validates this
and raises on divergence, so there is no presence mask — every column has a value at every valid
timestamp. `group_values(i)` returns a `(num_columns, *element_shape)` array whose column `j`
corresponds to `groups()[i]["keys"][j]`; it is empty until the first `static_read`.

```python
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

`time_series_type` must be one of the concrete forecast types — `Deterministic`,
`DeterministicSingleTimeSeries`, `Probabilistic`, or `Scenarios`; any other raises
`InvalidParameterError`.

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
a single _window slot_. `forecast_read` performs one backend (`.nc`) read per slot, not per entry,
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
from castore import SupplementalAttributeAssociation, Store

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
from castore import ParentChildAssociation, Store

store = Store.create(in_memory=True)
store.add_parent_child_association(ParentChildAssociation(1, "Generator", 7, "Bus"))
# The reversed pair is a different edge, not a duplicate.
store.add_parent_child_association(ParentChildAssociation(7, "Bus", 1, "Generator"))

store.list_children(parent_id=1)   # -> [7]
store.list_parents(child_id=7)     # -> [1]

store.remove_parent_child_associations(parent_types=["Bus"])   # -> 1
```

Neither association catalog is exposed over the [gRPC server](grpc-api.md) or the
[`cas` CLI](cli.md).

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
init_tracing("castore_core=debug")     # store core only
init_tracing("warn,castore_core=trace") # warn globally, trace the core
```

Silently no-ops if a subscriber is already registered (including the one auto-initialized from
`RUST_LOG` at module import). See the
[Python developer guide](../guides/python.md#diagnostics-and-tracing) for usage examples.
