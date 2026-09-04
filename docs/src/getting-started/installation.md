# Installation

Most users install a published package and need no build tools at all:

| Language | Install                                                                     |
| -------- | --------------------------------------------------------------------------- |
| Rust     | `cargo add infrastore-core`                                                 |
| Python   | `pip install infrastore`                                                    |
| CLI      | [download a binary](#the-infrastore-cli), or `cargo install infrastore-cli` |
| Julia    | `pkg> add InfraStore` — see [Julia](#julia)                                 |

The Python wheels and the Julia binary artifact are prebuilt and self-contained. The Rust crates
build HDF5 and zlib from vendored sources and link them statically, so they need `cmake` and a C
compiler but **no system HDF5**. The same vendored, statically linked stack backs every channel, so
the HDF5 version behind the on-disk format is pinned by infrastore rather than by the target
environment.

## The `infrastore` CLI

### Download a prebuilt binary

Each tagged release attaches archives to the
[Releases page](https://github.com/NatLabRockies/infrastore/releases). The executables are linked
statically against HDF5 and zlib, so there is nothing else to install.

| Archive                                       | Contents                                               |
| --------------------------------------------- | ------------------------------------------------------ |
| `infrastore-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64 — `infrastore`, `infrastore-server`       |
| `infrastore-x86_64-unknown-linux-gnu.tar.gz`  | Linux x86_64 — `libinfrastore_ffi.so` + `infrastore.h` |
| `infrastore-aarch64-apple-darwin.tar.gz`      | macOS Apple Silicon — executables and C library        |
| `infrastore-x86_64-pc-windows-msvc.zip`       | Windows x86_64 — executables and C library             |

Linux ships two archives because they serve different consumers. The executables are built against
musl and linked statically, so they run on any distribution regardless of its glibc version —
including an HPC login node much older than the build machine. The C library is built against glibc
instead: it gets loaded into a running Julia or Python process, and a musl shared library there
would put two C libraries in one address space.

```sh
VERSION=v0.11.0    # pick a release from the Releases page
BASE=https://github.com/NatLabRockies/infrastore/releases/download/$VERSION
curl -fsSLO $BASE/infrastore-aarch64-apple-darwin.tar.gz
tar xzf infrastore-aarch64-apple-darwin.tar.gz
./infrastore --version
```

Move `infrastore` onto your `PATH` to finish. Every archive carries a `.sha256` sidecar if you want
to verify the download first:

```sh
curl -fsSLO $BASE/infrastore-aarch64-apple-darwin.tar.gz.sha256
shasum -a 256 -c infrastore-aarch64-apple-darwin.tar.gz.sha256   # sha256sum -c on Linux
```

> **macOS.** The binaries are not notarized, so Gatekeeper blocks the first run of a downloaded
> executable. Clear the quarantine flag with `xattr -d com.apple.quarantine ./infrastore`.

### Install from crates.io

```sh
cargo install infrastore-cli      # installs the `infrastore` binary
```

This compiles HDF5 from vendored sources, so it needs `cmake` and a C compiler (see
[Build Prerequisites](#build-prerequisites)) and takes a few minutes on the first build.

## Julia

`InfraStore.jl` is registered in the Julia General registry:

```julia
using Pkg
Pkg.add("InfraStore")
```

The package does not link a system HDF5 or `HDF5_jll`. Its `Artifacts.toml` points at the
`libinfrastore_ffi` tarball attached to the matching GitHub Release, so `Pkg.add` downloads a
prebuilt, statically linked library for the platform — Linux x86_64 and aarch64 (glibc), macOS
x86_64 and Apple Silicon, and Windows x86_64 — and nothing else needs installing. To run against a
locally built library instead (a working tree, or a platform outside that list), set
`INFRASTORE_LIB`; it takes precedence over the artifact. The
[Julia guide](../guides/julia.md#install) has both recipes, and
[Releasing](../releasing.md#5-julia--general) explains why the binary is self-hosted rather than a
JLL.

The rest of this page covers building the workspace from a checkout.

## Build Prerequisites

A Rust toolchain, `cmake`, a C compiler, and `protobuf` for the gRPC codegen. The Python and Julia
bindings additionally need a Python interpreter (3.11+) or Julia (1.10+).

```sh
brew install cmake protobuf maturin              # macOS
sudo apt-get install cmake protobuf-compiler     # Linux (Debian / Ubuntu)
```

The first build compiles HDF5 from source — a few minutes — and then caches the result.

> **Do not set `HDF5_DIR`.** The vendored build forwards it to cmake as `HDF5_ROOT` while still
> requesting static libraries, which fails against a shared-only install. To build against system
> libraries instead, turn vendoring off with `--no-default-features` and install them
> (`brew install hdf5` / `apt-get install libhdf5-dev`).
>
> Because `hdf5-metno-sys` declares `links = "hdf5"`, Cargo's feature unification makes the
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

## Build the Native Library

`InfraStore.jl` and any other C consumer load the C ABI cdylib. Building it is the one target most
people need out of a checkout:

```sh
cargo build -p infrastore-ffi --release
# -> target/release/libinfrastore_ffi.{dylib,so,dll}
# Regenerates the C header at crates/infrastore-ffi/include/infrastore.h
```

Point consumers at it with `INFRASTORE_LIB`, which takes precedence over the artifact `Pkg`
installed:

```sh
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib  # .so on Linux
```

Add it to your shell profile to make it permanent. The Python wheel is built separately with
`maturin` — see [the Python guide](../guides/python.md#from-a-checkout).

## Crates in the Workspace

| Crate               | What it builds                                                       |
| ------------------- | -------------------------------------------------------------------- |
| `infrastore-core`   | Types, HDF5 + SQLite storage, hashing, Rust API                      |
| `infrastore-proto`  | Protobuf service definition + `tonic` codegen                        |
| `infrastore-server` | gRPC server binary + Rust client                                     |
| `infrastore-py`     | PyO3 bindings, `abi3-py311` wheel                                    |
| `infrastore-ffi`    | C ABI cdylib (the foundation of the Julia binding)                   |
| `infrastore-cli`    | `infrastore` CLI binary (CSV add/read, inspect on-disk stores)       |
| `infrastore-bench`  | `infrastore-bench` binary (bulk-ingest + simulation-read benchmarks) |

## Next Steps

- Build a store and round-trip a series in the [Python](./quick-start-python.md),
  [Julia](./quick-start-julia.md), or [CLI](./quick-start-cli.md) Quick Start.
- Read the developer guide for your language: [Python](../guides/python.md) ·
  [Julia](../guides/julia.md) · [Rust](../guides/rust.md) · [CLI](../guides/cli.md).
- Stand up the [gRPC server](../guides/server.md).
