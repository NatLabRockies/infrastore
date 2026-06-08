# Julia API

The Julia package is **`TimeSeriesStore.jl`** (module `TimeSeriesStore`); it wraps the
[C ABI](./c-abi.md) cdylib. The library is resolved from the `TIME_SERIES_STORE_LIB` environment
variable (development builds), or from the `TimeSeriesStore_jll` binary package when installed.

```julia
using TimeSeriesStore
```

Exported names: `Store`, `SingleTimeSeries`, `NonSequentialTimeSeries`, `TimeSeriesKey`,
`OwnerCategory`, `Component`, `SupplementalAttribute`, `add_time_series!`, `get_time_series`,
`remove_time_series!`, `has_time_series`, `get_counts`, `verify_integrity`, `compact!`, `clear!`,
`get_metadata`, `get_array_by_hash`, `open_store`, `flush!`, `close!`, `add_forecast!`,
`get_forecast_metadata`, `has_typed`, `remove_typed!`, `add_probabilistic!`,
`get_probabilistic_metadata`.

## Constructors

```julia
Store(; in_memory::Bool=true, path::Union{Nothing,AbstractString}=nothing) -> Store
open_store(path::AbstractString; read_only::Bool=false) -> Store
```

- `Store()` — in-memory store.
- `Store(in_memory=false, path="system.nc")` — persists to `system.nc` plus `system.nc.sqlite`.
- `open_store(path; read_only=true)` — opens an existing on-disk pair.

The store registers a finalizer; close it eagerly with `close!(store)`.

## Types

```julia
struct SingleTimeSeries
    initial_timestamp :: DateTime
    resolution        :: Period          # e.g. Hour(1), Millisecond(500)
    data              :: AbstractArray   # any element type; multi-dim allowed
    logical_type      :: Union{Nothing,String}
end

struct NonSequentialTimeSeries
    timestamps   :: Vector{DateTime}     # strictly increasing
    data         :: AbstractArray
    logical_type :: Union{Nothing,String}
end

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

`SingleTimeSeries` and `NonSequentialTimeSeries` take an optional trailing `logical_type` (default
`nothing`) — an opaque label the binding can use to reconstruct a domain object on read. `data`
keeps its Julia element type: the binding maps it to a stored dtype (`Float64`, `Float32`, `Int64`,
`Int32`, `UInt64`, `Bool`) and converts to row-major bytes on the way down.

## Static Series

```julia
add_time_series!(
    store::Store, owner_uuid, owner_type, owner_category::OwnerCategory, name,
    ts::SingleTimeSeries;
    features::AbstractDict = Dict(),
    units = nothing, scaling_factor_multiplier = nothing,
    logical_type = ts.logical_type,
) -> TimeSeriesKey

add_time_series!(
    store::Store, owner_uuid, owner_type, owner_category::OwnerCategory, name,
    ts::NonSequentialTimeSeries;
    features = Dict(), units = nothing, scaling_factor_multiplier = nothing,
    logical_type = ts.logical_type,
) -> TimeSeriesKey

get_time_series(store::Store, key::TimeSeriesKey) -> SingleTimeSeries
get_time_series(NonSequentialTimeSeries, store::Store, key::TimeSeriesKey) -> NonSequentialTimeSeries
```

`owner_uuid` is a string (typically the stringified IS.jl UUID). `features` is serialized to JSON
and must contain only JSON-scalar values (`Int`, `Float64`, `Bool`, `String`). Pass the type as the
first argument to `get_time_series` to read a non-sequential series back.

### Attribute-based lookups

```julia
get_metadata(store, owner_uuid, name; resolution=nothing, features=Dict()) -> NamedTuple
has_time_series(store, owner_uuid, name; resolution=nothing, features=Dict()) -> Bool
remove_time_series!(store, owner_uuid, name; resolution=nothing, features=Dict()) -> Nothing
```

`get_metadata` returns `(initial_timestamp, resolution, length, data_hash, dtype)`, where
`data_hash` is the 32-byte content hash. It throws `NotFoundError` if absent.

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

## Forecasts

The Julia binding now wraps the forecast C ABI. Forecast values are passed flattened (column-major,
see the [data model](../explanation/data-model.md#forecasts) for the conventional shapes). `ts_type`
is the `TimeSeriesType` integer code (`2 = Deterministic`, `3 = DeterministicSingleTimeSeries`,
`5 = Scenarios`).

```julia
add_forecast!(
    store, owner_uuid, owner_type, owner_category::OwnerCategory, name,
    ts_type::Integer, initial_timestamp::DateTime, resolution::Period,
    horizon::Period, interval::Period, count::Integer, flat_values::Vector{Float64};
    features=Dict(), units=nothing, scaling_factor_multiplier=nothing,
) -> TimeSeriesKey

add_probabilistic!(
    store, owner_uuid, owner_type, owner_category::OwnerCategory, name,
    initial_timestamp::DateTime, resolution::Period, horizon::Period,
    interval::Period, count::Integer,
    percentiles::Vector{Float64}, flat_values::Vector{Float64};
    features=Dict(), units=nothing, scaling_factor_multiplier=nothing,
) -> TimeSeriesKey

get_forecast_metadata(store, owner_uuid, name, ts_type::Integer; resolution=nothing, features=Dict())
    -> NamedTuple  # (initial_timestamp, resolution, horizon, interval, count, length, data_hash)

get_probabilistic_metadata(store, owner_uuid, name; resolution=nothing, features=Dict())
    -> NamedTuple  # (..., percentiles)

has_typed(store, owner_uuid, name, ts_type::Integer; resolution=nothing, features=Dict()) -> Bool
remove_typed!(store, owner_uuid, name, ts_type::Integer; resolution=nothing, features=Dict())
```

Read forecast values with `get_forecast_metadata`/`get_probabilistic_metadata` to obtain the
`data_hash`, then `get_array_by_hash`.

## Store-Wide Operations

```julia
get_counts(store) -> NamedTuple   # (components_with_time_series, static_time_series, forecasts)
verify_integrity(store) -> Int    # number of integrity errors; 0 == intact
compact!(store) -> Nothing
flush!(store) -> Nothing          # sync to disk; afterwards .nc and .sqlite can be copied
clear!(store) -> Nothing          # remove all series
close!(store) -> Nothing
```

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

- `DateTime` is converted to/from Unix nanoseconds at the boundary (millisecond precision).
- `resolution` is passed as a `Period` and converted to nanoseconds; reads return resolution as
  `Millisecond`.
