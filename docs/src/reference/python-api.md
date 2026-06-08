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
> arrays (a per-step element shape) are supported via the NumPy array's shape. The `logical_type`
> label is exposed on [`add_forecast`](#forecasts) but not on `add_time_series` for static series.

## `TimeSeriesStore`

### Constructors

```python
@classmethod
def create(cls, path: str | None = None, in_memory: bool = False) -> TimeSeriesStore: ...

@classmethod
def open(cls, path: str, read_only: bool = False) -> TimeSeriesStore: ...
```

- `create(in_memory=True)` — in-memory store; `path` is ignored.
- `create(path=...)` — writes `path` (NetCDF) and `path + ".sqlite"` (metadata).
- `open(path, read_only=True)` — read-only open; writes raise `ReadOnlyStoreError`.

### Property

```python
store.read_only -> bool
```

### Methods

```python
def add_time_series(
    self,
    owner_uuid: str,
    owner_type: str,
    owner_category: OwnerCategory,
    name: str,
    time_series: SingleTimeSeries | NonSequentialTimeSeries,
    features: dict[str, int | float | bool | str] | None = None,
    units: str | None = None,
    scaling_factor_multiplier: str | None = None,
) -> TimeSeriesKey: ...

def add_forecast(
    self,
    owner_uuid: str,
    owner_type: str,
    owner_category: OwnerCategory,
    name: str,
    time_series_type: TimeSeriesType,
    initial_timestamp: datetime,
    resolution: timedelta,
    horizon: timedelta,
    interval: timedelta,
    count: int,
    data: numpy.ndarray,
    features: dict[str, int | float | bool | str] | None = None,
    units: str | None = None,
    scaling_factor_multiplier: str | None = None,
    percentiles: list[float] | None = None,
    logical_type: str | None = None,
) -> TimeSeriesKey: ...

def get_time_series(
    self,
    key: TimeSeriesKey,
    time_range: tuple[datetime, datetime] | None = None,
) -> SingleTimeSeries | NonSequentialTimeSeries | Deterministic | Probabilistic | Scenarios: ...

def remove_time_series(self, key: TimeSeriesKey) -> None: ...
def clear_time_series(self, owner_uuid: str | None = None) -> int: ...

def list_time_series(
    self,
    owner_uuid: str | None = None,
    owner_type: str | None = None,
    time_series_type: TimeSeriesType | None = None,
    name: str | None = None,
    resolution: timedelta | None = None,
    features: dict[str, int | float | bool | str] | None = None,
) -> list[dict]: ...

def get_time_series_keys(self, owner_uuid: str) -> list[TimeSeriesKey]: ...
def has_time_series(self, key: TimeSeriesKey) -> bool: ...
def get_resolutions(self, time_series_type: TimeSeriesType | None = None) -> list[timedelta]: ...
def get_time_series_counts(self) -> dict: ...
def compact(self) -> dict: ...
def verify_integrity(self) -> list[str]: ...
def flush(self) -> None: ...
```

#### Return shapes

- **`add_time_series`** accepts a `SingleTimeSeries` or a `NonSequentialTimeSeries`;
  **`add_forecast`** writes a forecast (`Deterministic` / `DeterministicSingleTimeSeries` /
  `Probabilistic` / `Scenarios`) — see [Forecasts](#forecasts). **`get_time_series`** returns
  whichever matches the stored type.
- **`list_time_series`** returns a list of dicts, each with the keys: `owner_uuid`, `owner_type`,
  `owner_category`, `time_series_type`, `name`, `data_hash` (hex string), `length`,
  `resolution_seconds`, `timestamps`, `features`, `units`, `scaling_factor_multiplier`. `timestamps`
  is a list of RFC 3339 strings for non-sequential series and `None` otherwise. The `features`
  filter is a subset match — rows must contain at least the given pairs.
- **`get_time_series_counts`** returns
  `{"components_with_time_series": int, "static_time_series": int, "forecasts": int}`.
- **`compact`** returns `{"slots_reclaimed": int, "datasets_dropped": int}`.
- **`verify_integrity`** returns a list of error strings; an empty list means the store is intact.
- **`get_time_series`** with `time_range=(start, end)` slices on the time axis; `end` is exclusive.

## `SingleTimeSeries`

```python
SingleTimeSeries(
    initial_timestamp: datetime,
    resolution: timedelta,
    data: numpy.ndarray,   # shape (length,) or (length, k1, ...)
)
```

Read-only properties: `initial_timestamp -> datetime`, `resolution -> timedelta`, `length -> int`,
`data -> numpy.ndarray`. The array's dtype (one of `float64`, `float32`, `int64`, `int32`, `uint64`,
`bool`) and per-step element shape are preserved through a round-trip.

## `NonSequentialTimeSeries`

```python
NonSequentialTimeSeries(
    timestamps: list[datetime],
    data: numpy.ndarray,
)
```

Read-only properties: `timestamps`, `length`, and `data`. Timestamps must be timezone-aware,
strictly increasing, and match the first data dimension. `get_time_series` returns this class for a
non-sequential key.

## `TimeSeriesKey`

Returned by `add_time_series` and `get_time_series_keys`; not constructed directly. Read-only
properties:

```python
key.owner_uuid        -> str
key.time_series_type  -> TimeSeriesType
key.name              -> str
key.resolution        -> timedelta | None
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

Forecasts are written with [`add_forecast`](#methods) and read back through `get_time_series`, which
returns a `Deterministic`, `Probabilistic`, or `Scenarios` object depending on the stored type (a
`DeterministicSingleTimeSeries` is synthesized into a `Deterministic` on read).
[`get_time_series_counts`](#timeseriesstore) reports the forecast total under `forecasts`.

`data` is a NumPy array in the canonical shape for the `time_series_type`, where `H` is
`horizon / resolution`:

| `time_series_type`              | `data` shape                       | `percentiles` |
| ------------------------------- | ---------------------------------- | ------------- |
| `Deterministic`                 | `[H, count, *element_shape]`       | omit          |
| `Probabilistic`                 | `[len(percentiles), H, count, *E]` | required      |
| `Scenarios`                     | `[scenario_count, H, count, *E]`   | omit          |
| `DeterministicSingleTimeSeries` | `[total_len, *E]` (underlying STS) | omit          |

### `Deterministic`

Returned by `get_time_series` for a deterministic forecast; not constructed directly. Read-only
properties:

```python
forecast.initial_timestamp -> datetime
forecast.resolution        -> timedelta
forecast.horizon           -> timedelta
forecast.interval          -> timedelta
forecast.count             -> int
forecast.data              -> numpy.ndarray
```

### `Probabilistic`

Same properties as `Deterministic`, plus:

```python
forecast.percentiles -> list[float]
```

### `Scenarios`

Same properties as `Deterministic`, plus:

```python
forecast.scenario_count -> int
```

## Exceptions

All inherit from `TimeSeriesError`:

| Exception                  | Raised when                                           |
| -------------------------- | ----------------------------------------------------- |
| `NotFoundError`            | A key or array does not exist                         |
| `DuplicateTimeSeriesError` | Adding a series whose key already exists              |
| `InvalidParameterError`    | Bad arguments (e.g. bad feature type, bad timestamps) |
| `IntegrityError`           | On-disk inconsistency detected                        |
| `ReadOnlyStoreError`       | A write on a read-only store                          |

Feature-value typing note: because `bool` is a subtype of `int` in Python, the binding checks `bool`
first, so `True`/`False` features are stored as booleans, not integers.
