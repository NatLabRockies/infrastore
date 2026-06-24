# Julia API

The Julia package is **`TimeSeriesStore.jl`** (module `TimeSeriesStore`); it wraps the
[C ABI](./c-abi.md) cdylib. The library is resolved from the `TIME_SERIES_STORE_LIB` environment
variable (development builds), or from the `TimeSeriesStore_jll` binary package when installed.

```julia
using TimeSeriesStore
```

Exported names: `Store`, `SingleTimeSeries`, `NonSequentialTimeSeries`, `Deterministic`,
`DeterministicSingleTimeSeries`, `Probabilistic`, `Scenarios`, `TimeSeriesKey`, `OwnerCategory`,
`Component`, `SupplementalAttribute`, `add_time_series!`, `AddBatch`, `add_time_series_bulk!`,
`get_time_series`, `get_time_series_keys`, `key_info`, `remove_time_series!`, `has_time_series`,
`get_counts`, `counts_by_type`, `num_distinct_arrays`, `time_series_counts`, `list_owner_ids`,
`get_forecast_parameters`, `check_static_consistency`, `get_resolutions`, `get_compression`,
`verify_integrity`, `compact!`, `get_metadata`, `get_array_by_hash`, `open_store`, `flush!`,
`clear!`, `replace_owner!`, `transform_single_time_series!`, `has_typed`, `remove_typed!`, `close!`.

## Constructors

```julia
Store(; in_memory::Bool=true, path::Union{Nothing,AbstractString}=nothing,
        compression::Union{Symbol,AbstractString}=:deflate,
        compression_level::Integer=3, shuffle::Bool=true) -> Store
open_store(path::AbstractString; read_only::Bool=false) -> Store
```

- `Store()` — in-memory store.
- `Store(in_memory=false, path="system.nc")` — persists to `system.nc` plus `system.nc.sqlite`.
- `compression=:none` stores arrays uncompressed; `:deflate` (default) applies DEFLATE at
  `compression_level` (0–9) with optional byte `shuffle`. The policy is persisted with the store and
  reused on later appends; it is ignored for in-memory stores. An unknown `compression` throws
  `ArgumentError`.
- `open_store(path; read_only=true)` — opens an existing on-disk pair.

The store registers a finalizer; close it eagerly with `close!(store)`.

## Types

Each struct carries the association `name` (required). Construct with `name` as the positional after
`data`, and pass `logical_type=` as a keyword — e.g.
`SingleTimeSeries(initial, resolution, data, name; logical_type=nothing)`.

```julia
struct SingleTimeSeries
    initial_timestamp :: DateTime
    resolution        :: Period          # e.g. Hour(1), Millisecond(500)
    data              :: AbstractArray   # any element type; multi-dim allowed
    name              :: String          # required association name
    logical_type      :: Union{Nothing,String}
end

struct NonSequentialTimeSeries
    timestamps   :: Vector{DateTime}     # strictly increasing
    data         :: AbstractArray
    name         :: String
    logical_type :: Union{Nothing,String}
end

struct Deterministic
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Integer
    data              :: AbstractArray   # (H, count, element_dims...)
    name              :: String
end

struct Probabilistic
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Integer
    percentiles       :: Vector{Float64}
    data              :: AbstractArray   # (num_percentiles, H, count, element_dims...)
    name              :: String
end

struct Scenarios
    initial_timestamp :: DateTime
    resolution        :: Period
    horizon           :: Period
    interval          :: Period
    count             :: Integer
    data              :: AbstractArray   # (scenario_count, H, count, element_dims...); scenario_count from leading axis
    name              :: String
end

# Marker type; never constructed. Derived via transform_single_time_series! and
# read back as a Deterministic. Surfaces as a key's time_series_type.
abstract type DeterministicSingleTimeSeries end

mutable struct Store
    handle :: Ptr{Cvoid}
end

mutable struct TimeSeriesKey
    handle :: Ptr{Cvoid}                 # opaque; finalized automatically
end

@enum OwnerCategory begin
    Component             = 0
    SupplementalAttribute = 1
end
```

Every struct carries a required `name`, plus an optional `logical_type` — an opaque label the
binding can use to reconstruct a domain object on read. `add_time_series!` reads `name` off the
object (it is not a call argument), so the same array can be stored under different names. `data`
keeps its Julia element type: the binding maps it to a stored dtype (`Float64`, `Float32`, `Int64`,
`Int32`, `UInt64`, `Bool`) and converts to row-major bytes on the way down.

## Static Series

```julia
add_time_series!(
    store::Store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::SingleTimeSeries;
    features::AbstractDict = Dict(), units = nothing,
    logical_type = ts.logical_type,
) -> TimeSeriesKey

add_time_series!(
    store::Store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::NonSequentialTimeSeries;
    features = Dict(), units = nothing,
    logical_type = ts.logical_type,
) -> TimeSeriesKey

get_time_series(store::Store, key::TimeSeriesKey) -> SingleTimeSeries
get_time_series(SingleTimeSeries, store::Store, key::TimeSeriesKey) -> SingleTimeSeries
get_time_series(NonSequentialTimeSeries, store::Store, key::TimeSeriesKey) -> NonSequentialTimeSeries

get_time_series(SingleTimeSeries, store, owner_id, owner_category, name;
               resolution=nothing, features=Dict()) -> SingleTimeSeries
get_time_series(NonSequentialTimeSeries, store, owner_id, owner_category, name;
               resolution=nothing, features=Dict()) -> NonSequentialTimeSeries
```

`owner_id` is an integer identifier (`Int64`) and `owner_category` (`Component` /
`SupplementalAttribute`) completes the owner identity — the owner is the pair
`(owner_id, owner_category)`. `features` is serialized to JSON and must contain only JSON-scalar
values (`Int`, `Float64`, `Bool`, `String`). Pass the type as the first argument to
`get_time_series` to read a non-sequential series back.

`get_time_series` supports two unified calling conventions for **every** type: pass the
`TimeSeriesKey` returned by `add_time_series!` (key-based), or pass `owner_id, owner_category, name`
plus optional `resolution` / `features` keywords (attribute-based, the same addressing used by
`get_metadata` / `has_time_series` / `remove_time_series!`). Both forms return the same struct. The
bare `get_time_series(store, key)` remains a convenience alias for `SingleTimeSeries`.

## Bulk Adds

```julia
batch = AddBatch()
add_time_series!(batch, owner_id, owner_type, owner_category, ts; ...)  # any series type
add_time_series_bulk!(store::Store, batch::AddBatch) -> Vector{TimeSeriesKey}
```

`AddBatch` accepts the same `add_time_series!` methods as `Store` (every series and forecast type)
but only accumulates the requests; `add_time_series_bulk!` commits the whole batch in **one**
metadata transaction, which is much faster than per-item adds when ingesting many series. The submit
is all-or-nothing: on error nothing is committed. The batch is drained by the call in either case
and may be reused. `length(batch)` returns the number of pending requests.

### Attribute-based lookups

```julia
get_metadata(store, owner_id, owner_category::OwnerCategory, name;
             resolution=nothing, features=Dict()) -> NamedTuple
has_time_series(store, owner_id, owner_category::OwnerCategory, name;
                resolution=nothing, features=Dict()) -> Bool
remove_time_series!(store, owner_id, owner_category::OwnerCategory, name;
                    resolution=nothing, features=Dict()) -> Nothing
```

`owner_category` (`Component` / `SupplementalAttribute`) is required: the owner identity is the pair
`(owner_id, owner_category)`, so a component and a supplemental attribute may share a numeric
`owner_id` and remain distinct. `get_metadata` returns
`(initial_timestamp, resolution, length, data_hash, dtype)`, where `data_hash` is the 32-byte
content hash. It throws `NotFoundError` if absent.

```julia
get_array_by_hash(store, data_hash::Vector{UInt8}, ::Type{T}=Float64) -> Vector{T}
```

Fetches the flattened array for a 32-byte content hash, decoded as element type `T`. Combine with
`get_metadata` (for the dtype and shape) to read values without holding a `TimeSeriesKey`.

### Key-based variants

```julia
has_time_series(store, key::TimeSeriesKey) -> Bool
remove_time_series!(store, key::TimeSeriesKey) -> Nothing
```

### Enumerating keys

```julia
get_time_series_keys(store, owner_id, owner_category::OwnerCategory) -> Vector{TimeSeriesKey}
key_info(key::TimeSeriesKey) -> NamedTuple
# (owner_id, owner_category, name, time_series_type, resolution, features)
```

`get_time_series_keys` returns one key per stored association for the owner identified by the
`(owner_id, owner_category)` pair, including `DeterministicSingleTimeSeries` rows derived by
`transform_single_time_series!` — the way to read a transform-derived forecast by key (it returns no
key of its own). The keys are opaque; `key_info` inspects one. `time_series_type` is the **actual
Julia type** (`SingleTimeSeries`, `NonSequentialTimeSeries`, `Deterministic`,
`DeterministicSingleTimeSeries`, `Probabilistic`, or `Scenarios`), as in InfrastructureSystems.jl —
pass it straight to `get_time_series`. `features` is a `Dict` (empty when none) that round-trips the
JSON-scalar feature values. For example:

```julia
for k in get_time_series_keys(store, 12345, Component)
    info = key_info(k)
    series = get_time_series(info.time_series_type, store, k)
end
```

`key_info` also returns `owner_category` (the `Component` / `SupplementalAttribute` half of the
owner identity) alongside `owner_id`.

Reading a `DeterministicSingleTimeSeries` (by key or attributes) returns a `Deterministic`, since
the type has no materialized form.

## Forecasts

Dense forecasts are constructed as `Deterministic`, `Probabilistic`, or `Scenarios` structs (see
[Types](#types)) and added through the generic `add_time_series!`. Each struct wraps a native
`AbstractArray` of any element type and dimensionality — the binding derives the stored dtype and
dims and converts to row-major bytes, just like the static `add_time_series!` (see the
[data model](../explanation/data-model.md#forecasts) for the conventional shapes).

The forecast `name` comes from the struct, e.g.
`Deterministic(initial, resolution, horizon, interval, count, data, name)`.

```julia
add_time_series!(
    store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::Deterministic;
    features=Dict(), units=nothing, logical_type=nothing,
) -> TimeSeriesKey

add_time_series!(
    store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::Probabilistic;
    features=Dict(), units=nothing, logical_type=nothing,
) -> TimeSeriesKey

add_time_series!(
    store, owner_id, owner_type, owner_category::OwnerCategory,
    ts::Scenarios;
    features=Dict(), units=nothing, logical_type=nothing,
) -> TimeSeriesKey
```

A `DeterministicSingleTimeSeries` is not added directly. Derive one from every stored
`SingleTimeSeries` (sharing the backing array) with:

```julia
transform_single_time_series!(store, horizon::Period, interval::Period) -> Int   # number transformed
```

`has_typed` and `remove_typed!` operate on forecast types by `ts_type` integer code
(`2 = Deterministic`, `3 = DeterministicSingleTimeSeries`, `4 = Probabilistic`, `5 = Scenarios`):

```julia
has_typed(store, owner_id, owner_category, name, ts_type::Integer;
          resolution=nothing, features=Dict()) -> Bool
remove_typed!(store, owner_id, owner_category, name, ts_type::Integer;
              resolution=nothing, features=Dict())
```

### Reading forecast values

The type-dispatched `get_time_series(Type, …)` functions return the corresponding struct, whose
`data` field is a decoded N-dimensional Julia array (reshaped to the type's logical shape, with
native Julia indexing). Pass `time_range = (start::DateTime, end::DateTime)` (exclusive end) to
select a window sub-range.

Like the static readers, forecasts support both calling conventions: attribute-based
(`owner_id, owner_category, name` plus optional `resolution` / `features`) or key-based (the
`TimeSeriesKey` returned by `add_time_series!`).

```julia
get_time_series(Deterministic, store, owner_id, owner_category, name;
                resolution=nothing, features=Dict(), time_range=nothing) -> Deterministic
get_time_series(Deterministic, store, key::TimeSeriesKey; time_range=nothing) -> Deterministic
                # data shape: (H, count, element_dims...)

get_time_series(Probabilistic, store, owner_id, owner_category, name;
                resolution=nothing, features=Dict(), time_range=nothing) -> Probabilistic
get_time_series(Probabilistic, store, key::TimeSeriesKey; time_range=nothing) -> Probabilistic
                # data shape: (num_percentiles, H, count, element_dims...)

get_time_series(Scenarios, store, owner_id, owner_category, name;
                resolution=nothing, features=Dict(), time_range=nothing) -> Scenarios
get_time_series(Scenarios, store, key::TimeSeriesKey; time_range=nothing) -> Scenarios
                # data shape: (scenario_count, H, count, element_dims...)
```

The **attribute-based** `Deterministic` reader also resolves a transformed
`DeterministicSingleTimeSeries` (synthesized into a `Deterministic`) when no directly-stored
`Deterministic` matches. The **key-based** readers carry the exact stored type in the key, so there
is no fallback — a `DeterministicSingleTimeSeries` key reads back as a `Deterministic`. You can also
request the derived type explicitly — `get_time_series(DeterministicSingleTimeSeries, store, …)` (by
key or attributes) — which likewise returns a `Deterministic`.

Alternatively, use `get_metadata` to obtain the `data_hash`, then `get_array_by_hash` for the raw
flattened array.

## Store-Wide Operations

```julia
get_counts(store) -> NamedTuple   # (components_with_time_series, static_time_series, forecasts)
counts_by_type(store) -> Vector{NamedTuple}   # (time_series_type, count) per stored type
num_distinct_arrays(store) -> Int   # distinct content hashes; shared arrays count once
time_series_counts(store) -> NamedTuple   # distinct owners per category + distinct arrays per kind
list_owner_ids(store, owner_category; time_series_type=nothing, resolution=nothing) -> Vector{Int}
get_forecast_parameters(store; resolution=nothing, interval=nothing) -> NamedTuple  # (horizon, interval, count, resolution); fields `nothing` when none match
check_static_consistency(store) -> Union{Nothing,NamedTuple}  # shared (initial_timestamp, length) of SingleTimeSeries; throws if they disagree
get_resolutions(store; time_series_type=nothing) -> Vector{Millisecond}  # distinct resolutions, ascending
get_compression(store) -> NamedTuple  # (compression=:deflate|:none, level, shuffle); restored from file on open
verify_integrity(store) -> Int    # number of integrity errors; 0 == intact
compact!(store) -> Nothing
flush!(store) -> Nothing          # sync to disk; afterwards .nc and .sqlite can be copied
clear!(store) -> Nothing          # remove all series
clear!(store, owner_id, owner_category::OwnerCategory) -> Nothing
                                  # remove one owner's series ((owner_id, owner_category) pair)
replace_owner!(store, old_owner_id, new_owner_id, owner_category::OwnerCategory) -> Int
                                  # reassign one owner's series to a new id (same category); count moved
close!(store) -> Nothing
```

`list_keys(store; owner_id=nothing, owner_category=nothing)` lists the key of every stored series as
`NamedTuple`s (identity plus the per-type descriptive snapshot: `initial_timestamp`, `resolution`,
`length`, `horizon`, `interval`, `count`, `features`). It accepts `owner_id` and `owner_category` as
independent filters to scope the listing. Physical storage detail (`data_hash`, `logical_type`,
`percentiles`) is not on a key — read it via `get_metadata` / `get_forecast_metadata`.

## Errors

All subtype `TimeSeriesException`:

| Type                       | Mapped from FFI code                                                       |
| -------------------------- | -------------------------------------------------------------------------- |
| `NotFoundError`            | `TS_ERR_NOT_FOUND`                                                         |
| `DuplicateTimeSeriesError` | `TS_ERR_DUPLICATE`                                                         |
| `InvalidParameterError`    | `TS_ERR_INVALID_PARAMETER` / `TS_ERR_INVALID_UTF8` / `TS_ERR_NULL_POINTER` |
| `IntegrityError`           | `TS_ERR_INTEGRITY`                                                         |
| `ReadOnlyStoreError`       | `TS_ERR_READ_ONLY`                                                         |
| `GenericError`             | Any other non-zero code (carries the numeric `code`)                       |

The message text comes from the FFI layer's thread-local error buffer.

## Time and Resolution Conversions

- `DateTime` is converted to/from Unix milliseconds at the boundary.
- `resolution` is passed as a `Period` and converted to milliseconds; reads return resolution as
  `Millisecond`.

## Tracing

```julia
init_logging(level::AbstractString = "") -> Nothing
```

Initialize the Rust tracing subscriber. `level` is an
[`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
directive string such as `"debug"` or `"time_series_store_core=debug"`. Pass an empty string (the
default) to read `RUST_LOG`; if that variable is also unset, no output is produced.

The subscriber is initialized at most once per process — subsequent calls are no-ops. The module's
`__init__` hook calls `init_logging("")` automatically when `RUST_LOG` is set, so the common case
requires no code change:

```sh
export RUST_LOG=time_series_store_core=debug
julia --project=. myscript.jl
```

For programmatic control without environment variables:

```julia
using TimeSeriesStore
init_logging("time_series_store_core=debug")
```

See [Julia developer guide](../guides/julia.md#diagnostics-and-tracing) for usage examples and a
table of available span targets.
