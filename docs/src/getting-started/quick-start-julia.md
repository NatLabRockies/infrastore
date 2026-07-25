# Quick Start (Julia)

This walkthrough creates an in-memory store, adds a `SingleTimeSeries`, and reads it back — the
shortest path to a working round-trip. It assumes `InfraStore.jl` can find the native library; if
the first store call errors, see [Integrate with Julia](../how-to/integrate-julia.md).

## A Minimal Round-Trip

```julia
using Dates, InfraStore

# `in_memory=true` means no filesystem I/O. Pass `path=` with `in_memory=false`
# to write a NetCDF file plus its SQLite catalog.
store = Store(in_memory=true)

# The name lives on the series struct, not on `add_time_series!`.
ts = SingleTimeSeries(
    DateTime(2024, 1, 1),    # initial timestamp
    Hour(1),                 # resolution
    collect(100.0:123.0),    # 24 hourly values
    "load",                  # name
)

# The owner is identified by an integer id, an owner type, and a category.
# Features and units are optional.
key = add_time_series!(
    store,
    42,             # owner_id
    "Generator",    # owner_type
    Component,      # owner_category
    ts;
    features = Dict("model_year" => 2030),
    units = "MW",
)

got = get_time_series(store, key)
println("read $(length(got)) values @ $(got.resolution) from $(got.initial_timestamp)")
# read 24 values @ 3600000 milliseconds from 2024-01-01T00:00:00
@assert got.data == ts.data
```

## What Just Happened

1. **`Store(in_memory=true)`** built a store backed by an in-memory array backend and an in-memory
   SQLite metadata database.
2. **`add_time_series!`** hashed the array, wrote it to the backend (deduplicating on the hash), and
   recorded a metadata association keyed by
   `(owner_id, owner_category, type, name, resolution, interval, features)`. It returned a
   [`TimeSeriesKey`](../reference/julia-api.md#types) holding an opaque handle into the store.
3. **`get_time_series(store, key)`** looked up the association, read the array back by its content
   hash, and reconstructed a `SingleTimeSeries`. Note that `resolution` comes back as a
   `Millisecond`.

`features` is serialized to JSON, so its values must be JSON scalars (`Int`, `Float64`, `Bool`,
`String`). The data is any `AbstractArray` of `Float64`, `Float32`, `Int64`, `Int32`, `UInt64`, or
`Bool`; dimensions beyond the first attach a per-step element shape, such as the coefficient tuple
of a cost curve.

## Look It Up Without the Key

A series can also be addressed by its attributes, which is convenient when a caller keeps its own
identifiers:

```julia
got = get_time_series(SingleTimeSeries, store, 42, Component, "load"; resolution = Hour(1))

for m in list_time_series(store; owner_id = 42)
    println(m["name"], " ", m["resolution"], " ", m["units"])
end
# load PT1H MW
```

`get_time_series`, `has_time_series`, and `remove_time_series!` all accept either a `TimeSeriesKey`
or `(owner_id, owner_category, name; resolution, features)` attributes.

## Writing to Disk

Swap the constructor to persist. The do-block form closes the store on exit, including on throw:

```julia
Store(in_memory=false, path="system.nc") do store
    add_time_series!(store, 42, "Generator", Component, ts; units = "MW")
    flush!(store)   # sync buffered NetCDF writes to disk
end
```

This produces two files that travel together:

- `system.nc` — the NetCDF4 file holding the arrays.
- `system.nc.sqlite` — the catalog holding the metadata associations.

Reopen them later with `open_store("system.nc"; read_only=true)`, which has a do-block form too:

```julia
open_store("system.nc"; read_only=true) do store
    keys = get_time_series_keys(store, 42, Component)
    series = get_time_series(SingleTimeSeries, store, keys[1])
end
```

## Next Steps

- Work through the [Julia Developer Guide](../guides/julia.md) for forecasts, readers, associations,
  and error handling.
- Understand the [Data Model](../explanation/data-model.md): owners, keys, and features.
- Browse the full [Julia API reference](../reference/julia-api.md).
