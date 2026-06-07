# Python API

The PyO3 binding is importable as the `time_series` module. It is built as an `abi3-py310` wheel, so
one build runs on CPython 3.10 and newer.

```python
from time_series import (
    TimeSeriesStore, SingleTimeSeries, TimeSeriesKey,
    TimeSeriesType, OwnerCategory,
    TimeSeriesError, NotFoundError, DuplicateTimeSeriesError,
    InvalidParameterError, IntegrityError, ReadOnlyStoreError,
)
```

`time_series.__version__` reports the wheel version.

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
    time_series: SingleTimeSeries,
    features: dict[str, int | float | bool | str] | None = None,
    units: str | None = None,
    scaling_factor_multiplier: str | None = None,
) -> TimeSeriesKey: ...

def get_time_series(
    self,
    key: TimeSeriesKey,
    time_range: tuple[datetime, datetime] | None = None,
) -> SingleTimeSeries: ...

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

- **`list_time_series`** returns a list of dicts, each with the keys: `owner_uuid`, `owner_type`,
  `owner_category`, `time_series_type`, `name`, `data_hash` (hex string), `length`,
  `resolution_seconds`, `features`, `units`, `scaling_factor_multiplier`. The `features` filter is a
  subset match — rows must contain at least the given pairs.
- **`get_time_series_counts`** returns
  `{"components_with_time_series": int, "static_time_series":
  int, "forecasts": int}`.
- **`compact`** returns `{"slots_reclaimed": int, "datasets_dropped": int}`.
- **`verify_integrity`** returns a list of error strings; an empty list means the store is intact.
- **`get_time_series`** with `time_range=(start, end)` slices on the time axis; `end` is exclusive.

## `SingleTimeSeries`

```python
SingleTimeSeries(
    initial_timestamp: datetime,
    resolution: timedelta,
    data: numpy.ndarray,   # dtype float64
)
```

Read-only properties: `initial_timestamp -> datetime`, `resolution -> timedelta`, `length -> int`,
`data -> numpy.ndarray[float64]`. The NetCDF backend accepts 1-D `data` only.

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

## Exceptions

All inherit from `TimeSeriesError`:

| Exception                  | Raised when                                         |
| -------------------------- | --------------------------------------------------- |
| `NotFoundError`            | A key or array does not exist                       |
| `DuplicateTimeSeriesError` | Adding a series whose key already exists            |
| `InvalidParameterError`    | Bad arguments (e.g. non-1-D data, bad feature type) |
| `IntegrityError`           | On-disk inconsistency detected                      |
| `ReadOnlyStoreError`       | A write on a read-only store                        |

Feature-value typing note: because `bool` is a subtype of `int` in Python, the binding checks `bool`
first, so `True`/`False` features are stored as booleans, not integers.
