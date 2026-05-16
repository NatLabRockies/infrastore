# Packaging & distribution

> **Status: local development.** Nothing is published to a registry yet (the repo isn't open
> source). `TimeSeriesStore.jl` is consumed via `Pkg.develop`, and the cdylib via the
> `TIME_SERIES_STORE_LIB` env var. The steps below are the publish plan for when the repo can be
> open-sourced.

This repo is the single source of truth for the time-series-store engine and the language bindings
that wrap it. Layout (monorepo):

```
crates/time-series-store-ffi/   # Rust cdylib (the C ABI)
crates/time-series-store-py/    # PyO3 crate  →  PyPI: time-series-store
julia/TimeSeriesStore.jl/       # Julia API   →  General: TimeSeriesStore.jl
yggdrasil/build_tarballs.jl     # BinaryBuilder recipe → General: TimeSeriesStore_jll
```

## Julia

Three registered pieces, all consumable from the General registry:

| Package                    | What it is                                                                                | Source                                                |
| -------------------------- | ----------------------------------------------------------------------------------------- | ----------------------------------------------------- |
| `TimeSeriesStore_jll`      | the compiled `libtime_series_store_ffi` binary, per platform                              | built by Yggdrasil from `yggdrasil/build_tarballs.jl` |
| `TimeSeriesStore.jl`       | the `ccall` wrapper + high-level Julia API (module `TimeSeriesStore`, store type `Store`) | `julia/TimeSeriesStore.jl/` (registered via `subdir`) |
| `InfrastructureSystems.jl` | depends on `TimeSeriesStore.jl` for its Rust time-series backend                          | separate repo                                         |

`TimeSeriesStore.jl` resolves the binary in this order: the `TIME_SERIES_STORE_LIB` env var (dev
builds), then `TimeSeriesStore_jll`. So it works today against a local `cargo build` and switches to
the JLL automatically once published — no code change.

Steps to publish:

1. **JLL** — copy `yggdrasil/build_tarballs.jl` into a Yggdrasil fork under `T/TimeSeriesStore/`,
   pin `version` + the source commit, get it building for the target platforms, open the PR. The
   recipe links `NetCDF_jll` + `HDF5_jll` so the binary shares the ecosystem's HDF5 (one `libhdf5`
   per process).
2. **`TimeSeriesStore.jl`** — once the JLL is merged, add `TimeSeriesStore_jll` to its `[deps]` and
   switch `_jll_library_path()` to a direct `import TimeSeriesStore_jll`. Register via
   Registrator/JuliaRegistries with the `subdir = julia/TimeSeriesStore.jl` option.
3. **`InfrastructureSystems.jl`** — add `TimeSeriesStore` to `[deps]`; replace the raw `ccall`s in
   `src/rust_time_series_store.jl` with calls into the package.

## Python

| Package                                          | What it is                                | Source                         |
| ------------------------------------------------ | ----------------------------------------- | ------------------------------ |
| `time-series-store` (import `time_series_store`) | PyO3/maturin wheel exposing the store API | `crates/time-series-store-py/` |

Steps to publish:

1. Set the distribution name `time-series-store` and module `time_series_store` in
   `crates/time-series-store-py/pyproject.toml` (rename from the current `time_series`).
2. Build wheels with `maturin` under `cibuildwheel` (abi3, manylinux + macOS + windows), publish to
   PyPI on tagged releases.

## Versioning

The Rust crates, the JLL, `TimeSeriesStore.jl`, and the Python package share the workspace version;
bump together and tag the repo per release so Yggdrasil and the registries pin the same commit.
