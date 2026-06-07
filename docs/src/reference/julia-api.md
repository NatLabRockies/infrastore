# Julia API

`TimeSeries.jl` wraps the [C ABI](./c-abi.md) cdylib. It loads the library from the path in the
`TIME_SERIES_STORE_LIB` environment variable at first use.

```julia
using TimeSeries
```

Exported names: `TimeSeriesStore`, `SingleTimeSeries`, `TimeSeriesKey`, `OwnerCategory`,
`Component`, `SupplementalAttribute`, `add_time_series!`, `get_time_series`, `remove_time_series!`,
`has_time_series`, `get_counts`, `verify_integrity`, `compact!`, `get_metadata`,
`get_array_by_hash`, `open_store`, `flush!`, `close!`.

## Constructors

```julia
TimeSeriesStore(; in_memory::Bool=true, path::Union{Nothing,AbstractString}=nothing) -> TimeSeriesStore
open_store(path::AbstractString; read_only::Bool=false) -> TimeSeriesStore
```

- `TimeSeriesStore()` — in-memory store.
- `TimeSeriesStore(in_memory=false, path="system.nc")` — persists to `system.nc` plus
  `system.nc.sqlite`.
- `open_store(path; read_only=true)` — opens an existing on-disk pair.

The store registers a finalizer; it is also closed explicitly with `close!(store)`.

## Types

```julia
struct SingleTimeSeries
    initial_timestamp :: DateTime
    resolution        :: Period          # e.g. Hour(1), Millisecond(500)
    data              :: Vector{Float64}
end

mutable struct TimeSeriesStore
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

`TimeSeriesKey` is opaque — it holds a pointer into the Rust store and is produced by
`add_time_series!`. It is finalized automatically when garbage-collected.

## Operations

```julia
add_time_series!(
    store::TimeSeriesStore,
    owner_uuid::AbstractString,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts::SingleTimeSeries;
    features::AbstractDict = Dict{String,Any}(),
    units::Union{Nothing,AbstractString} = nothing,
    scaling_factor_multiplier::Union{Nothing,AbstractString} = nothing,
) -> TimeSeriesKey

get_time_series(store::TimeSeriesStore, key::TimeSeriesKey) -> SingleTimeSeries
```

`owner_uuid` is a string — typically the stringified IS.jl UUID. `features` is serialized to JSON
and must contain only JSON-scalar values (`Int`, `Float64`, `Bool`, `String`).

### Attribute-based lookups

These resolve a series by its attributes rather than a key handle:

```julia
get_metadata(store, owner_uuid, name; resolution=nothing, features=Dict()) -> NamedTuple
has_time_series(store, owner_uuid, name; resolution=nothing, features=Dict()) -> Bool
remove_time_series!(store, owner_uuid, name; resolution=nothing, features=Dict()) -> Nothing
```

`get_metadata` returns
`(initial_timestamp::DateTime, resolution::Millisecond, length::Int,
data_hash::Vector{UInt8})`,
where `data_hash` is the 32-byte content hash. It throws `NotFoundError` if absent.

```julia
get_array_by_hash(store, data_hash::Vector{UInt8}) -> Vector{Float64}
```

Fetches the full array for a 32-byte content hash. Combine with `get_metadata` to read values
without holding a `TimeSeriesKey`.

### Key-based variants

```julia
has_time_series(store, key::TimeSeriesKey) -> Bool
remove_time_series!(store, key::TimeSeriesKey) -> Nothing
```

### Store-wide operations

```julia
get_counts(store) -> NamedTuple   # (components_with_time_series, static_time_series, forecasts)
verify_integrity(store) -> Int    # number of integrity errors; 0 == intact
compact!(store) -> Nothing
flush!(store) -> Nothing          # sync to disk; afterwards .nc and .sqlite can be copied
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
- `resolution` is passed as a `Period` and converted to nanoseconds; `get_time_series` and
  `get_metadata` return resolution as `Millisecond`.
