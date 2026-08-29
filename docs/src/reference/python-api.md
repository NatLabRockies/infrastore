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
`time_range` bound, a reader's `when` — may be aware or naive, and the store records **which**.

An **aware** datetime names an instant, and any zone will do: `datetime.timezone.utc`, a `ZoneInfo`,
or a fixed offset. It is converted to UTC on the way in, so two aware datetimes naming the same
instant are the same instant to the store — and the spelling it arrived in is recorded, so it is the
spelling that comes back.

A **naive** datetime names a wall clock and no instant. It is accepted and recorded as
`time_reference = "zoneless"`; its fields are read as they stand (never through `astimezone`, which
would apply the machine's local zone), and a read hands back a naive datetime again. That round-trip
is the whole reason accepting one is safe:

```python
datetime(2024, 1, 1) == datetime(2024, 1, 1, tzinfo=timezone.utc)   # False
datetime(2024, 1, 1) <  datetime(2024, 1, 1, tzinfo=timezone.utc)   # TypeError
```

A store that took a naive datetime and returned an aware one would be worse than one that refused.

### Time references

Every series carries a `time_reference` recording how its timestamps were spelled, inferred from the
`datetime` it was built with:

| Input                        | `time_reference`   |
| ---------------------------- | ------------------ |
| `tzinfo=timezone.utc`        | `"utc"`            |
| a fixed-offset `tzinfo`      | `"-07:00"`         |
| `ZoneInfo("America/Denver")` | `"America/Denver"` |
| naive                        | `"zoneless"`       |

`ZoneInfo("UTC")` records the _zone_ `"UTC"`, not the literal `"utc"`: the two render identically
forever, and the difference is only in what the catalog reports back.

Reads spell the timestamp back the same way — a `ZoneInfo` series returns datetimes carrying that
`ZoneInfo`, including the correct side of a fall-back hour. A **query bound must match**: a naive
bound against a series that records instants, or an aware bound against a zoneless one, raises
`InvalidParameterError` rather than being coerced, and so does a `time_range` whose two ends
disagree. `list_time_series(zoneless=...)`, `build_static_reader(..., zoneless=...)`, and the other
filter-taking methods take a `zoneless` predicate for building a coherent selection. See
[Time references](../explanation/data-model.md#time-references) for the full rules.

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
    path: str | os.PathLike | None = None,
    *,
    in_memory: bool = False,
    compression: str = "deflate",   # "deflate" or "none"
    compression_level: int = 3,     # 0–9, DEFLATE only
    shuffle: bool = True,           # byte-shuffle filter, DEFLATE only
    catalog: str | None = None,     # "attached" or "memory"; None matches the backend
    overwrite: bool = False,        # discard an artifact already at `path`
) -> Store: ...

@classmethod
def open(
    cls, path: str | os.PathLike, *, read_only: bool = False, catalog: str = "attached"
) -> Store: ...

@classmethod
def open_copy(
    cls, src: str | os.PathLike, dest: str | os.PathLike, *, catalog: str = "attached"
) -> Store: ...
```

Every argument after the path(s) is keyword-only; `Store.create("s.h5", True)` raises `TypeError`.
Paths accept anything `os.fspath` does, `pathlib.Path` included (the shipped stub spells them
`str`).

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

### Properties

```python
store.read_only -> bool
store.catalog -> str            # "attached" or "memory"
store.in_transaction -> bool
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
    *,
    features: dict[str, int | float | bool | str] | None = None,
    units: str | None = None,
    element_type: str | None = None,
    application_data: str | None = None,
    quantity_kind: str | None = None,
    unit_system: str | None = None,   # "natural_units" | "component_base"
    time_reference: str | None = None,   # "utc" | "zoneless" | "-07:00" | "America/Denver"
    component_field: str | None = None,  # e.g. "max_active_power"
) -> AddedTimeSeries: ...
# `time_reference` is normally omitted: it is inferred from the datetime the
# series was built with (see "Time references" below). Pass it to override.
# An unrecognized `unit_system` raises InvalidParameterError rather than
# degrading to unspecified; omitting it leaves the basis unspecified, which is
# not the same as declaring natural units.
# `name` comes from the time_series object
# (e.g. SingleTimeSeries(..., name=...)), not from this call.
# A `features` key that shadows a time-series or key field (`name`, `resolution`,
# `owner_id`, ...) raises InvalidParameterError.

def add_time_series_bulk(self, items: list[dict]) -> list[AddedTimeSeries]: ...
# Each item dict mirrors add_time_series's parameters: required `owner_id`,
# `owner_type`, `owner_category`, `time_series`; optional `features`, `units`,
# `element_type`, `application_data`, `quantity_kind`, `unit_system`,
# `time_reference`, `component_field`.
# All items commit in ONE metadata transaction (all-or-nothing), which is much
# faster than looping over add_time_series. Results are in input order.

class AddedTimeSeries:
    key: TimeSeriesKey   # names the series
    id: int              # the catalog row's id — store it to reference the series later
# Hashable and comparable; the id is never reissued once its row is deleted.
# No add takes an id — the catalog assigns, and this reports what it chose. The
# one writer that files rows under supplied ids is
# import_time_series_associations_openapi.

def get_metadata_by_id(self, id: int) -> dict | None: ...   # None when no row has the id
def association_exists(self, id: int) -> bool: ...          # no row fetched

def transform_single_time_series(
    self,
    horizon: timedelta | str,
    interval: timedelta | str,
    *,
    owner_category: OwnerCategory | None = None,
    resolution: timedelta | str | None = None,
) -> int: ...
# Derives a DeterministicSingleTimeSeries from every stored SingleTimeSeries —
# or, with `owner_category` / `resolution`, only from the ones matching — and
# returns the count. `horizon / resolution` steps must fit inside each source.

def copy_time_series(
    self,
    src: TimeSeriesKey,
    dst_owner_id: int,
    dst_owner_type: str,
    *,
    new_name: str | None = None,
) -> TimeSeriesKey: ...
# Attach the same array to another owner (no data is duplicated); returns the new key.
def rename_time_series(self, key: TimeSeriesKey, new_name: str) -> TimeSeriesKey: ...
# Same identity, new name; returns the renamed key.

def get_time_series(
    self,
    key: TimeSeriesKey,
    *,
    time_range: tuple[datetime, datetime] | None = None,
) -> SingleTimeSeries | NonSequentialTimeSeries | Deterministic | Probabilistic | Scenarios: ...
def get_metadata(self, key: TimeSeriesKey) -> dict: ...
# The whole catalog record for one key — the same dict shape list_time_series
# returns (see Return shapes), without reading the array.
def get_array_by_hash(self, data_hash: str) -> numpy.ndarray: ...
# The raw array behind a 64-char hex content hash, bypassing the catalog.
def count_array_references(self, data_hash: str) -> dict: ...
# {"sts": int, "dst": int}: SingleTimeSeries and DeterministicSingleTimeSeries
# associations sharing that array.
def resolve_forecast_key(
    self,
    owner_id: int,
    owner_category: OwnerCategory,
    name: str,
    requested_type: TimeSeriesType,
    *,
    resolution: timedelta | str | None = None,
    interval: timedelta | str | None = None,
    features: dict[str, int | float | bool | str] | None = None,
) -> TimeSeriesKey: ...
# Attributes + requested type -> the concrete key. TimeSeriesType.Deterministic
# also matches a stored DeterministicSingleTimeSeries; the returned key's
# time_series_type says which was found.

def bulk_read(
    self,
    keys: list[TimeSeriesKey],
    *,
    time_range: tuple[datetime, datetime] | None = None,
) -> list[SingleTimeSeries | NonSequentialTimeSeries | Deterministic | Probabilistic | Scenarios]: ...
# `time_range` applies the same window to every key (default: each series in full).
# Results are returned in the same order as `keys`; an empty list of keys returns an empty list.

def read_by_ids(
    self, ids: list[int]
) -> list[SingleTimeSeries | NonSequentialTimeSeries | Deterministic | Probabilistic | Scenarios]: ...
# The same read addressed by catalog association id. Results follow the order the
# ids are given, repeats included; NotFoundError if any id names no row.

def remove_time_series(self, key: TimeSeriesKey) -> None: ...
def remove_time_series_bulk(self, keys: list[TimeSeriesKey]) -> int: ...
# All-or-nothing: a key matching nothing fails the whole batch. Returns the count removed.
def remove_by_filter(self, *, ...) -> int: ...
# Same keyword-only filter arguments as list_time_series; one all-or-nothing
# transaction; returns the count removed (0 when nothing matched).
def clear_time_series(
    self,
    *,
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
def list_keys(self, *, ...) -> list[TimeSeriesKey]: ...
def list_names(self, *, ...) -> list[str]:  ...        # distinct names, sorted
def list_owner_types(self, *, ...) -> list[str]: ...   # distinct owner types, sorted
# Every `...` above is the same keyword-only filter as list_time_series, and so
# is remove_by_filter's.
# `time_series_type` is a TimeSeriesType (or its member name as a str).
# TimeSeriesType.Deterministic matches both Deterministic and
# DeterministicSingleTimeSeries rows. Every filter surface takes it, including
# has_any_time_series, get_resolutions, get_intervals, list_owner_ids, and
# build_forecast_reader.
def list_owner_ids(
    self,
    owner_category: OwnerCategory,
    *,
    time_series_type: TimeSeriesType | None = None,
    resolution: timedelta | str | None = None,
) -> list[int]: ...
# Distinct owner ids of that category holding time series, ascending.

def get_time_series_keys(
    self,
    owner_id: int,
    owner_category: OwnerCategory,
) -> list[TimeSeriesKey]: ...
def has_time_series(self, key: TimeSeriesKey) -> bool: ...
def has_any_time_series(self, *, ...) -> bool: ...
# Existence without listing ("does this owner have any time series?"); same
# keyword-only filter arguments as list_time_series. Index-probe fast.
def is_empty(self) -> bool: ...
# Whether the store holds nothing at all — no time series, no associations in
# any catalog. One index probe per catalog table, so its cost does not grow with
# the store, and it stays correct as the catalog gains tables; a conjunction over
# the count_* methods does neither.
def get_resolutions(self, time_series_type: TimeSeriesType | None = None) -> list[str]: ...
def get_intervals(self, time_series_type: TimeSeriesType | None = None) -> list[str]: ...
# Distinct resolutions / forecast intervals as ISO 8601 duration strings, e.g. "PT1H".
def get_time_series_counts(self) -> dict: ...
def time_series_counts_detailed(self) -> dict: ...
def counts_by_type(self) -> dict[str, int]: ...       # {time_series_type name: count}
def num_distinct_arrays(self) -> int: ...
def static_summary(self) -> list[dict]: ...
def forecast_summary(self) -> list[dict]: ...
def check_static_consistency(self, resolution: timedelta | str | None = None) -> list[dict]: ...
# One {"resolution", "initial_timestamp", "length"} per resolution present (or
# the one given); raises if the SingleTimeSeries of one resolution disagree on
# their grid — the precondition build_static_reader relies on.
def get_forecast_parameters(self, *, resolution: timedelta | str | None = None,
                            interval: timedelta | str | None = None) -> dict: ...
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
                                            # `with store.transaction() as s:` binds the Store
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
  `SingleTimeSeries` (or the subset its `owner_category` / `resolution` arguments select) and
  returns the count transformed. **`get_time_series`** returns whichever matches the stored type.
- **`bulk_read`** returns one typed object per key, in the same order as `keys` (an empty key list
  returns an empty list). It is the bulk counterpart to `get_time_series`: packed `SingleTimeSeries`
  are read in one decompress-once pass per dataset instead of one read per key. Pass the
  keyword-only `time_range=(start, end)` to apply the same window to every key; by default each
  series comes back in full.
- **`read_by_ids`** is the same read addressed by catalog
  [association id](../explanation/data-model.md) rather than by key — the read direction of the id
  every write reports on its `AddedTimeSeries`, for a caller that recorded ids in its own model
  instead of keeping an id-to-key map beside the store. Results follow the order the ids are given,
  repeats included; an id naming no row raises `NotFoundError` and fails the whole call, unlike
  `association_exists`, which asks the question rather than committing to a read.
- **`list_time_series`** returns a list of dicts (the same shape `get_metadata` returns for one
  key), each with the keys: `owner_id`, `owner_type`, `owner_category`, `time_series_type`, `name`,
  `data_hash` (hex string), `initial_timestamp` (RFC 3339 string, or `None` for non-sequential
  series), `length`, `resolution` (ISO 8601 duration string, e.g. `PT1H`, or `None`), `timestamps`,
  `horizon`, `interval`, `count`, `percentiles`, `element_type`, `element_shape`, `features`,
  `units`, `quantity_kind`, `unit_system` (`"natural_units"` / `"component_base"` / `None`),
  `time_reference`, `component_field`, `application_data`. `timestamps` is a list of RFC 3339
  strings for non-sequential series and `None` otherwise; `horizon` / `interval` / `count` are set
  for forecasts and `percentiles` for `Probabilistic` only. The `features` filter is a **subset**
  match — rows must contain at least the given pairs — whereas a `TimeSeriesKey` matches its feature
  map exactly.
- **`list_array_groups`** accepts the same filters as `list_time_series` and groups the matching
  series by their underlying stored array. It returns a list of dicts, each with `data_hash` (hex
  string), `keys` (a list of `TimeSeriesKey`s that resolve to that array), and `ids` (each of those
  keys' association id, positionally aligned with `keys` — a `TimeSeriesKey` is opaque and carries
  no id itself; `None` for a row written before ids were minted). Keys sharing one dict share one
  deduplicated array.
- **`get_time_series_counts`** returns
  `{"components_with_time_series": int, "static_time_series": int, "forecasts": int}`;
  **`time_series_counts_detailed`** adds `supplemental_attributes_with_time_series` and spells the
  other two `static_time_series_count` / `forecast_count`.
- **`static_summary`** returns one dict per distinct
  `(owner_type, owner_category, time_series_type, name, initial_timestamp, resolution,
  time_step_count)`
  with its `count`; **`forecast_summary`** does the same for forecasts, adding `horizon`,
  `interval`, and `window_count`.
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
`PT1H`), `length -> int`, `data -> numpy.ndarray`, `name -> str`, `time_reference -> str | None`.
`initial_timestamp` comes back spelled the way it was written — see
[Time references](#time-references). The constructor accepts either a `timedelta` or an ISO 8601
duration string for `resolution`; the getter always returns the ISO string. `name` is a required
association attribute (the same array may be stored under different names). It is read off the
object by `add_time_series` and populated on `get_time_series`. The array's `element_type` and
per-step element shape are preserved through a round-trip.

## `NonSequentialTimeSeries`

```python
NonSequentialTimeSeries(
    timestamps: list[datetime],
    data: numpy.ndarray,
    name: str,
)
```

Read-only properties: `timestamps`, `length`, `data`, `name`, and `time_reference`. Timestamps must
be strictly increasing, match the first data dimension, and agree on one spelling — a vector mixing
naive and aware values raises `InvalidParameterError`, since one series records one reference.
`get_time_series` returns this class for a non-sequential key.

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
[`get_time_series_counts`](#methods) reports the forecast total under `forecasts`.

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
    component_field: str | None = None,
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
    component_field: str | None = None,
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

Read-only properties: `component_id`, `component_type`, `attribute_id`, `attribute_type`, and `id` —
the catalog row's own number, `None` on a value that has not been through the catalog. The object is
hashable and compares structurally (the `id` stays out of both), so attachments work in sets and as
dict keys. In the **catalog**, though, identity is only the `(component_id, attribute_id)` pair —
the type names are denormalized labels carried for filtering — so re-attaching the same pair under
different type names raises `DuplicateAssociationError`.

The `id` is an output only. The constructor takes none, and an add ignores whatever a listed row
carries, so attaching a row read from one store to another files it under a fresh id there.

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

Read-only properties: `parent_id`, `parent_type`, `child_id`, `child_type`, and `id`; hashable and
structurally comparable like the attachment object, with the same output-only `id`. In the
**catalog**, identity is the _ordered_ `(parent_id, child_id)` pair, so the reversed pair is a
different edge, while repeating the same ordered pair under different type names raises
`DuplicateAssociationError`. There is no relationship-kind column, so one ordered pair may be
related at most once.

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

### OpenAPI-row association serde

Direct JSON serde of the two association catalogs, in the wire spelling
[SiennaSchemas](https://github.com/Sienna-Platform/SiennaSchemas) defines (`TimeSeries/*.json`,
`Core/Associations/SupplementalAttributeAssociation.json`). Unlike `list_time_series` /
`list_supplemental_attribute_associations`, which return Python objects, these four methods exchange
the wire JSON verbatim — the format a document author (e.g. PowerTableDataParser) reads and writes
directly.

```python
def export_time_series_associations_openapi(
    self, *, owner_id=None, owner_category=None, owner_type=None,
    time_series_type=None, name=None, name_glob=None, component_field=None,
    resolution=None, interval=None, features=None,
) -> str: ...
def import_time_series_associations_openapi(self, json: str) -> int: ...
def export_supplemental_attribute_associations_openapi(self) -> str: ...
def import_supplemental_attribute_associations_openapi(self, json: str) -> int: ...
```

`export_time_series_associations_openapi` takes the same filter keywords as `list_time_series`.
Every row's `uri` and `data_hash` are the hex-encoded content hash the store already has for that
row — never a caller-supplied locator. With no filter this exports the whole catalog, sorted by
identity.

`export_supplemental_attribute_associations_openapi` exports the whole
`supplemental_attribute_associations` table, sorted by `(component_id, attribute_id)`;
`import_supplemental_attribute_associations_openapi` is its import half — a bulk, all-or-nothing
insert (a duplicate anywhere in the batch raises `DuplicateAssociationError` and rolls the batch
back), returning the number of rows inserted.

`import_time_series_associations_openapi` is the time-series import half, and it writes **rows
only**: the document carries locators, never values, so every row must name an array this store
already holds — the arrays arrive with the artifact. Each row keeps the `association_id` it carries,
which is the point: an import that assigned fresh ids would leave every reference the document
records pointing at the wrong series. A row whose array is absent, or a `NonSequentialTimeSeries`
row (whose `timestamps_hash` is store-internal and so not on the wire, leaving the document with no
way to say which stored time axis the row sits on), raises `InvalidParameterError` and rolls the
whole batch back.

Infrastore never modifies the data to make an incoming document agree with what it already holds. A
geometry disagreement between an added series and its own association row is likewise rejected at
the add boundary (`InvalidParameterError`), loudly and without writing anything.

```python
store = Store.create(in_memory=True)
store.add_time_series(
    owner_id=1, owner_type="Generator", owner_category=OwnerCategory.Component,
    time_series=SingleTimeSeries(t0, timedelta(hours=1), values, "load"),
)

json_str = store.export_time_series_associations_openapi()
```

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

A malformed ISO 8601 period string raises `InvalidParameterError` (inside the hierarchy), as does a
naive `datetime`. Only a period argument that is neither a `timedelta` nor a `str` (or a
`time_series_type` that is neither a `TimeSeriesType` nor a `str`) raises a plain `TypeError`, which
`except TimeSeriesError` will not catch. `init_tracing` with an unparseable filter raises
`ValueError`.

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
