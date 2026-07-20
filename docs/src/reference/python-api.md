# Python API

The PyO3 binding is importable as the `time_series_store` module (package `time-series-store`). It
is built as an `abi3-py310` wheel, so one build runs on CPython 3.10 and newer.

```python
from time_series_store import (
    TimeSeriesStore, SingleTimeSeries, NonSequentialTimeSeries, TimeSeriesKey,
    Deterministic, Probabilistic, Scenarios,
    TimeSeriesType, OwnerCategory,
    TimeSeriesError, NotFoundError, DuplicateTimeSeriesError,
    InvalidParameterError, IntegrityError, ReadOnlyStoreError,
)
```

`time_series_store.__version__` reports the wheel version.

> **Array dtypes.** The binding accepts and returns NumPy arrays of `float64`, `float32`, `int64`,
> `int32`, `uint64`, or `bool`; whatever dtype is given round-trips unchanged. Multi-dimensional
> arrays (a per-step element shape) are supported via the NumPy array's shape.

## `TimeSeriesStore`

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
) -> TimeSeriesStore: ...

@classmethod
def open(cls, path: str, read_only: bool = False) -> TimeSeriesStore: ...
```

- `create(in_memory=True)` — in-memory store; `path` and compression arguments are ignored.
- `create(path=...)` — writes `path` (NetCDF) and `path + ".sqlite"` (metadata).
- `create(path=..., compression="none")` — store arrays uncompressed; `compression="deflate"` with a
  `compression_level` / `shuffle` of your choice tunes the filter. The policy persists with the
  store and is reused on later appends. An unknown `compression` or out-of-range level raises
  `InvalidParameterError`.
- `open(path, read_only=True)` — read-only open; writes raise `ReadOnlyStoreError`.

The store is also a context manager: `with TimeSeriesStore.create(...) as store:` closes it on exit.
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
) -> list[SingleTimeSeries | NonSequentialTimeSeries | Deterministic | Probabilistic | Scenarios]: ...
# Reads each key's series in full — no time-range slicing. Results are returned in
# the same order as `keys`; an empty list of keys returns an empty list.

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
def get_forecast_parameters(self) -> dict: ...
def get_compression(self) -> dict: ...
def compact(self) -> dict: ...
def verify_integrity(self) -> dict: ...
# {"ok": bool, "errors": list[str]}
def flush(self) -> None: ...
```

> **Keyword-only arguments.** Every optional argument in the binding is keyword-only (the `*`
> marker): filter kwargs, `features=`/`units=`/`logical_type=` on the add paths, `time_range=` on
> the read paths, and so on. Positional use raises `TypeError`. The wheel ships a
> `time_series_store.pyi` stub, so IDEs and type checkers see the full signatures.

#### Return shapes

- **`add_time_series`** accepts a `SingleTimeSeries`, a `NonSequentialTimeSeries`, or a dense
  forecast object (`Deterministic` / `Probabilistic` / `Scenarios`) — see [Forecasts](#forecasts).
  **`transform_single_time_series`** derives a `DeterministicSingleTimeSeries` from every stored
  `SingleTimeSeries` and returns the count transformed. **`get_time_series`** returns whichever
  matches the stored type.
- **`bulk_read`** returns one typed object per key, in the same order as `keys` (an empty key list
  returns an empty list). It is the bulk counterpart to `get_time_series`: packed `SingleTimeSeries`
  are read in one decompress-once pass per dataset instead of one read per key. It does not slice —
  every series comes back in full; use `get_time_series(key, time_range=...)` for a window.
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
  `{"horizon": str, "interval": str, "count": int, "resolution": str}`, where `horizon`, `interval`,
  and `resolution` are ISO 8601 duration strings (e.g. `"PT1H"`). Every value is `None` when the
  store holds no forecasts.
- **`get_compression`** returns `{"compression": "deflate" | "none", "level": int, "shuffle": bool}`
  — the policy the store was created with (restored from the file on open; `"none"` for in-memory).
- **`compact`** returns
  `{"slots_reclaimed": int, "datasets_dropped": int, "feature_sets_reclaimed":
  int}`.
  `feature_sets_reclaimed` counts content-addressed feature rows that no association referenced any
  more; see the [file format](file-format.md#feature_sets).
- **`verify_integrity`** returns `{"ok": bool, "errors": list[str]}`; `ok` is `True` when the error
  list is empty.
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

## Exceptions

All inherit from `TimeSeriesError`:

| Exception                   | Raised when                                           |
| --------------------------- | ----------------------------------------------------- |
| `NotFoundError`             | A key or array does not exist                         |
| `DuplicateTimeSeriesError`  | Adding a series whose key already exists              |
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
init_tracing("time_series_store_core=debug")     # store core only
init_tracing("warn,time_series_store_core=trace") # warn globally, trace the core
```

Silently no-ops if a subscriber is already registered (including the one auto-initialized from
`RUST_LOG` at module import). See the
[Python developer guide](../guides/python.md#diagnostics-and-tracing) for usage examples.
