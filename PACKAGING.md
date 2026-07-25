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

## NetCDF linkage

The two distribution channels deliberately link NetCDF differently, and the choice is not
interchangeable:

| Channel                   | Linkage                                  | How                                                                  |
| ------------------------- | ---------------------------------------- | -------------------------------------------------------------------- |
| Rust crates, Python wheel | vendored netcdf-c + HDF5 + zlib, static  | the `vendored` feature, enabled by default in every crate            |
| `Castore_jll` (Julia)     | dynamic, against `NetCDF_jll`/`HDF5_jll` | `cargo build --no-default-features` in `yggdrasil/build_tarballs.jl` |

Vendoring is what lets a downstream package get NetCDF for free — `pip install castore` and
`cargo add castore-core` need no system libraries, only `cmake` and a C compiler at build time. The
JLL is the exception: Julia's ecosystem already ships HDF5, and a statically vendored copy inside
`libcastore_ffi` would put **two libhdf5 instances in one process** alongside any other JLL that
links it. The recipe therefore passes `--no-default-features`; that flag is load-bearing.

The same hazard is the open question for Python wheels — see the Python section below.

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

### Pending: move wheels onto the vendored build

The wheel build still provisions system NetCDF/HDF5 and bundles them via `auditwheel`/`delvewheel`,
which predates the `vendored` feature. Switching over removes the provisioning entirely, but is
gated on one compatibility check.

1. **Verify HDF5 duplication is safe.** A statically vendored HDF5 inside the extension module,
   alongside the copy that `netCDF4`/`h5py` bring, means two libhdf5 instances in one interpreter —
   the same problem the JLL avoids by linking dynamically. Users of the primary downstream consumer
   (`infrasys`) very likely have `netCDF4` installed. The check, on manylinux:

   ```sh
   python -c "import castore, netCDF4, h5py; print('ok')"
   ```

   On Linux this can fail through symbol interposition rather than a clean error, so exercise an
   actual read/write from both libraries in the same process, not just the imports.
2. **If it passes**, delete the `before-all` provisioning from `[tool.cibuildwheel.*]` in
   `crates/castore-py/pyproject.toml` (brew on macOS, yum/dnf on Linux) and the conda/vcpkg
   HDF5/NetCDF step in `.github/workflows/python-wheels.yml`. Keep `rustup`; add `cmake`.
3. **Un-skip musllinux.** The current `skip = "*-musllinux* *-win32"` justifies the musl exclusion
   by the RPM-based `before-all` being unable to install HDF5 there — a reason that disappears once
   the sources are vendored. `win32` stays skipped (no 32-bit HDF5 from the current provider).
4. **If it fails**, keep Linux wheels on the system-library path with `--no-default-features` and
   apply vendoring only where it is clean, or hide the HDF5 symbols with a version script.

Unrelated to the wheels, `.github/workflows/test.yml` still installs system HDF5/NetCDF on all three
platforms. Those are now redundant (the default build vendors), and removing them trades a package
install for a few minutes of cold compile per cache miss.

## Versioning

The Rust crates, the JLL, `Castore.jl`, and the Python package share the workspace version; bump
together and tag the repo per release so Yggdrasil and the registries pin the same commit.
