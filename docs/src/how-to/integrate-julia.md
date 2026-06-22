# Integrate with Julia

Wire `TimeSeriesStore.jl` to the native library. For API usage once it loads, see the
[Julia Developer Guide](../guides/julia.md).

## Prerequisites

- Julia 1.10 or newer.
- The [system libraries](./install.md#1-install-system-libraries) (HDF5, NetCDF, Protobuf).

## 1. Build the Native Library

`TimeSeriesStore.jl` calls into the C ABI cdylib, so build it first:

```sh
cargo build -p time-series-store-ffi --release
```

## 2. Point Julia at the Library

`TimeSeriesStore.jl` reads the library path from the `TIME_SERIES_STORE_LIB` environment variable at
first use:

```sh
export TIME_SERIES_STORE_LIB=$PWD/target/release/libtime_series_store_ffi.dylib  # .so on Linux
```

If the variable is unset, `using TimeSeriesStore` works but the first store operation errors with a
message telling you to set it. Add the export to your shell profile to make it permanent.

## 3. Instantiate and Test the Package

```sh
julia --project=julia/TimeSeriesStore.jl -e 'using Pkg; Pkg.instantiate()'
julia --project=julia/TimeSeriesStore.jl julia/TimeSeriesStore.jl/test/runtests.jl
```

## 4. Use It From Your Project

Develop the package into your own environment, then activate it with the library variable set:

```julia
using Pkg
Pkg.develop(path="/path/to/time-series-store/julia/TimeSeriesStore.jl")
```

## Smoke Test

```julia
using Dates, TimeSeriesStore

store = Store(in_memory=true)
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0), "load")
key = add_time_series!(
    store,
    "42",
    "Generator",
    Component,
    ts;
    features=Dict("model_year" => 2030),
    units="MW",
)
got = get_time_series(store, key)
@assert got.data == ts.data
@assert got.name == "load"
println("ok")
```

## Troubleshooting

- **`TIME_SERIES_STORE_LIB env var must point to ...`** — Export the variable (step 2) before the
  first store call, in the same shell that launched Julia.
- **`could not load library`** — Check the path exists and has the right extension for your OS
  (`.dylib` on macOS, `.so` on Linux, `.dll` on Windows), and that you built with `--release` if
  your variable points at `target/release`.
- **`InvalidParameterError` on add** — `owner_id` must be an integer (e.g. `42`, an `Int64`), and
  `features` values must be JSON scalars.

## Next

- [Julia Developer Guide](../guides/julia.md)
- [Julia API reference](../reference/julia-api.md)
- [C ABI reference](../reference/c-abi.md)
