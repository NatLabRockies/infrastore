# Integrate with Julia

Get `InfraStore.jl` and its native library into a Julia environment. For API usage once it loads,
see the [Julia Developer Guide](../guides/julia.md).

## Prerequisites

- Julia 1.10 or newer.
- For a from-source build only: the [build tools](./install.md#1-install-build-tools) (`cmake`, a C
  compiler, `protobuf`). No system HDF5 is needed in either path.

## Install From the Registry

`InfraStore.jl` is registered in General, and the native library comes with it:

```julia
using Pkg
Pkg.add("InfraStore")
```

`Pkg` downloads the `libinfrastore_ffi` artifact for your platform from the matching GitHub Release
(`Artifacts.toml` in the package pins its URL and hash). The library is linked statically against
HDF5 and zlib, so there is no `HDF5_jll` and no system HDF5 involved, and the HDF5 version behind
the on-disk format is the one infrastore pinned. Artifacts exist for Linux x86_64 and aarch64
(glibc), macOS x86_64 and Apple Silicon, and Windows x86_64; on any other platform, build the
library yourself and use the override below.

That is the whole recipe for a consumer package. The rest of this page is for developing against a
checkout.

## Develop Against a Checkout

### 1. Build the Native Library

`InfraStore.jl` calls into the C ABI cdylib, so build it first:

```sh
cargo build -p infrastore-ffi --release
```

### 2. Point Julia at the Library

`InfraStore.jl` resolves the cdylib at first use, in this order:

1. The `INFRASTORE_LIB` environment variable — the development override, pointing at a build from
   step 1.
2. The `libinfrastore_ffi` artifact `Pkg` downloaded at install time.

For a development build, export the variable (add it to your shell profile to make it permanent):

```sh
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib  # .so on Linux
```

`using InfraStore` always works; the resolution happens on the first call that reaches the native
library, and the path is cached for the rest of the session — set the variable before that first
call. If neither source yields a file, that call errors (see [Troubleshooting](#troubleshooting)).
With the registered package you can skip the export entirely — set `INFRASTORE_LIB` only when you
want your local build to win over the artifact.

### 3. Instantiate and Test the Package

```sh
julia --project=julia/InfraStore.jl -e 'using Pkg; Pkg.instantiate()'
julia --project=julia/InfraStore.jl julia/InfraStore.jl/test/runtests.jl
```

### 4. Use It From Your Project

Develop the package into your own environment, then activate it with the library variable set:

```julia
using Pkg
Pkg.develop(path="/path/to/infrastore/julia/InfraStore.jl")
```

## Smoke Test

```julia
using Dates, InfraStore

store = Store(in_memory=true)
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0), "load")
id = add_time_series!(
    store,
    42,
    "Generator",
    Component,
    ts;
    features=Dict("model_year" => 2030),
    units="MW",
)
got = read_by_id(store, id)
@assert got.data == ts.data
@assert got.name == "load"
println("ok")
```

## Troubleshooting

- **`Could not locate libinfrastore_ffi at <path>. Set the INFRASTORE_LIB environment variable to
  a built cdylib, or reinstall the package to fetch the artifact.`**
  — The resolved path is not a file. If `<path>` is under `.julia/artifacts`, the artifact download
  did not complete — `Pkg.instantiate()` (or `Pkg.add("InfraStore")` again) fetches it. If it is
  your own path, export `INFRASTORE_LIB` before the first store call, in the same shell that
  launched Julia.
- **`could not load library`** — Check the path exists and has the right extension for your OS
  (`.dylib` on macOS, `.so` on Linux, `.dll` on Windows), and that you built with `--release` if
  your variable points at `target/release`.
- **`InvalidParameterError` on add** — `owner_id` must be an integer (e.g. `42`, an `Int64`), and
  `features` values must be JSON scalars.

## Next

- [Julia Developer Guide](../guides/julia.md)
- [Julia API reference](../reference/julia-api.md)
- [C ABI reference](../reference/c-abi.md)
