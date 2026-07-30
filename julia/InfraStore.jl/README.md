# InfraStore.jl

Julia bindings for [infrastore](https://github.com/NatLabRockies/infrastore) — time-series storage
for power-systems and energy simulations.

Numerical arrays are persisted in HDF5; the metadata associating each array with its owning
component lives in SQLite. Identical arrays are stored once and shared through content addressing.

The package wraps the `libinfrastore_ffi` C ABI, distributed as `InfraStore_jll`. The binary
statically links its own pinned HDF5 (no `HDF5_jll` dependency, no MPI); its symbols are not
exported, so it coexists with HDF5.jl's `libhdf5` in the same process. Opening a live store's `.h5`
file directly with HDF5.jl or NCDatasets.jl is not supported — access the store through this
package.

## Install

```julia
using Pkg
Pkg.add("InfraStore")
```

## Quick start

```julia
using Dates, InfraStore

store = Store(in_memory=true)
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0), "load")
key = add_time_series!(store, 42, "Generator", Component, ts;
                       features=Dict("model_year" => 2030), units="MW")
got = get_time_series(store, key)
@assert got.data == ts.data
```

`Store` and `open_store` also take do-block forms, which close the store on exit:

```julia
Store("demo.h5") do store
    add_time_series!(store, 42, "Generator", Component, ts)
end
```

## Features

- **One array, stored once** — arrays are addressed by a SHA-256 content hash.
- **Typed, N-dimensional values** — `Float64`, `Float32`, `Int64`, `Int32`, `UInt64`, and `Bool`,
  with an optional per-timestep element shape.
- **Six time-series types** — `SingleTimeSeries` and `NonSequentialTimeSeries` read+write;
  `Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, and `Scenarios` for forecasts.
- **Columnar simulation readers** — `StaticReader` / `ForecastReader` serve every series' value at
  one timestamp.
- **Association catalogs** — component ↔ supplemental attribute and directed component ↔ component
  edges, recorded independently of time series.
- Overloads `Base`: `==` / `hash` on keys via the core identity, `show`, and `length` / `iterate` /
  `getindex` on values.

## Development builds

To run against a local `cargo build` instead of the JLL, point `INFRASTORE_LIB` at the cdylib:

```sh
cargo build -p infrastore-ffi --release
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib  # .so on Linux
```

`INFRASTORE_LIB` takes precedence over `InfraStore_jll`, so no code change is needed to switch
between them.

## Documentation

<https://natlabrockies.github.io/infrastore/latest/> — see the
[Julia guide](https://natlabrockies.github.io/infrastore/latest/guides/julia.html) and the
[Julia API reference](https://natlabrockies.github.io/infrastore/latest/reference/julia-api.html).

## License

BSD-3-Clause. See [LICENSE](LICENSE).
