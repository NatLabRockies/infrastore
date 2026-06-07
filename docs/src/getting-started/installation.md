# Installation

time-series-store is a Rust workspace. Building any of its interfaces requires a Rust toolchain plus
the HDF5, NetCDF, and Protobuf system libraries. The Python and Julia bindings additionally need a
Python interpreter (3.10+) or Julia (1.10+).

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

The workspace targets **edition 2024** and builds on Rust 1.95 or newer.

```sh
rustup update stable
```

## Build the Workspace

```sh
git clone https://github.com/NatLabRockies/time-series-store
cd time-series-store
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The workspace Cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build
--workspace` can link the PyO3 cdylib without `maturin`. On Linux those flags are inert.

## Crates in the Workspace

| Crate                      | What it builds                                     |
| -------------------------- | -------------------------------------------------- |
| `time-series-store-core`   | Types, NetCDF + SQLite storage, hashing, Rust API  |
| `time-series-store-proto`  | Protobuf service definition + `tonic` codegen      |
| `time-series-store-server` | gRPC server binary + Rust client                   |
| `time-series-store-py`     | PyO3 bindings, `abi3-py310` wheel                  |
| `time-series-store-ffi`    | C ABI cdylib (the foundation of the Julia binding) |

## Next Steps

- Build a store and round-trip a series in the [Quick Start](./quick-start.md).
- Set up a language binding: [Python](../how-to/integrate-python.md) ·
  [Julia](../how-to/integrate-julia.md).
- Stand up the [gRPC server](../how-to/run-server.md).
