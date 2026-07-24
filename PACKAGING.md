# Packaging & distribution

> **Status: local development.** Nothing is published to a registry yet (the repo isn't open
> source). `Castore.jl` is consumed via `Pkg.develop`, and the cdylib via the `CASTORE_LIB` env var.
> The steps below are the publish plan for when the repo can be open-sourced.

This repo is the single source of truth for the castore engine and the language bindings that wrap
it. Layout (monorepo):

```
crates/castore-ffi/   # Rust cdylib (the C ABI)
crates/castore-py/    # PyO3 crate  →  PyPI: castore
julia/Castore.jl/       # Julia API   →  General: Castore.jl
yggdrasil/build_tarballs.jl     # BinaryBuilder recipe → General: Castore_jll
```

## Julia

Three registered pieces, all consumable from the General registry:

| Package                    | What it is                                                                        | Source                                                |
| -------------------------- | --------------------------------------------------------------------------------- | ----------------------------------------------------- |
| `Castore_jll`              | the compiled `libcastore_ffi` binary, per platform                                | built by Yggdrasil from `yggdrasil/build_tarballs.jl` |
| `Castore.jl`               | the `ccall` wrapper + high-level Julia API (module `Castore`, store type `Store`) | `julia/Castore.jl/` (registered via `subdir`)         |
| `InfrastructureSystems.jl` | depends on `Castore.jl` for its Rust time-series backend                          | separate repo                                         |

`Castore.jl` resolves the binary in this order: the `CASTORE_LIB` env var (dev builds), then
`Castore_jll`. So it works today against a local `cargo build` and switches to the JLL automatically
once published — no code change.

Steps to publish:

1. **JLL** — copy `yggdrasil/build_tarballs.jl` into a Yggdrasil fork under `C/Castore/`, pin
   `version` + the source commit, get it building for the target platforms, open the PR. The recipe
   links `NetCDF_jll` + `HDF5_jll` so the binary shares the ecosystem's HDF5 (one `libhdf5` per
   process).
2. **`Castore.jl`** — once the JLL is merged, add `Castore_jll` to its `[deps]` and switch
   `_jll_library_path()` to a direct `import Castore_jll`. Register via Registrator/JuliaRegistries
   with the `subdir = julia/Castore.jl` option.
3. **`InfrastructureSystems.jl`** — add `Castore` to `[deps]`; replace the raw `ccall`s in
   `src/rust_time_series_store.jl` with calls into the package.

## Python

| Package                      | What it is                                | Source               |
| ---------------------------- | ----------------------------------------- | -------------------- |
| `castore` (import `castore`) | PyO3/maturin wheel exposing the store API | `crates/castore-py/` |

The distribution name (`castore`) and module name (`castore`) are already set in
`crates/castore-py/pyproject.toml`.

Steps to publish:

1. Build wheels with `maturin` under `cibuildwheel` (abi3, manylinux + macOS + windows), publish to
   PyPI on tagged releases.

## Versioning

The Rust crates, the JLL, `Castore.jl`, and the Python package share the workspace version; bump
together and tag the repo per release so Yggdrasil and the registries pin the same commit.
