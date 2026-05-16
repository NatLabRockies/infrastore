# AGENTS.md

## Project Overview

**time-series-store** is a Rust library for managing time-series data in power-systems / energy
simulations. Persistence is split between numerical arrays in NetCDF4 and metadata associations in
SQLite. It exposes multiple bindings over a shared core:

- **Native Rust** — `time-series-store-core` public API
- **gRPC server + Rust client** — `time-series-store-server` (read-only server; writes need local
  filesystem access)
- **Python** — `time-series-store-py` via PyO3 (abi3-py310 wheel)
- **Julia** — `time-series-store-ffi` C ABI cdylib, wrapped by `julia/TimeSeriesStore.jl`

**Current feature coverage:** `SingleTimeSeries` and `NonSequentialTimeSeries` are implemented
end-to-end across Rust, gRPC, Python, and Julia. `Deterministic`, `DeterministicSingleTimeSeries`,
`Probabilistic`, and `Scenarios` are implemented in the Rust core and C ABI, but are not yet fully
wrapped by Python, Julia, or gRPC. Arrays are dtype-generic and may have multidimensional
per-timestep values. Auth is `none` (default) or `api_key` via the `x-api-key` header. See
`README.md` and `docs/src/explanation/data-model.md` for the authoritative feature matrix.

## Code Quality Requirements

**All code changes must pass the following checks before being committed:**

```bash
cargo fmt --all -- --check                              # Rust formatting
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features                   # Tests
dprint check                                             # Markdown formatting
cargo deny check --config deny.toml                     # Dependency policy
```

**Key requirements:**

- **Rust code**: Must compile without clippy warnings. Use
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` to verify.
- **Toolchain**: The workspace uses Rust edition 2024 and declares an MSRV of Rust 1.95.0. Do not
  use APIs requiring a newer compiler without intentionally updating `rust-version` and CI.
- **Pre-commit**: `cargo-husky` installs `.cargo-husky/hooks/pre-commit`, which runs rustfmt,
  Clippy, dprint, and shellcheck when available. Do not bypass a failing hook. Tests and
  `cargo-deny` are still required before committing.
- **CI**: Workspace builds and tests run on Linux, macOS, and Windows. Avoid Unix-only assumptions
  in shared Rust code, build scripts, paths, and workflow changes.
- **Dependency policy**: `deny.toml` rejects wildcard dependencies and unknown registries or Git
  sources. Internal path dependencies must include a version. New licenses must be reviewed before
  adding them to the allowlist.

For detailed style guidelines, see `docs/style-guide.md`.

## Repository Structure

```
crates/
  time-series-store-core/    # Types, NetCDF + SQLite storage, hashing, public Rust API
    src/types/               #   key.rs, metadata.rs, time_series.rs
    src/storage/             #   memory.rs, netcdf.rs (storage backends)
    src/metadata/            #   schema.rs (SQLite catalog schema)
    src/store.rs             #   Store: the top-level public API
    src/hash.rs              #   SHA-256 column hashing
  time-series-store-proto/   # Protobuf service definition + tonic codegen, conversions
  time-series-store-server/  # gRPC server binary (src/bin/server.rs) + Rust client
  time-series-store-py/      # PyO3 bindings
  time-series-store-ffi/     # C ABI cdylib (used by the Julia binding)
proto/                       # .proto sources
julia/TimeSeriesStore.jl/    # Julia package wrapping the C ABI
python/tests/                # pytest suite
examples/                    # Sample server config + basic_rust.rs example
.github/workflows/           # Cross-platform tests, linting, security, wheel builds
```

## Build & Test

```bash
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

### Prerequisites

System libraries are required for the NetCDF and protobuf builds. On macOS:

```bash
brew install hdf5 netcdf protobuf maturin
```

If `cargo build` fails with `Unable to locate HDF5 root directory and/or headers`, point the build
script at Homebrew explicitly:

```bash
export HDF5_DIR="$(brew --prefix hdf5)"
```

On Linux (Debian/Ubuntu): `sudo apt-get install libhdf5-dev libnetcdf-dev protobuf-compiler` (set
`HDF5_DIR=/usr/lib/x86_64-linux-gnu/hdf5/serial` if the build script can't find HDF5).

On Windows, CI installs `netcdf-c:x64-windows` with vcpkg and sets `NETCDF_DIR`, `HDF5_DIR`, and
`PKG_CONFIG_PATH`. Keep these requirements in mind when changing native dependencies.

The workspace cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build --workspace` can link the PyO3 cdylib without `maturin`. On Linux and Windows those
flags are inert.

## Bindings

### Python (PyO3)

```bash
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy
maturin develop --manifest-path crates/time-series-store-py/Cargo.toml
pytest python/tests
```

### Julia (C ABI)

```bash
cargo build -p time-series-store-ffi --release
export TIME_SERIES_STORE_LIB=$PWD/target/release/libtime_series_store_ffi.dylib  # .so on Linux
julia --project=julia/TimeSeriesStore.jl -e 'using Pkg; Pkg.instantiate()'
julia --project=julia/TimeSeriesStore.jl julia/TimeSeriesStore.jl/test/runtests.jl
```

The FFI build script generates `crates/time-series-store-ffi/include/time_series_store.h` via
`cbindgen`. Never hand-edit the header. Any change to an exported `extern "C"` function must:

- include an accurate Rustdoc `# Safety` section covering pointer validity, ownership, lengths,
  concurrency, and the matching deallocator;
- regenerate and commit the header;
- update the Julia wrapper and tests when the ABI behavior changes.

## Server

```bash
cp examples/server.toml my_server.toml
# edit my_server.toml: point [data].files at your .nc, set [authentication]
cargo run -p time-series-store-server -- --config my_server.toml
```

`auth = "api_key"` requires at least one entry in `keys`; clients must send the chosen key in the
`x-api-key` header.

## Storage Format

- A persisted store is a NetCDF file plus a SQLite catalog at `<netcdf-path>.sqlite`. They are one
  logical artifact and must be moved, copied, and deleted together.
- `DATA_FORMAT_VERSION` in `crates/time-series-store-core/src/version.rs` is the on-disk
  compatibility contract. Any incompatible NetCDF layout, SQLite schema, dtype encoding, or hashing
  change must bump it and update format documentation and compatibility tests.
- Packed arrays use datasets named `sts_{dtype}_{shape}_{length}_{resolution}` with a companion
  `<dataset>_h` hash variable. Standalone arrays use `arr_{hex_hash}`. See
  `crates/time-series-store-core/src/storage/netcdf.rs` for the implementation and
  `docs/src/reference/file-format.md` for the user-facing specification; keep them synchronized.
- Deletion creates reusable packed slots or unreachable standalone variables. Physical shrinking is
  not available in NetCDF, and compaction behavior must remain explicit.

## Conventions

- Keep the multi-language surface consistent: a change to the core public API usually needs matching
  updates across the proto definitions, the gRPC server/client, the PyO3 bindings, and the FFI/Julia
  binding. When adding a feature, check all bindings before considering it done.
- Treat `time-series-store-core` as the source of truth. Binding crates depend on core; core must
  not depend on bindings.
- Use `TimeSeriesError` and the shared `Result` alias for core errors. Unsupported operations must
  return an explicit error rather than silently changing semantics.
- Preserve typed-array dtype, shape, byte order, timestamps, features, and hashes across every
  binding and persistence round trip.
- Do not manually edit generated artifacts. Besides the C header, protobuf output is generated by
  the proto crate's build script.
- Keep changes scoped. Do not commit local virtual environments, Python caches, generated NetCDF
  test data, or machine-specific library paths.
