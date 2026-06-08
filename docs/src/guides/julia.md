# Julia Developer Guide

This guide covers building on `TimeSeriesStore.jl`, the Julia package that wraps the
[C ABI](../reference/c-abi.md). For exact signatures see the
[Julia API reference](../reference/julia-api.md); to set up the package and the native library, see
[Integrate with Julia](../how-to/integrate-julia.md).

## Load the Package

`TimeSeriesStore.jl` resolves the native library from the `TIME_SERIES_STORE_LIB` environment
variable (or the `TimeSeriesStore_jll` package when installed). Build the cdylib and point at it
before `using TimeSeriesStore`:

```sh
cargo build -p time-series-store-ffi --release
export TIME_SERIES_STORE_LIB=$PWD/target/release/libtime_series_store_ffi.dylib  # .so on Linux
```

```julia
using Dates, TimeSeriesStore
```

Exported names include `Store`, `SingleTimeSeries`, `NonSequentialTimeSeries`, `OwnerCategory`
(`Component`, `SupplementalAttribute`), the `add_time_series!` / `get_time_series` / `get_metadata`
family, and the forecast functions (`add_forecast!`, `add_probabilistic!`, …). The store type is
named **`Store`**.

## Open or Create a Store

```julia
# In-memory.
store = Store(in_memory=true)

# On disk: writes system.nc and system.nc.sqlite.
store = Store(in_memory=false, path="system.nc")

# Reopen read-only.
store = open_store("system.nc"; read_only=true)
```

The store is finalized automatically, but you can release it eagerly with `close!(store)`.

## Add a Series

```julia
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0))

key = add_time_series!(
    store,
    "42",
    "Generator",
    Component,
    "load",
    ts;
    features = Dict("model_year" => 2030),
    units = "MW",
)
```

Notes:

- **`owner_uuid` is a string** — typically the stringified IS.jl UUID. (Integer-looking owners must
  still be passed as strings, e.g. `"42"`.)
- **`resolution` is a `Period`** such as `Hour(1)` or `Minute(5)`.
- **`features`** is a `Dict` serialized to JSON, so values must be JSON scalars (`Int`, `Float64`,
  `Bool`, `String`). String features are supported and round-trip unchanged.
- Adding a duplicate [key](../explanation/data-model.md#keys) throws `DuplicateTimeSeriesError`.

`add_time_series!` returns a `TimeSeriesKey` holding an opaque handle into the store.

## Read a Series

```julia
got = get_time_series(store, key)
@assert got.data == ts.data
println(got.initial_timestamp, " ", got.resolution)   # resolution comes back as Millisecond
```

## Attribute-Based Lookups

Beyond key handles, `TimeSeriesStore.jl` can resolve a series directly from its attributes —
convenient when a caller keeps its own identifiers (as an IS.jl-side store does):

```julia
meta = get_metadata(
    store,
    "42",
    "load";
    resolution = Hour(1),
    features = Dict("model_year" => 2030),
)
# meta :: (initial_timestamp::DateTime, resolution::Millisecond, length::Int,
#          data_hash::Vector{UInt8}, dtype)

values = get_array_by_hash(store, meta.data_hash)     # Vector{Float64}; pass ::Type{T} for other dtypes

present = has_time_series(store, "42", "load"; resolution = Hour(1))
remove_time_series!(store, "42", "load"; resolution = Hour(1))
```

`has_time_series` and `remove_time_series!` also accept a `TimeSeriesKey` directly.

## Forecasts

`TimeSeriesStore.jl` wraps the forecast API. Pass forecast values flattened (column-major) and the
`TimeSeriesType` integer code (`2 = Deterministic`, `3 = DeterministicSingleTimeSeries`,
`5 = Scenarios`); `add_probabilistic!` carries the percentile vector for `Probabilistic`:

```julia
key = add_forecast!(
    store,
    "42",
    "Generator",
    Component,
    "load_fc",
    2,
    DateTime(2024, 1, 1), Hour(1), Hour(24), Hour(24),
    7,
    flat_values;
    units = "MW",
)

meta = get_forecast_metadata(store, "42", "load_fc", 2; resolution = Hour(1))
values = get_array_by_hash(store, meta.data_hash)
```

`has_typed` and `remove_typed!` operate on forecast types by `ts_type`. See the
[Julia API reference](../reference/julia-api.md#forecasts).

## Store-Wide Operations

```julia
counts = get_counts(store)        # (components_with_time_series, static_time_series, forecasts)
nerr   = verify_integrity(store)  # 0 == intact
compact!(store)
```

## Persist to Disk

```julia
flush!(store)   # sync NetCDF + SQLite; afterwards system.nc + system.nc.sqlite can be copied
```

Keep the `.nc` and `.nc.sqlite` files together.

## Error Handling

Errors subtype `TimeSeriesException`. Catch broadly or narrowly:

```julia
try
    add_time_series!(store, "42", "Generator", Component, "load", ts)
catch e
    if e isa DuplicateTimeSeriesError
        @warn "already present"
    else
        rethrow()
    end
end
```

The available types are `NotFoundError`, `DuplicateTimeSeriesError`, `InvalidParameterError`,
`IntegrityError`, `ReadOnlyStoreError`, and `GenericError` (which carries the raw FFI status
`code`).

## IS.jl Integration Notes

The model is designed to back an InfrastructureSystems.jl time-series store:

- Owners are string UUIDs, so IS.jl component/attribute UUIDs map straight through.
- `OwnerCategory` distinguishes `Component` from `SupplementalAttribute`.
- The attribute-based accessors (`get_metadata`, `has_time_series`, `remove_time_series!`) plus
  `get_array_by_hash` let an IS.jl-side store keep its own key objects and reach the array layer
  without holding a `TimeSeriesKey`.

See [Language Bindings](../explanation/bindings.md#isjl-integration) for how this maps onto the FFI.
