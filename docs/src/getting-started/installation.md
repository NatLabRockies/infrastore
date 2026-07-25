# Installation

castore is a Rust workspace. Building any of its interfaces requires a Rust toolchain plus the HDF5,
NetCDF, and Protobuf system libraries. The Python and Julia bindings additionally need a Python
interpreter (3.10+) or Julia (1.10+).

## System Libraries

### macOS (Homebrew)

```sh
brew install hdf5 netcdf protobuf maturin
```

`hdf5` is a transitive dependency of `netcdf`, but the `hdf5-metno-sys` build script does not always
locate it on its own. If `cargo build` fails with
`Unable to locate HDF5 root directory and/or headers`, point it at the Homebrew install explicitly:

```sh
export HDF5_DIR="$(brew --prefix hdf5)"
```

Add that line to your shell profile to make it permanent.

### Linux (Debian / Ubuntu)

```sh
sudo apt-get install libhdf5-dev libnetcdf-dev protobuf-compiler
# If the build script can't find HDF5:
export HDF5_DIR=/usr/lib/x86_64-linux-gnu/hdf5/serial
```

## Rust Toolchain

The workspace targets **edition 2024** and declares an MSRV of **Rust 1.94** — that is the oldest
toolchain it is guaranteed to build on. (`rust-version` in the root `Cargo.toml` is the authority;
the repo pins no `rust-toolchain` file, and CI builds on stable.)

```sh
rustup update stable
```

## Build the Workspace

```sh
git clone https://github.com/NatLabRockies/time-series-store
cd castore
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

The workspace Cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build
--workspace` can link the PyO3 cdylib without `maturin`. On Linux those flags are inert.

## Crates in the Workspace

| Crate            | What it builds                                                |
| ---------------- | ------------------------------------------------------------- |
| `castore-core`   | Types, NetCDF + SQLite storage, hashing, Rust API             |
| `castore-proto`  | Protobuf service definition + `tonic` codegen                 |
| `castore-server` | gRPC server binary + Rust client                              |
| `castore-py`     | PyO3 bindings, `abi3-py310` wheel                             |
| `castore-ffi`    | C ABI cdylib (the foundation of the Julia binding)            |
| `castore-cli`    | `cas` CLI binary (CSV add/read, inspect on-disk stores)       |
| `castore-bench`  | `cas-bench` binary (bulk-ingest + simulation-read benchmarks) |

## Next Steps

- Build a store and round-trip a series in the [Python](./quick-start-python.md) or
  [Julia](./quick-start-julia.md) Quick Start.
- Set up a language binding: [Python](../how-to/integrate-python.md) ·
  [Julia](../how-to/integrate-julia.md).
- Stand up the [gRPC server](../how-to/run-server.md).
