# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this
repository.

## Project Overview

**time-series-store** is a Rust library for managing time-series data in power-systems / energy
simulations. Persistence is split between numerical arrays in NetCDF4 and metadata associations in
SQLite. It exposes multiple bindings over a shared core:

- **Native Rust** — `time-series-store-core` public API
- **gRPC server + Rust client** — `time-series-store-server` (read-only server; writes need local
  filesystem access)
- **Python** — `time-series-store-py` via PyO3 (abi3-py310 wheel)
- **Julia** — `time-series-store-ffi` C ABI cdylib, wrapped by `julia/TimeSeries.jl`

**v0 scope:** `SingleTimeSeries` is the only time-series type implemented end-to-end. The other five
types (NonSequentialTimeSeries, Deterministic, DeterministicSingleTimeSeries, Probabilistic,
Scenarios) have reserved slots in the metadata schema and the `TimeSeriesType` enum so they can land
later without breaking changes. Only 1-D `data` is supported. Auth is `none` (default) or `api_key`
via the `x-api-key` header. See `README.md` for the full scope and resolved design questions.

## Code Quality Requirements

**All code changes must pass the following checks before being committed:**

```bash
cargo fmt --all -- --check                              # Rust formatting
cargo clippy --workspace --all-targets -- -D warnings   # Rust linting
cargo test --workspace                                  # Tests
```

**Key requirements:**

- **Rust code**: Must compile without clippy warnings. Use
  `cargo clippy --workspace --all-targets -- -D warnings` to verify.
- **Edition**: The workspace is on Rust edition 2024 (requires Rust 1.95+). Match the surrounding
  code's idioms.
- **Before committing**: Always run the checks manually. Keep the workspace clippy-clean.

For detailed style guidelines, see `docs/style-guide.md`.

## Repository Structure

```
crates/
  time-series-store-core/    # Types, NetCDF + SQLite storage, hashing, public Rust API
    src/types/               #   key.rs, metadata.rs, time_series.rs
    src/storage/             #   memory.rs, netcdf.rs (storage backends)
    src/metadata/            #   schema.rs (SQLite sidecar schema)
    src/store.rs             #   Store: the top-level public API
    src/hash.rs              #   SHA-256 column hashing
  time-series-store-proto/   # Protobuf service definition + tonic codegen, conversions
  time-series-store-server/  # gRPC server binary (src/bin/server.rs) + Rust client
  time-series-store-py/      # PyO3 bindings
  time-series-store-ffi/     # C ABI cdylib (used by the Julia binding)
proto/                       # .proto sources
julia/TimeSeries.jl/         # Julia package wrapping the C ABI
python/tests/                # pytest suite
examples/                    # Sample server config + basic_rust.rs example
```

## Build & Test

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
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

The workspace cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build
--workspace` can link the PyO3 cdylib without `maturin`. On Linux those flags are inert.

## Bindings

### Python (PyO3)

```bash
cd crates/time-series-store-py
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy
maturin develop
pytest ../../python/tests
```

### Julia (C ABI)

```bash
cargo build -p time-series-store-ffi --release
export TIME_SERIES_STORE_LIB=$PWD/target/release/libtime_series_store_ffi.dylib  # .so on Linux
julia --project=julia/TimeSeries.jl -e 'using Pkg; Pkg.instantiate()'
julia --project=julia/TimeSeries.jl julia/TimeSeries.jl/test/runtests.jl
```

The FFI crate generates `time_series_store.h` via `cbindgen`; keep the checked-in header in sync
with the `extern "C"` surface in `time-series-store-ffi/src/lib.rs`.

## Server

```bash
cp examples/server.toml my_server.toml
# edit my_server.toml: point [data].files at your .nc, set [authentication]
cargo run -p time-series-store-server -- --config my_server.toml
```

`auth = "api_key"` requires at least one entry in `keys`; clients must send the chosen key in the
`x-api-key` header.

## Storage Format

- **Arrays**: a NetCDF file with attribute `data_format_version = "0.1.0"` and group
  `time_series/single/`. Each compacted dataset is named `sts_{length}_{resolution_seconds}` with
  shape `(length, 1000)` and chunking `(1, 1000)` (per-timestep reads across all components are
  contiguous). A sibling string variable `<dataset>_h` holds the SHA-256 hex hash per column; an
  empty string marks a free slot.
- **Metadata**: a sidecar SQLite file at `<path>.sqlite`. The two artifacts ship together.
- **Compaction**: triggered only by an explicit `Store::compact()` call.

## Conventions

- Keep the multi-language surface consistent: a change to the core public API usually needs matching
  updates across the proto definitions, the gRPC server/client, the PyO3 bindings, and the FFI/Julia
  binding. When adding a feature, check all bindings before considering it done.
- Reserved-but-unimplemented time-series types should return `InvalidParameter` (or the equivalent)
  rather than silently mis-handling input, preserving the v0 forward-compatibility contract.
