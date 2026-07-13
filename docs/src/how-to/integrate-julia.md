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

`TimeSeriesStore.jl` resolves the cdylib at first use, in this order:

1. The `TIME_SERIES_STORE_LIB` environment variable — the development override, pointing at a build
   from step 1.
2. The `TimeSeriesStore_jll` binary package, if it is installed in the active environment.

For a development build, export the variable (add it to your shell profile to make it permanent):

```sh
export TIME_SERIES_STORE_LIB=$PWD/target/release/libtime_series_store_ffi.dylib  # .so on Linux
```

`using TimeSeriesStore` always works; the resolution happens on the first call that reaches the
native library. If neither source yields a path, that call errors (see
[Troubleshooting](#troubleshooting)). With `TimeSeriesStore_jll` installed you can skip the export
entirely — set `TIME_SERIES_STORE_LIB` only when you want your local build to win over the JLL.

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
    42,
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

- **`Could not locate libtime_series_store_ffi. Set the TIME_SERIES_STORE_LIB environment variable to
  a built cdylib, or install TimeSeriesStore_jll.`**
  — Neither resolution path produced a library. Export the variable (step 2) before the first store
  call, in the same shell that launched Julia, or add `TimeSeriesStore_jll` to the environment.
- **`could not load library`** — Check the path exists and has the right extension for your OS
  (`.dylib` on macOS, `.so` on Linux, `.dll` on Windows), and that you built with `--release` if
  your variable points at `target/release`.
- **`InvalidParameterError` on add** — `owner_id` must be an integer (e.g. `42`, an `Int64`), and
  `features` values must be JSON scalars.

## Next

- [Julia Developer Guide](../guides/julia.md)
- [Julia API reference](../reference/julia-api.md)
- [C ABI reference](../reference/c-abi.md)
