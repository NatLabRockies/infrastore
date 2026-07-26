# Installation

Most users install a published package and need no build tools at all:

| Language | Install                        |
| -------- | ------------------------------ |
| Rust     | `cargo add infrastore-core`    |
| Python   | `pip install infrastore`       |
| Julia    | `Pkg.add("InfraStore")`        |
| CLI      | `cargo install infrastore-cli` |

The Python wheels and the Julia binary (`InfraStore_jll`) are prebuilt and self-contained. The Rust
crates build NetCDF, HDF5, and zlib from vendored sources and link them statically, so they need
`cmake` and a C compiler but **no system NetCDF or HDF5**. The same vendored, statically linked
stack backs every channel, so the HDF5 version behind the on-disk format is pinned by infrastore
rather than by the target environment.

The rest of this page covers building the workspace from a checkout.

## Build Prerequisites

A Rust toolchain, `cmake`, a C compiler, and `protobuf` for the gRPC codegen. The Python and Julia
bindings additionally need a Python interpreter (3.11+) or Julia (1.10+).

```sh
brew install cmake protobuf maturin              # macOS
sudo apt-get install cmake protobuf-compiler     # Linux (Debian / Ubuntu)
```

The first build compiles netcdf-c and HDF5 from source — a few minutes — and then caches the result.

> **Do not set `HDF5_DIR` or `NETCDF_DIR`.** The vendored netcdf-c build forwards them to cmake as
> `HDF5_ROOT` while still requesting static libraries, which fails against a shared-only install. To
> build against system libraries instead, turn vendoring off with `--no-default-features` and
> install them (`brew install hdf5 netcdf` / `apt-get install libhdf5-dev libnetcdf-dev`).
>
> Because `netcdf-sys` declares `links = "netcdf"`, Cargo's feature unification makes the
> vendored-versus-system choice all-or-nothing across the whole dependency graph — an individual
> crate cannot opt out on its own.

## Rust Toolchain

The workspace targets **edition 2024** and declares an MSRV of **Rust 1.94** — that is the oldest
toolchain it is guaranteed to build on. (`rust-version` in the root `Cargo.toml` is the authority;
the repo pins no `rust-toolchain` file, and CI builds on stable.)

```sh
rustup update stable
```

## Build the Workspace

```sh
git clone https://github.com/NatLabRockies/infrastore
cd infrastore
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

The workspace Cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build
--workspace` can link the PyO3 cdylib without `maturin`. On Linux those flags are inert.

## Crates in the Workspace

| Crate               | What it builds                                                       |
| ------------------- | -------------------------------------------------------------------- |
| `infrastore-core`   | Types, NetCDF + SQLite storage, hashing, Rust API                    |
| `infrastore-proto`  | Protobuf service definition + `tonic` codegen                        |
| `infrastore-server` | gRPC server binary + Rust client                                     |
| `infrastore-py`     | PyO3 bindings, `abi3-py311` wheel                                    |
| `infrastore-ffi`    | C ABI cdylib (the foundation of the Julia binding)                   |
| `infrastore-cli`    | `infrastore` CLI binary (CSV add/read, inspect on-disk stores)       |
| `infrastore-bench`  | `infrastore-bench` binary (bulk-ingest + simulation-read benchmarks) |

## Next Steps

- Build a store and round-trip a series in the [Python](./quick-start-python.md) or
  [Julia](./quick-start-julia.md) Quick Start.
- Set up a language binding: [Python](../how-to/integrate-python.md) ·
  [Julia](../how-to/integrate-julia.md).
- Stand up the [gRPC server](../how-to/run-server.md).
