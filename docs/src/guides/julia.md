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

Exported names include `Store`, `SingleTimeSeries`, `NonSequentialTimeSeries`, the forecast structs
(`Deterministic`, `Probabilistic`, `Scenarios`), `OwnerCategory` (`Component`,
`SupplementalAttribute`), the `add_time_series!` / `get_time_series` / `get_metadata` family, and
`transform_single_time_series!`. The store type is named **`Store`**.

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
# `name` ("load") is a required field on the struct.
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0), "load")

key = add_time_series!(
    store,
    "42",
    "Generator",
    Component,
    ts;                                   # name comes from ts
    features = Dict("model_year" => 2030),
    units = "MW",
)
```

Notes:

- **`owner_uuid` is a string** — typically the stringified InfrastructureSystems.jl UUID.
  (Integer-looking owners must still be passed as strings, e.g. `"42"`.)
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
convenient when a caller keeps its own identifiers (as an InfrastructureSystems.jl-side store does):

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

# get_time_series itself resolves by attributes too (pass the type as the first argument):
got = get_time_series(SingleTimeSeries, store, "42", "load"; resolution = Hour(1))

present = has_time_series(store, "42", "load"; resolution = Hour(1))
remove_time_series!(store, "42", "load"; resolution = Hour(1))
```

`get_time_series`, `has_time_series`, and `remove_time_series!` all accept either a `TimeSeriesKey`
or `(owner_uuid, name; resolution, features)` attributes — the conventions are interchangeable for
every time series type, static or forecast.

## Forecasts

`TimeSeriesStore.jl` exposes `Deterministic`, `Probabilistic`, and `Scenarios` structs that wrap a
native `AbstractArray` in the type's logical shape (the wrapper derives the dtype and dims and
serializes the buffer row-major). Construct one and add it through the generic `add_time_series!`:

```julia
data = zeros(Float64, 24, 7)   # (horizon_count, count)
fc = Deterministic(DateTime(2024, 1, 1), Hour(1), Hour(24), Hour(24), 7, data, "load_fc")
key = add_time_series!(
    store,
    "42",
    "Generator",
    Component,
    fc;                         # name comes from fc
    units = "MW",
)

got = get_time_series(Deterministic, store, "42", "load_fc"; resolution = Hour(1))
values = got.data   # Float64 matrix, shape (24, 7)

# Same forecast, read by the key returned from add_time_series! — forecasts and
# static series both support the key-based and attribute-based conventions.
got_by_key = get_time_series(Deterministic, store, key)
```

`Probabilistic(initial_timestamp, resolution, horizon, interval, count, percentiles, data, name)`
carries the percentile vector, and
`Scenarios(initial_timestamp, resolution, horizon, interval, count, data, name)` takes
`scenario_count` from `data`'s leading axis. Read the corresponding type back with
`get_time_series(Probabilistic, …)` / `get_time_series(Scenarios, …)`; requesting `Deterministic`
also returns a transformed `DeterministicSingleTimeSeries` (synthesized).

A `DeterministicSingleTimeSeries` is not added directly — derive one from every stored
`SingleTimeSeries` with `transform_single_time_series!(store, horizon::Period, interval::Period)`,
which returns the number transformed:

```julia
n = transform_single_time_series!(store, Hour(24), Hour(24))
```

`transform_single_time_series!` returns no keys, so to read a derived forecast by key, enumerate the
owner's keys with `get_time_series_keys(store, owner_uuid)` and use `key_info`, whose
`time_series_type` is the actual Julia type — pass it straight to `get_time_series` (a
`DeterministicSingleTimeSeries` reads back as a `Deterministic`):

```julia
for k in get_time_series_keys(store, "42")
    info = key_info(k)
    series = get_time_series(info.time_series_type, store, k)
end
```

`has_typed` and `remove_typed!` operate on forecast types by `ts_type`. The low-level
`get_metadata` + `get_array_by_hash` path is still available for raw access. See the
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

Errors subtype `TimeSeriesStore.TimeSeriesException`. The exception types are not exported, so
reference them module-qualified. Catch broadly or narrowly:

```julia
try
    add_time_series!(store, "42", "Generator", Component, ts)
catch e
    if e isa TimeSeriesStore.DuplicateTimeSeriesError
        @warn "already present"
    else
        rethrow()
    end
end
```

The available types are `TimeSeriesStore.NotFoundError`, `TimeSeriesStore.DuplicateTimeSeriesError`,
`TimeSeriesStore.InvalidParameterError`, `TimeSeriesStore.IntegrityError`,
`TimeSeriesStore.ReadOnlyStoreError`, and `TimeSeriesStore.GenericError` (which carries the raw FFI
status `code`).

## InfrastructureSystems.jl Integration Notes

The model is designed to back an InfrastructureSystems.jl time-series store:

- Owners are string UUIDs, so InfrastructureSystems.jl component/attribute UUIDs map straight
  through.
- `OwnerCategory` distinguishes `Component` from `SupplementalAttribute`.
- The attribute-based accessors (`get_metadata`, `has_time_series`, `remove_time_series!`) plus
  `get_array_by_hash` let an InfrastructureSystems.jl-side store keep its own key objects and reach
  the array layer without holding a `TimeSeriesKey`.

See [Language Bindings](../explanation/bindings.md#infrastructuresystemsjl-integration) for how this
maps onto the FFI.

## Diagnostics and tracing

The store emits structured tracing spans for every significant operation. To see them, initialize a
subscriber before your first store call.

**Via environment variable** — set `RUST_LOG` before loading the package. The module's `__init__`
hook calls `init_logging("")` automatically, which reads `RUST_LOG` if set:

```sh
# shell
export RUST_LOG=time_series_store_core=debug
julia --project=. myscript.jl
```

**Programmatically** — call `init_logging` with a filter directive string:

```julia
using TimeSeriesStore

init_logging("time_series_store_core=debug")

store = Store(in_memory=true)
add_time_series!(store, ...)   # spans appear on stderr
```

`init_logging` is a no-op if a subscriber is already registered (including the automatic one from
`RUST_LOG`). The filter syntax is the same as `RUST_LOG`: comma-separated `target=level` pairs, or a
bare level such as `"debug"` to match everything. Useful targets:

| Target                   | What it covers                                               |
| ------------------------ | ------------------------------------------------------------ |
| `time_series_store_core` | All store operations — `add`, `get`, `remove` and NetCDF I/O |
