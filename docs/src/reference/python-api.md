# Python API

The PyO3 binding is importable as the `infrastore` module (package `infrastore`). It is built as an
`abi3-py311` wheel, so one build runs on CPython 3.11 and newer.

```python
from infrastore import (
    Store, SingleTimeSeries, NonSequentialTimeSeries, PersistentTimeSeries,
    Deterministic, Probabilistic, Scenarios,
    TimeSeriesType, OwnerCategory,
    SupplementalAttributeAssociation, ParentChildAssociation,
    TimeSeriesError, NotFoundError, OwnerMismatchError, DuplicateTimeSeriesError,
    DuplicateAssociationError, InvalidParameterError, IntegrityError, ReadOnlyStoreError,
)
```

`infrastore.__version__` reports the wheel version.

> **Array dtypes.** The binding accepts and returns NumPy arrays of `float64`, `float32`, the signed
> and unsigned integer widths (`int64`/`int32`/`int16`/`int8`/`uint64`/`uint32`/`uint16`/`uint8`),
> or `bool`; whatever dtype is given round-trips unchanged. What those elements _mean_ is the
> association's `element_type` (see [Element types](./element-types.md)). A composite series is
> built with the `from_values` classmethod, which encodes the per-timestep values and declares the
> element type they imply, and read back with `.decoded_values()`; `element_type=` on the plain
> constructor declares it for an array you already hold, and `encode_element_values` /
> `decode_element_values` are the standalone pair. Multi-dimensional arrays (a per-step element
> shape) are supported via the NumPy array's shape.

## Datetimes

Every `datetime` argument — an initial timestamp, a `NonSequentialTimeSeries` timestamp vector or a
`PersistentTimeSeries` breakpoint vector, a `time_range` bound, a reader's `when` — may be aware or
naive, and the store records **which**.

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
disagree. `list_metadata(zoneless=...)`, `build_static_reader(..., zoneless=...)`, and the other
filter-taking methods take a `zoneless` predicate for building a coherent selection. See
[Time references](../explanation/time-references.md) for the full rules.

A `datetime` that is **stored** — an initial timestamp, or an entry of a `NonSequentialTimeSeries`
or `PersistentTimeSeries` timestamp vector — must also be a whole number of milliseconds;
`microsecond` must be a multiple of 1000. A finer instant raises `InvalidParameterError` rather than
being silently truncated, because it cannot survive every binding intact (see
[timestamp precision](../explanation/time-series-types.md#timestamp-precision)). Note that
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
    time_series: SingleTimeSeries | NonSequentialTimeSeries | PersistentTimeSeries
        | Deterministic | Probabilistic | Scenarios,
    *,
    features: dict[str, int | float | bool | str] | None = None,
) -> int: ...   # the catalog id its row was filed under
# `features` is the only thing this call adds. `name` and every descriptive
# attribute -- `units`, `quantity_kind`, `unit_system`, `component_field`,
# `application_data`, `element_type`, `time_reference` -- come off the
# time_series object, where they were set at construction. That is what makes a
# read-then-add lossless: a series read from one store can be added to another
# unchanged, with nothing to re-supply.
# A `features` key that shadows a time-series or identity field (`name`,
# `resolution`, `owner_id`, ...) raises InvalidParameterError.

def add_time_series_bulk(self, items: list[dict]) -> list[int]: ...
# Each item dict mirrors add_time_series's parameters: required `owner_id`,
# `owner_type`, `owner_category`, `time_series`; optional `features`. Any other
# key raises, as the misspelled keyword it almost always is.
# All items commit in ONE metadata transaction (all-or-nothing), which is much
# faster than looping over add_time_series. Results are in input order.

# Every write returns the catalog `id` its row was filed under -- the handle to
# record in your own object model, and what every read and removal
# takes. It is never reissued once its row is deleted. No add takes an id: the
# catalog assigns, and the write reports what it chose. The one writer that
# files rows under supplied ids is import_time_series_associations_openapi.

def get_metadata_by_id(self, id: int) -> dict | None: ...   # None when no row has the id
def list_metadata_by_ids(self, ids: list[int]) -> list[dict]: ...
# The listing addressed by id, in the order given; NotFoundError if any is stale.
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
    src: int,
    dst_owner_id: int,
    dst_owner_type: str,
    *,
    new_name: str | None = None,
) -> int: ...
# Attach the same array to another owner (no data is duplicated); returns the
# copy's own id. The source id is untouched and still resolves.

def get_array_by_hash(self, data_hash: str) -> numpy.ndarray: ...
# The raw array behind a 64-char hex content hash, bypassing the catalog.
def count_array_references(self, data_hash: str) -> dict: ...
# {"sts": int, "dst": int}: SingleTimeSeries and DeterministicSingleTimeSeries
# associations sharing that array.
def read_by_ids_range(
    self, ids: list[int], time_range: tuple[datetime, datetime]
) -> list[SingleTimeSeries | NonSequentialTimeSeries | PersistentTimeSeries
          | Deterministic | Probabilistic | Scenarios]: ...
# The bounds read: it CLIPS to what falls between the two instants, where
# read_by_id's window is CHECKED. Both bounds must be spelled the way the series
# are; a selection spanning both coherence groups is refused. A PersistentTimeSeries
# clips on its own terms: the result begins at the breakpoint in force at `start`.

def read_by_ids(
    self, ids: list[int]
) -> list[SingleTimeSeries | NonSequentialTimeSeries | Deterministic | Probabilistic | Scenarios]: ...
# The same read addressed by catalog association id. Results follow the order the
# ids are given, repeats included; NotFoundError if any id names no row.

def read_by_id(
    self,
    id: int,
    *,
    start_time: datetime | None = None,
    len: int | None = None,
    count: int | None = None,
    owner_id: int | None = None,
    owner_category: OwnerCategory | None = None,
) -> SingleTimeSeries | NonSequentialTimeSeries | Deterministic | Probabilistic | Scenarios: ...
# The single-id read, which also takes the slice -- in one call, because the
# primary-key lookup already returns the row the window resolves against. `len`
# counts timesteps (static types) and `count` counts windows (forecasts);
# passing the one that does not apply raises InvalidParameterError, as does a
# `start_time` off the series' grid or an extent past its end. A window is
# checked where read_by_ids_range clips. No keywords reads the whole series.
# `owner_id` + `owner_category` is the owner guard -- see below.

def remove_by_ids(
    self,
    ids: list[int],
    *,
    owner_id: int | None = None,
    owner_category: OwnerCategory | None = None,
) -> int: ...
# One all-or-nothing transaction: NotFoundError if any id names no row, and
# nothing removed. A repeated id is removed, and counted, once. `owner_id` +
# `owner_category` is the owner guard -- see below.
def remove_by_filter(self, *, ...) -> int: ...
# Same keyword-only filter arguments as list_metadata; one all-or-nothing
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

def list_metadata(
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

def list_names(self, *, ...) -> list[str]:  ...        # distinct names, sorted
def list_owner_types(self, *, ...) -> list[str]: ...   # distinct owner types, sorted
# Every `...` above is the same keyword-only filter as list_metadata, and so
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

def has_any_time_series(self, *, ...) -> bool: ...
# Existence without listing ("does this owner have any time series?"); same
# keyword-only filter arguments as list_metadata. Index-probe fast.
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
def show(self, *, file=None) -> None: ...
# Prints the above as a summary — see below. `file` is any writable object,
# defaulting to sys.stdout; it is handed straight to print.
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
def persist_arrays_to(self, path: str) -> None: ...
# Writes only the array half, leaving no catalog beside it — the write-side
# counterpart of Store.open_without_catalog, for shipping an artifact as arrays
# plus a document of your own. Atomic: one file, one rename. StoreExistsError if
# a <path>.sqlite is already there, since its rows would be left dangling.
def persist_catalog(self) -> None: ...
# Writes an in-memory catalog to this store's own <path>.sqlite, stamped to
# match the HDF5 file already beside it. Unlike persist_to, writes no arrays:
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
    store.remove_by_ids([old_id])
# both applied, or neither -- including the removal
```

> **Keyword-only arguments.** Every optional argument in the binding is keyword-only (the `*`
> marker): filter kwargs, `features=` on the add paths, `units=`/`application_data=` and the rest of
> the descriptors on the value constructors, `time_range=` on the read paths, and so on. Positional
> use raises `TypeError`. The wheel ships a `infrastore.pyi` stub, so IDEs and type checkers see the
> full signatures.

#### Return shapes

- **`add_time_series`** accepts a `SingleTimeSeries`, a `NonSequentialTimeSeries`, a
  `PersistentTimeSeries`, or a dense forecast object (`Deterministic` / `Probabilistic` /
  `Scenarios`) — see [Forecasts](#forecasts). **`transform_single_time_series`** derives a
  `DeterministicSingleTimeSeries` from every stored `SingleTimeSeries` (or the subset its
  `owner_category` / `resolution` arguments select) and returns the count transformed.
  **`read_by_id`** returns whichever matches the stored type — a read names only an id, so the row's
  own `time_series_type` decides, with no requested type to disagree with it.
- **`read_by_ids`** returns one typed object per id, in the order the ids are given, repeats
  included (an empty id list returns an empty list). It is the bulk counterpart to `read_by_id`:
  packed `SingleTimeSeries` are read in one decompress-once pass per dataset instead of one read
  each. An id naming no row raises `NotFoundError` and fails the whole call, unlike
  `association_exists`, which asks the question rather than committing to a read.
- **`read_by_ids_range`** is the bounds read: it _clips_ every series to what falls between the two
  instants, where `read_by_id`'s window is _checked_. An export names bounds and does not know how
  many steps each series has inside them.
- **`remove_by_ids`** is the removal direction of the same reference: one all-or-nothing
  transaction, the count removed, and `NotFoundError` if any id names no row — in which case nothing
  is removed. A repeated id is removed, and counted, once.
- **The owner guard.** Both id-addressed calls take an optional keyword-only `owner_id` +
  `owner_category` — both together, or neither, since a component and a supplemental attribute can
  carry the same integer id and half an owner would check less than the caller asked for:

  ```python
  store.read_by_id(id, owner_id=7, owner_category=OwnerCategory.Component)
  store.remove_by_ids(ids, owner_id=7, owner_category=OwnerCategory.Component)
  ```

  The addressed row is held to that owner and one belonging to anyone else raises
  `OwnerMismatchError` — distinct from `NotFoundError`, because the row is there and it is the
  caller's belief about who owns it that is stale. For the removal the check and the delete are one
  transaction, so a refused batch removes nothing.

  A caller whose model says "this component's series" must pass the owner rather than confirm it in
  a call of its own. An id is the whole address and it survives `replace_owner`, so a
  `get_metadata_by_id` that confirms the owner and a `remove_by_ids` that then deletes are two calls
  with a window between them — and a reassignment landing in that window makes the removal retire
  the _new_ owner's series, the very thing the check was for. On the read side there is no window
  either way, but the guard is still the cheaper spelling: the owner comes off the same row the
  values are materialized from, where a separate check is a second round trip.
- **`list_metadata`** returns a list of dicts (the same shape `get_metadata_by_id` returns for one
  row), each with the keys: `owner_id`, `owner_type`, `owner_category`, `time_series_type`, `name`,
  `data_hash` (hex string), `initial_timestamp` (RFC 3339 string, or `None` for non-sequential
  series), `length`, `resolution` (ISO 8601 duration string, e.g. `PT1H`, or `None`), `timestamps`,
  `horizon`, `interval`, `count`, `percentiles`, `element_type`, `element_shape`, `features`,
  `units`, `quantity_kind`, `unit_system` (`"natural_units"` / `"component_base"` / `None`),
  `time_reference`, `component_field`, `application_data`. `timestamps` is a list of RFC 3339
  strings for non-sequential series and `None` otherwise; `horizon` / `interval` / `count` are set
  for forecasts and `percentiles` for `Probabilistic` only. `timestamps` is always `None` on a
  listing row — an irregular series' time axis is the one part of a row that costs a read per row,
  so a listing omits it and `read_by_id` returns the series with its axis. The `features` filter is
  a **subset** match — rows must contain at least the given pairs.
- Every row also carries `data_hash`, so grouping a listing by that field finds the series that
  share one stored array (a deduplicated array, or a `SingleTimeSeries` together with a
  `DeterministicSingleTimeSeries` derived from it). That replaced a separate array-group listing,
  which was this same query projected differently.
- **`list_metadata_by_ids`** is the same listing addressed by id, for a caller hydrating a model
  full of recorded references: one catalog query for the whole set rather than one call each.
- **`get_time_series_counts`** returns
  `{"components_with_time_series": int, "static_time_series": int, "forecasts": int}`;
  **`time_series_counts_detailed`** adds `supplemental_attributes_with_time_series` and spells the
  other two `static_time_series_count` / `forecast_count`.
- **`show`** prints those same counts as a block of text and returns `None`. It is the
  hand-inspection surface — what a store holds, at a glance, without composing four calls and
  formatting the result. See [`show()`](#show).
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
- **`read_by_ids_range`** with `time_range=(start, end)` slices on the time axis; `end` is
  exclusive.

### `show()`

`show()` prints what the store holds. It composes nothing you cannot ask for individually — the
counts come from `counts_by_type`, `time_series_counts_detailed`, `num_distinct_arrays`, and the two
association `count_*` methods — but it is the one call to reach for at a REPL or in a log line:

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

Every number is a catalog aggregate query, so the cost does not grow with how much data the store
holds; no array is read. The type breakdown lists only the types actually present, static types
before forecasts — not the numeric type-code order `counts_by_type` returns, which puts
`PersistentTimeSeries` after the forecasts. An empty store reports `Time series: none` rather than a
zero, and the header reads `(read-only)` for a store opened that way. Writing elsewhere is the
`file` argument, handed straight to `print`:

```python
store.show(file=sys.stderr)
```

The output is meant for a person to read; parse the `count_*` methods instead if you need the
numbers.

## `SingleTimeSeries`

```python
SingleTimeSeries(
    initial_timestamp: datetime,
    resolution: timedelta,
    data: numpy.ndarray,   # shape (length,) or (length, k1, ...)
    name: str,
    *,
    application_data: str | None = None,
    element_type: str | None = None,
    units: str | None = None,
    quantity_kind: str | None = None,
    unit_system: str | None = None,      # "natural_units" | "component_base"
    component_field: str | None = None,  # e.g. "max_active_power"
    time_reference: str | None = None,   # "utc" | "zoneless" | "-07:00" | "America/Denver"
)
```

Read-only properties: `initial_timestamp -> datetime`, `resolution -> str` (ISO 8601 duration, e.g.
`PT1H`), `length -> int`, `data -> numpy.ndarray`, `timestamps -> list[datetime]`, `name -> str`,
plus the seven [descriptive attributes](#descriptive-attributes). `initial_timestamp` comes back
spelled the way it was written — see [Time references](#time-references). The constructor accepts
either a `timedelta` or an ISO 8601 duration string for `resolution`; the getter always returns the
ISO string. `name` is a required association attribute (the same array may be stored under different
names). It is read off the object by `add_time_series` and populated on `read_by_id`. The array's
`element_type` and per-step element shape are preserved through a round-trip.

### `SingleTimeSeries.from_timestamps`

```python
@classmethod
def from_timestamps(
    cls, timestamps: Sequence[datetime], data: numpy.ndarray, name: str, **descriptors
) -> SingleTimeSeries: ...
```

Build from the timeline you actually hold, inferring `resolution` and **proving** the instants lie
on it. The constructor takes `initial_timestamp` + `resolution` and the store cannot check that
claim — the vector it describes is never supplied. This takes the vector: it either fits a period
exactly, or raises `InvalidParameterError` naming the entry that broke the pattern and pointing at
`NonSequentialTimeSeries`.

**This is how a local-clock timeline reaches the store.** The core has no time-zone database and
never runs local → instant; you materialize the grid with `zoneinfo` — where the policy for a
nonexistent or ambiguous wall clock belongs — and hand over the instants.

```python
denver = ZoneInfo("America/Denver")

# An hourly local grid IS a uniform instant grid, so it compacts.
hours = local_hourly_walk(datetime(2024, 11, 3, tzinfo=denver), 6)
SingleTimeSeries.from_timestamps(hours, values, "load").resolution   # "PT1H"

# A daily one is not, and says so.
days = [datetime(2024, 11, d, tzinfo=denver) for d in range(1, 6)]
SingleTimeSeries.from_timestamps(days, values, "peak")               # InvalidParameterError
```

> **Step the timeline in instants, not wall clocks.** `aware_datetime + timedelta(hours=1)` is
> **wall-clock** arithmetic in Python: on a fall-back day it jumps 01:00 straight to 02:00 and
> silently drops a real hour. Go through UTC —
> `(t.astimezone(timezone.utc) + step).astimezone(zone)` — or `from_timestamps` will (correctly)
> refuse the result.

A fixed span wins when both a fixed and a calendar reading fit; two entries always fit some fixed
span, so pass an explicit `"P1M"` to the constructor if you mean calendar months on a short vector.

`timestamps` materializes the grid — `initial_timestamp + k · resolution`, spelled the way the
series was written. It is the only correct way to rebuild the timeline: a `P1M` resolution steps on
the **calendar**, so a January 31st series lands on February 29th, and multiplying a fixed span by
the index would get it wrong.

### Descriptive attributes

Every value type — the three static ones and all three forecasts — takes the same seven keyword-only
arguments and exposes each as a read-only property. They describe the values without addressing
them, so none is part of a series' identity: two series differing only in these are a duplicate, and
none can be filtered on except `component_field`.

| Argument           | Meaning                                                                                                                    |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------- |
| `units`            | Free-form label for the values, e.g. `"MW"`. Never interpreted or validated.                                               |
| `quantity_kind`    | What kind of physical quantity they measure, e.g. `"ActivePower"`. QUDT `QuantityKind` local names are recommended.        |
| `unit_system`      | `"natural_units"` or `"component_base"`. Omitted leaves the basis **unspecified**, which is not the same as natural units. |
| `component_field`  | The owning component's field these values are the time-varying form of, e.g. `"max_active_power"`. The one filterable one. |
| `application_data` | Opaque, package-owned payload (typically JSON) stored verbatim. End users are not expected to set it.                      |
| `element_type`     | What the array's elements mean, e.g. `"tuple(3,f64)"`. Omit for plain numbers; the property then reports the dtype.        |
| `time_reference`   | Overrides the spelling otherwise inferred from the timestamps. See [Time references](#time-references).                    |

An unrecognized `unit_system` raises `InvalidParameterError` rather than degrading to unspecified.
`element_type` is the only property that never returns `None`: it is always concrete, reporting the
array's own dtype spelling for a plain numeric series.

These live on the object rather than on `add_time_series` so that a read-then-add is lossless — a
series read from one store can be added to another unchanged, with no descriptor to re-supply and
none that a write could silently replace.

### `to_arrow()`

All three static types — `SingleTimeSeries`, `NonSequentialTimeSeries`, and `PersistentTimeSeries` —
convert to a two-column `pyarrow.Table` of `timestamp` and `value`:

```python
table = series.to_arrow()
table.to_pandas()           # if pandas is installed
polars.from_arrow(table)    # if polars is
pyarrow.parquet.write_table(table, "series.parquet")
```

**pyarrow is an optional extra.** It is not installed with infrastore — it is several times the size
of the wheel that would pull it in, and the binding's own currency is numpy arrays. Install it with
`pip install 'infrastore[arrow]'`; calling `to_arrow()` without it raises `ImportError` naming the
extra. Nothing else in the package imports pyarrow.

**The timestamp column carries the series' spelling.** Arrow's `timestamp(unit, tz)` is the same
shape as the store's own model — an instant plus how it was spelled — so the mapping is total, and
millisecond unit throughout means nothing is widened or truncated:

| `time_reference`   | Arrow column type                  |
| ------------------ | ---------------------------------- |
| unset, or `"utc"`  | `timestamp[ms, tz=UTC]`            |
| `"zoneless"`       | `timestamp[ms]` (no zone)          |
| `"-07:00"`         | `timestamp[ms, tz=-07:00]`         |
| `"America/Denver"` | `timestamp[ms, tz=America/Denver]` |

A zone this interpreter's tz database does not know warns and falls back to UTC, exactly as reading
`initial_timestamp` does: the instants are intact either way. For a `SingleTimeSeries` the column is
the materialized grid (calendar-aware for a monthly resolution); for the two irregular types it is
the stored vector.

**The value column is the array.** A scalar series gives an Arrow primitive (`double`, `int64`,
`bool`, …); a multidimensional per-timestep value gives nested `fixed_size_list`, one level per
element dimension. Composite element types stay in their stored packing — `decode_element_values`
unpacks them, and `element_type` in the metadata says which.

**The descriptive attributes ride in `table.schema.metadata`**, so the table is not lossy against
the object it came from and survives a Parquet round trip: `name`, `time_series_type`,
`element_type`, `time_reference`, `resolution` (a `SingleTimeSeries` only — an irregular timeline
has no constant step), and whichever of `units`, `quantity_kind`, `unit_system`, `component_field`,
and `application_data` were declared. An undeclared one is **absent** rather than empty, so
`b"units" in table.schema.metadata` answers "was a label declared?".

`time_reference` is the exception: it is always written, and a series that records no spelling gets
the literal `b"unspecified"`. Arrow's timestamp type has a zone or it has none, and _unspecified_
has no third spelling — an unspecified reference produces a UTC-zoned column, the same as `"utc"` —
so omitting the key would leave that column as the only evidence and `from_arrow` would hand back a
series claiming `utc`, which it never did. `unspecified` is a metadata encoding only: it is not a
value `time_reference=` accepts on any constructor.

The value column is named `value` rather than after the series so that tables from different
components concatenate without renaming; the series' own name is in the metadata.

A `PersistentTimeSeries` table has **one row per breakpoint, not per instant** — it is the sparse
step function as stored. Resampling onto a dense grid is the caller's to do, and needs a grid the
series does not carry: there is no value before the first breakpoint, so a grid starting earlier has
no answer to give.

Also written, and not part of the descriptive set above: `element_shape`, as a JSON list (`[]` for a
scalar element). It is a fact about the data rather than a label, so it is written even when empty,
and the CLI's Parquet export writes the same key — the two producers write one schema.

### `from_arrow()`

The inverse, on the same three types, and a reader of **foreign** tables too — anything with a
`timestamp` and a `value` column, whether or not it carries the metadata `to_arrow()` writes:

```python
series = SingleTimeSeries.from_arrow(series.to_arrow())            # exact round trip

import pyarrow.parquet as pq
SingleTimeSeries.from_arrow(pq.read_table("load.parquet"))         # written by anything
NonSequentialTimeSeries.from_arrow(table, name="irregular")
PersistentTimeSeries.from_arrow(table, name="steps")
```

The rules are the same ones `infrastore add --parquet` applies, and they are stated once in the
[CLI reference](cli.md#parquet-import) so the two implementations cannot drift apart quietly. In
short: what the metadata says is used; what it does not say is inferred from the Arrow schema,
taking the reading that assumes least.

| Missing          | Read as                                                                                                                                                                         |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `resolution`     | Inferred from the timestamps, which must then walk a grid.                                                                                                                      |
| `element_type`   | The leaf Arrow type. A `fixed_size_list<double>[3]` becomes `f64` with shape `(rows, 3)` — _dense_, not `tuple(3,f64)`, because the bytes cannot say.                           |
| `time_reference` | The timestamp column's zone; a column with no zone reads as `zoneless`, since a naive timestamp is a wall clock. A metadata `unspecified` beats the zone and gives back `None`. |
| `name`           | Nothing. A name is part of a series' identity, so pass `name=`.                                                                                                                 |

Every keyword overrides the metadata, with one exception: **`element_type` is an assertion.**
`element_type="tuple(3,f64)"` states the reading the bytes cannot, and a value that contradicts the
table's own raises `InvalidParameterError` rather than replacing it — the rule this project applies
to every assertion.

Refused rather than coerced: **nulls** in either column (the store holds none, and NaN is a value
rather than an absence); a **microsecond or nanosecond timestamp that is not a whole millisecond**
(the store's own precision, and rounding one would move it); **rows that leave a declared grid**,
checked against the grid the resolution generates rather than against successive differences, since
`P1M` clamps to month end; and **`struct`/`list` value columns**, which are the decoded form
`to_arrow()` does not produce.

`from_arrow` is not the whole catalog row: a table records no owner and no catalog id, because
`to_arrow()` is a method on a value object and a series built here is not filed anywhere. Pass the
result to `Store.add_time_series` with the owner you want, as you would any other series.

`from_arrow` covers the three static types. A **dense forecast** has a file shape of its own -- a
long table of `issue_time`, `target_time`, `value` (plus `percentile` or `scenario`), which the CLI
writes and reads with `export -f parquet` and `add --parquet`. `to_arrow_windows()` is deliberately
not that shape: it returns a dict of per-window tables, which is an in-memory analysis form rather
than anything that could be one Parquet file, so the two are not competing spellings of the same
thing.

## `NonSequentialTimeSeries`

```python
NonSequentialTimeSeries(
    timestamps: list[datetime],
    data: numpy.ndarray,
    name: str,
    *,
    application_data: str | None = None,
    element_type: str | None = None,
    units: str | None = None,
    quantity_kind: str | None = None,
    unit_system: str | None = None,      # "natural_units" | "component_base"
    component_field: str | None = None,  # e.g. "max_active_power"
    time_reference: str | None = None,   # "utc" | "zoneless" | "-07:00" | "America/Denver"
)
```

Read-only properties: `timestamps`, `length`, `data`, `name`, and the seven
[descriptive attributes](#descriptive-attributes), plus [`to_arrow()`](#to_arrow). Timestamps must
be strictly increasing, match the first data dimension, and agree on one spelling — a vector mixing
naive and aware values raises `InvalidParameterError`, since one series records one reference.
`read_by_id` returns this class for a non-sequential row.

## `PersistentTimeSeries`

```python
PersistentTimeSeries(
    timestamps: list[datetime],
    data: numpy.ndarray,
    name: str,
    *,
    application_data: str | None = None,
    element_type: str | None = None,
    units: str | None = None,
    quantity_kind: str | None = None,
    unit_system: str | None = None,      # "natural_units" | "component_base"
    component_field: str | None = None,  # e.g. "max_active_power"
    time_reference: str | None = None,   # "utc" | "zoneless" | "-07:00" | "America/Denver"
)
```

A sparse **step function**. Constructed exactly like a `NonSequentialTimeSeries` — same arguments,
same validation, same spelling inference — with the same read-only properties: `timestamps`,
`length`, `data`, `name`, and the seven [descriptive attributes](#descriptive-attributes), plus
[`to_arrow()`](#to_arrow). `timestamps` is the breakpoint vector, and so are the rows of the Arrow
table — one per breakpoint, not per instant. A step function's scalar-collapse policy belongs in
`application_data`; the store has no column for it.

What differs is the read: the value at breakpoint `i` is in force until breakpoint `i + 1`, and past
the last one forever, where a `NonSequentialTimeSeries` has no value between its timestamps at all.
There is no value before the first breakpoint, and asking for one raises `InvalidParameterError`
rather than clamping.

Three methods ask that question of a series in hand:

```python
value_at(at: datetime) -> Any        # the value in force at `at`
index_at(at: datetime) -> int        # the row it came from
breakpoint_at(at: datetime) -> datetime  # the instant it has been in force since
```

`value_at` is the everyday call, and it is not an approximation: a step function is defined at
_every_ instant from its first breakpoint onward, so it has a genuine value at `at`. It returns
exactly what indexing `data` returns — a numpy scalar of the series' own dtype, or the per-step
subarray for a series with a shaped element. `at` must be spelled the way the breakpoints are (both
aware or both naive), the same rule a `time_range` bound follows. Only an `at` strictly _before_ the
first breakpoint raises.

```python
curve = PersistentTimeSeries(
    [datetime(2024, 1, 1), datetime(2024, 4, 1), datetime(2024, 7, 1)],
    np.array([10.0, 40.0, 70.0]),
    "gas",
)
curve.value_at(datetime(2024, 5, 17))       # 40.0, carried forward from April
curve.breakpoint_at(datetime(2024, 5, 17))  # datetime(2024, 4, 1)
```

A range read slices on those terms — the returned series begins at the breakpoint _in force at_
`start`, so it always defines a value there:

```python
sliced, = store.read_by_ids_range([id], (mid_april, september))
# sliced.timestamps[0] is the April breakpoint, not the July one.
```

A zero-width range (`end == start`) is the exception, and selects nothing — as it does for every
other type, since `[t, t)` holds no instant for a value to be in force at. That applies before the
first breakpoint too, where a non-empty window raises.

Policy about how a step function collapses for a downstream solver belongs to the application and
travels in `application_data`; the store never interprets it. See the
[time-series types](../explanation/time-series-types.md#persistenttimeseries).

## Enums

```python
TimeSeriesType.SingleTimeSeries
TimeSeriesType.NonSequentialTimeSeries
TimeSeriesType.PersistentTimeSeries
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
passed to [`add_time_series`](#methods). They are read back through `read_by_id`, which returns the
matching object for the row's stored type (a `DeterministicSingleTimeSeries` is synthesized into a
`Deterministic` on read). A `DeterministicSingleTimeSeries` is not added directly — derive one from
stored `SingleTimeSeries` with [`transform_single_time_series`](#methods).
[`get_time_series_counts`](#methods) reports the forecast total under `forecasts`.

```python
ts = Deterministic(
    initial_timestamp, resolution, horizon, interval, count, data, "load_fc", units="MW"
)
series_id = store.add_time_series(42, "Generator", OwnerCategory.Component, ts)
```

`data` is a NumPy array in the canonical shape for the forecast type, where `H` is
`horizon / resolution`. As with `SingleTimeSeries`, every period argument (`resolution`, `horizon`,
`interval`) accepts either a `timedelta` or an ISO 8601 duration string — the string form is
required for calendar periods such as `"P1M"` — and the getters always return the ISO string. Every
forecast also takes a required `name` (after `data`), exposed as a read-only property, and the same
seven keyword-only [descriptive attributes](#descriptive-attributes) as the static types.

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
    *,
    application_data: str | None = None,
    element_type: str | None = None,
    units: str | None = None,
    quantity_kind: str | None = None,
    unit_system: str | None = None,
    component_field: str | None = None,
    time_reference: str | None = None,
)
```

Read-only properties (plus the seven [descriptive attributes](#descriptive-attributes)):

```python
forecast.initial_timestamp -> datetime
forecast.resolution        -> str   # ISO 8601 duration, e.g. "PT1H"
forecast.horizon           -> str   # ISO 8601 duration
forecast.interval          -> str   # ISO 8601 duration
forecast.count             -> int
forecast.data              -> numpy.ndarray
forecast.name              -> str
```

#### `to_arrow_windows()`

```python
Deterministic.to_arrow_windows() -> dict[datetime, pyarrow.Table]
```

The forecast as one `pyarrow.Table` per window, keyed by **issue time** —
`initial_timestamp + k · interval`, spelled the way the series was written. Requires the same
[`arrow` extra](#to_arrow) as the static types.

```python
windows = forecast.to_arrow_windows()
windows[datetime(2024, 1, 2, tzinfo=timezone.utc)]   # that window's forecast
for issue_time, table in windows.items(): ...        # chronological
```

Each value is a two-column `timestamp`/`value` table shaped **exactly like a
`SingleTimeSeries.to_arrow()`** — `horizon / resolution` rows stepping by `resolution` from the
issue time — so one window drops into anything that already consumes a static table.

The dict is in window order, which Python's insertion-ordered `dict` makes an ordering you can rely
on: `next(iter(windows))` is the earliest issue time and iteration is chronological. It is not a
sorted _container_, so there is no O(log n) range lookup; `bisect` over `list(windows)` selects a
span of issue times.

**Two grids, both needed to place a value.** Windows step by `interval`; the rows inside one window
step by `resolution`. They coincide only for a forecast whose windows abut without overlapping,
which is not the common case — a day-ahead forecast reissued hourly overlaps 23 of every 24 rows.
The tables repeat those instants rather than pretending one timeline covers them, which is why this
is a dict of tables rather than a single table.

Each table carries the forecast's descriptive attributes as schema metadata, plus `resolution`,
`horizon`, `interval`, `count`, and its own `issue_time` — so a window written to Parquet on its own
still knows which one it is.

This materializes every window. The stored array is `[H, count, *E]` — window index innermost — so
it is transposed once on the way out; for a per-timestamp sweep the cheap path is
[`build_forecast_reader`](#readers), which reads along the axis the data is already laid out on.
`DeterministicSingleTimeSeries` rows read back as a `Deterministic`, so they convert the same way.
`Probabilistic` and `Scenarios` do not have this yet — their windows carry a third axis, and how to
spell it is an open question.

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
    *,
    application_data: str | None = None,
    element_type: str | None = None,
    units: str | None = None,
    quantity_kind: str | None = None,
    unit_system: str | None = None,
    component_field: str | None = None,
    time_reference: str | None = None,
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
    *,
    application_data: str | None = None,
    element_type: str | None = None,
    units: str | None = None,
    quantity_kind: str | None = None,
    unit_system: str | None = None,
    component_field: str | None = None,
    time_reference: str | None = None,
)
```

Same properties as `Deterministic`, plus:

```python
forecast.scenario_count -> int
```

## Readers

`read_by_id` returns one whole series or forecast. For the simulation access pattern — _walk every
timestamp and, at each, read the value of every matching series_ — use a **reader** instead. A
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
    window_start: datetime | None = None,             # sweep a named span instead of
    window_length: int | None = None,                 # inheriting the shared grid
    time_series_type: TimeSeriesType | None = None,   # default: SingleTimeSeries
    owner_id: int | None = None,
    owner_category: OwnerCategory | None = None,
    owner_type: str | None = None,
    name: str | None = None,
    name_glob: str | None = None,
    component_field: str | None = None,
    initial_timestamp: datetime | None = None,        # select one grid, dropping
    length: int | None = None,                        # the series not on it
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
its timeline is the timestamp vector its cohort shares instead. Likewise for
`TimeSeriesType.PersistentTimeSeries`, whose timeline is the union of its columns' breakpoints.
`static_read` / `forecast_read` fill the reader's buffers in place and return `None`; passing a
`when` that is off the reader's timeline raises `InvalidParameterError`.

### `StaticReader`

Reads the value of every matching static series at one timestamp. Results are **columnar**: series
are partitioned into `(dtype, element_shape)` groups, and each group's values come back as one dense
`(num_columns, *element_shape)` numpy array.

```python
class StaticReader:
    def grid(self) -> dict: ...     # {"time_series_type": str, "initial_timestamp": rfc3339 str, "resolution": ISO str | None, "length": int}
    def groups(self) -> list[dict]: ...  # each: {"dtype": str, "element_type": str, "element_shape": list[int], "ids": list[int]}
    def timestamps(self) -> list[datetime]: ...   # every timestamp on the timeline, in order
    def group_values(self, index: int) -> numpy.ndarray: ...  # last read of group `index`
```

All matched series must share one timeline — one grid (`initial_timestamp` + `length`) for
`SingleTimeSeries`, one timestamp vector for `NonSequentialTimeSeries`. The build validates this and
raises on divergence, so there is no presence mask — every column has a value at every valid
timestamp. When they do not share one there are two remedies, and they answer different questions:
[a reader window](#reader-windows) sweeps a span across the ragged series, while the
[`initial_timestamp` / `length` filter](#selecting-one-grid) drops the ones that are not on the grid
you want.

`PersistentTimeSeries` is the exception: its columns may sit on **different** breakpoint vectors,
because a step function has a value at every instant from its first breakpoint on. `timestamps()` is
then the sorted union of every column's breakpoints, and each column reports the value in force
there. Reading before some column's first breakpoint raises `InvalidParameterError` naming that
column. There is still no presence mask.

`grid()["resolution"]` is `None` for an irregular or persistent reader; `timestamps()` is the
timeline in every case, so a read loop written against it works unchanged for all three.
`group_values(i)` returns a `(num_columns, *element_shape)` array whose column `j` corresponds to
`groups()[i]["ids"][j]`; it is empty until the first `static_read`.

```python
# For irregular series: build_static_reader(time_series_type=TimeSeriesType.NonSequentialTimeSeries)
# For step functions:   build_static_reader(time_series_type=TimeSeriesType.PersistentTimeSeries)
reader = store.build_static_reader(timedelta(hours=1))
grid = reader.grid()
groups = reader.groups()
start = datetime.fromisoformat(grid["initial_timestamp"])
for ts in reader.timestamps():
    store.static_read(reader, ts)
    for i, g in enumerate(groups):
        vals = reader.group_values(i)   # column j ↔ g["ids"][j]
```

#### Reader windows

A shared grid is a strong requirement, and a real system rarely meets it: a year of load sits beside
a week of an outage schedule, or one component's data begins an hour later than the rest. Passing
`window_start` (and optionally `window_length`) drops the requirement. The reader's axis becomes the
span you named, and each column reads at an **offset of its own** — how many of its own steps
precede the anchor — so series that begin at different instants, or run for different lengths, sweep
together as long as they all cover the span.

```python
# 24 hours of one series, a leap year of another: no shared grid, so no reader.
store.build_static_reader("PT1H")
# InvalidParameterError: StaticReader requires a uniform grid; series 'load' (owner 7)
# has grid (2024-01-01T00:00:00Z, PT1H, 24) but the reader grid is
# (2024-01-01T07:00:00Z, PT1H, 8784). Build the reader over a window ...

reader = store.build_static_reader("PT1H", window_start=datetime(2024, 1, 1, 7, tzinfo=utc))
reader.grid()["length"]   # 17 -- as far as *every* matched series reaches from 07:00
```

Without `window_length` the reader runs as far from the anchor as every matched series reaches,
which is the widest span on which no column has to be dropped. With one, the span is exactly what
you asked for and is **checked**, in the three ways that would otherwise return a full, plausible,
wrong row:

- a matched series that does not cover the span raises `InvalidParameterError` **naming that
  series**, rather than quietly leaving its column out — an absent column is invisible at read time;
- the anchor must fall at or after each series' start and on one of its own step boundaries. A
  timestamp part-way through a step is an error, not a floor. (`read_by_id` floors, because there a
  value covers its step; a reader hands back a whole cohort at one instant, so flooring per column
  would shift columns against each other by up to a step.)
- a monthly resolution is refused where re-anchoring would move the dates, by the same rule that
  governs a sliced `read_by_id` — a monthly grid from Jan-31 re-anchored at its own Feb-29 would
  read Feb-29, Mar-29, Apr-29.

`window_start` must be spelled the way the series are (aware for a zoned series, naive for a
zoneless one), like every other query bound, and belongs to `SingleTimeSeries` alone: the two
irregular types carry their timeline rather than deriving it, so there is nothing to re-anchor.
`window_length` without `window_start` is refused — a span with no anchor is the ambiguity this
argument exists to remove.

Everything downstream is unchanged: `grid()` reports the window, `timestamps()` walks it, and
`groups()` still lists every matched column.

#### Selecting one grid

The window's counterpart. `initial_timestamp` and `length` are **filter** arguments — they match
only the series already on that grid, so the ones that are not on it never become columns:

```python
# 24 hours of stray data beside two full leap years, all named active_power
store.build_static_reader("PT1H")                                # InvalidParameterError
store.build_static_reader("PT1H", window_start=t7).grid()        # 3 columns, 17 steps
store.build_static_reader("PT1H", initial_timestamp=t7).grid()   # 2 columns, 8784 steps
```

Use the **window** when the ragged series should all take part in the sweep, and the **filter** when
they should not. They compose: filter to a cohort, then window a span inside it.

With `resolution` these two complete the grid triple, which is what lets a filter name a whole grid
rather than only be refused a divergent one — the role `zoneless` plays for time-reference
coherence. They are ordinary filter arguments, so they reach every filter-taking call
(`list_metadata`, `list_names`, `has_any_time_series`, `remove_by_filter`, …), and like every filter
they _select_ rather than assert: a grid no row is on is an empty result, not an error. A row that
stores no `initial_timestamp` — the two irregular types — matches no value at all, the same
SQL-equality trap [`component_field`](#methods) has.

```python
store.list_metadata(initial_timestamp=t7, length=8784)   # the cohort
store.remove_by_filter(initial_timestamp=t0, length=24)  # retire the stray one
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
    def entries(self) -> list[int]: ...   # per-entry catalog ids, in order (parallel to entry_values)
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

### Store attributes

Key/value provenance about the **artifact as a whole**, as opposed to a supplemental attribute
(which belongs to a component) or `application_data` (which belongs to one series). See
[Store attributes](../explanation/data-model.md#store-attributes) for the model.

```python
def set_store_attribute(self, key: str, value: str) -> None: ...
def get_store_attribute(self, key: str) -> str | None: ...
def list_store_attributes(self) -> dict[str, str]: ...
def remove_store_attribute(self, key: str) -> bool: ...
```

```python
store.set_store_attribute("creator", "sienna-build")
store.set_store_attribute("source_system", "WECC 2032 ADS")

store.get_store_attribute("creator")      # 'sienna-build'
store.get_store_attribute("absent")       # None — a question, not an exception
store.list_store_attributes()             # {'creator': ..., 'source_system': ...}
store.remove_store_attribute("creator")   # True; a second call is False
```

A `set` replaces rather than appending. `None` and `""` are different answers: a key set to the
empty string is present. An empty key or one beginning with `infrastore.` — reserved, on removal as
well as on write — raises `InvalidParameterError`; a write to a read-only store raises
`ReadOnlyStoreError`.

The store never interprets a value, so structure rides in the text:

```python
import json
store.set_store_attribute("provenance", json.dumps({"pipeline": "nightly", "run": 412}))
json.loads(store.get_store_attribute("provenance"))   # {'pipeline': 'nightly', 'run': 412}
```

### OpenAPI-row association serde

Direct JSON serde of the two association catalogs, in the wire spelling
[SiennaSchemas](https://github.com/Sienna-Platform/SiennaSchemas) defines (`TimeSeries/*.json`,
`Core/Associations/SupplementalAttributeAssociation.json`). Unlike `list_metadata` /
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

@classmethod
def open_without_catalog(cls, path: str, *, catalog: str = "attached") -> Store: ...
```

`export_time_series_associations_openapi` takes the same filter keywords as `list_metadata`. Every
row's `uri` and `data_hash` are the hex-encoded content hash the store already has for that row —
never a caller-supplied locator. With no filter this exports the whole catalog, sorted by identity —
except `PersistentTimeSeries` rows, which are omitted: the type is an infrastore-local extension the
wire contract has no schema for, so it cannot be spelled in a document. A filter naming that type is
an error rather than an empty array.

`export_supplemental_attribute_associations_openapi` exports the whole
`supplemental_attribute_associations` table, sorted by `(component_id, attribute_id)`;
`import_supplemental_attribute_associations_openapi` is its import half — a bulk, all-or-nothing
insert (a duplicate anywhere in the batch raises `DuplicateAssociationError` and rolls the batch
back), returning the number of rows inserted.

`import_time_series_associations_openapi` is the time-series import half, and it writes **rows
only**: the document carries locators, never values, so every row must name an array this store
already holds — the arrays arrive with the artifact. Each row keeps the `association_id` it carries,
which is the point: an import that assigned fresh ids would leave every reference the document
records pointing at the wrong series. An irregular series locates its time axis with
`timestamps_uri`, filled from the axis's own content hash: the axis is stored beside the arrays and
shared across a cohort, and the values cannot imply it — two irregular series with byte-identical
values on different axes share one content-addressed array. A row missing the locator, or naming an
axis the store does not hold, is refused. A `PersistentTimeSeries` row is refused before any of
that: the type is an infrastore-local extension, outside the six the wire contract defines, so a
document naming one is rejected by the discriminator check. Any of those, or an absent array, raises
`InvalidParameterError` and rolls the whole batch back.

Infrastore never modifies the data to make an incoming document agree with what it already holds. A
geometry disagreement between an added series and its own association row is likewise rejected at
the add boundary (`InvalidParameterError`), loudly and without writing anything.

Incoming rows are validated against the vendored SiennaSchemas specs before anything is decoded, so
a document that drifted from the contract is refused in the schema's own terms — naming the row and
the field.

### Reading a bundle back with no catalog

`Store.open_without_catalog` opens the array half of an artifact whose `.sqlite` is **absent**,
minting an empty catalog, so the document's rows can be replayed into it. This is what lets a
consumer ship arrays plus JSON and nothing else:

```python
store = Store.open_without_catalog("bundle.h5")
store.import_time_series_associations_openapi(ts_rows_json)
store.import_supplemental_attribute_associations_openapi(sa_rows_json)
```

`Store.open` cannot open that bundle — the array file carries a generation stamp and a catalog
created on the spot does not, so it reports a mismatched artifact. The catalog minted here inherits
the array file's own stamp, so every later `open` behaves normally. It raises `StoreExistsError`
when a catalog is already there; a store that has one wants `Store.open`.

A bundle carrying `NonSequentialTimeSeries` still needs its `.sqlite`: those rows cannot be
replayed, for the reason above.

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

| Exception                       | Raised when                                             |
| ------------------------------- | ------------------------------------------------------- |
| `NotFoundError`                 | A key or array does not exist                           |
| `OwnerMismatchError`            | An id-addressed call named an owner the row is not      |
| `DuplicateTimeSeriesError`      | Adding a series whose key already exists                |
| `DuplicateAssociationError`     | Re-adding an attachment or edge that already exists     |
| `InvalidParameterError`         | Bad arguments (bad feature type, malformed period, …)   |
| `IntegrityError`                | On-disk inconsistency detected                          |
| `ReadOnlyStoreError`            | A write on a read-only store                            |
| `IoError`                       | Filesystem I/O failure                                  |
| `ConnectionError`               | Connection failure (module-scoped, not the builtin)     |
| `IncompatibleFormatError`       | Store written in an incompatible on-disk format         |
| `IncompatibleForecastError`     | Forecast parameters clash with existing forecasts       |
| `StorageError`                  | SQLite catalog or serialization failure                 |
| `StoreExistsError`              | Creating a store where one already exists               |
| `MismatchedArtifactError`       | The `.h5` and `.sqlite` halves came from two saves      |
| `CatalogMigrationRequiredError` | Read-only open of a store whose catalog needs upgrading |
| `CatalogTooNewError`            | The catalog was written by a newer infrastore           |

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
