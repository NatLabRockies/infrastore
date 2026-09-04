# Quick Start (Julia)

This walkthrough creates an in-memory store, adds a `SingleTimeSeries`, and reads it back — the
shortest path to a working round-trip. It assumes `InfraStore.jl` can find the native library; if
the first store call errors, see [Integrate with Julia](../guides/julia.md#install).

## A Minimal Round-Trip

```julia
using Dates, InfraStore

# `in_memory=true` means no filesystem I/O. Pass `path=` with `in_memory=false`
# to write an HDF5 file plus its SQLite catalog.
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
id = add_time_series!(
    store,
    42,             # owner_id
    "Generator",    # owner_type
    Component,      # owner_category
    ts;
    features = Dict("model_year" => 2030),
    units = "MW",
)

got = read_by_id(store, id)
println("read $(length(got)) values @ $(got.resolution) from $(got.initial_timestamp)")
# read 24 values @ 3600000 milliseconds from 2024-01-01T00:00:00
@assert got.data == ts.data
```

## What Just Happened

1. **`Store(in_memory=true)`** built a store backed by an in-memory array backend and an in-memory
   SQLite metadata database.
2. **`add_time_series!`** hashed the array, wrote it to the backend (deduplicating on the hash), and
   recorded a catalog association filed under
   `(owner_id, owner_category, type, name, resolution, interval, features)`. It returned that row's
   **id** — the handle to record in your own object model, and what every read and removal takes
   from here on.
3. **`read_by_id(store, id)`** looked up the row by primary key, read the array back by its content
   hash, and reconstructed a `SingleTimeSeries`. Note that `resolution` comes back as a
   `Millisecond`.

`features` is serialized to JSON, so its values must be JSON scalars (`Int`, `Float64`, `Bool`,
`String`). The data is any `AbstractArray` of `Float64`, `Float32`, `Int64`, `Int32`, `UInt64`, or
`Bool`; dimensions beyond the first attach a per-step element shape, such as the coefficient tuple
of a cost curve.

## Finding a Series You Did Not Just Write

The store splits _identify_ from _act_. `list_metadata` is the identify half — it answers which
series exist and hands back the `id` that addresses each — and every read and removal takes that id.
A caller that records ids in its own object model does the first half once and skips it from then
on:

```julia
row = only(list_metadata(store; owner_id = 42, name = "load", resolution = Hour(1)))
got = read_by_id(store, row.id)

for m in list_metadata(store; owner_id = 42)   # Vector{TimeSeriesMetadata}
    println(m.name, " ", m.resolution, " ", m.units)
end
# load 3600000 milliseconds MW
```

`list_metadata` matches `features` as a subset; pass `exact_features` when you mean the whole set.
There is deliberately no separate attribute-to-id resolver — a caller that wants exactly one row
poses the filter and checks that it got one, which is what `only` does above.

## Writing to Disk

Swap the constructor to persist. The do-block form closes the store on exit, including on throw:

```julia
Store(in_memory=false, path="system.h5") do store
    add_time_series!(store, 42, "Generator", Component, ts; units = "MW")
    flush!(store)   # sync buffered HDF5 writes to disk
end
```

This produces two files that travel together:

- `system.h5` — the HDF5 file holding the arrays.
- `system.h5.sqlite` — the catalog holding the metadata associations.

Reopen them later with `open_store("system.h5"; read_only=true)`, which has a do-block form too:

```julia
open_store("system.h5"; read_only=true) do store
    rows = list_metadata(store; owner_id = 42, owner_category = Component)
    series = read_by_id(store, rows[1].id)
end
```

## Next Steps

- Work through the [Julia Developer Guide](../guides/julia.md) for forecasts, readers, associations,
  and error handling.
- Understand the [Data Model](../explanation/data-model.md): owners, ids, and features.
- Browse the full [Julia API reference](../reference/julia-api.md).
