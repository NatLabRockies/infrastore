# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this
repository.

## Project Overview

**castore** is a Rust library for managing time-series data in power-systems / energy simulations.
Persistence is split between numerical arrays in NetCDF4 and metadata associations in SQLite. It
exposes multiple bindings over a shared core:

- **Native Rust** — `castore-core` public API
- **gRPC server + Rust client** — `castore-server` (read-only server; writes need local filesystem
  access)
- **Python** — `castore-py` via PyO3 (abi3-py310 wheel)
- **Julia** — `castore-ffi` C ABI cdylib, wrapped by `julia/Castore.jl`
- **CLI** — `castore-cli` (`cas` binary): loads time series from CSV + a descriptor JSON and
  inspects a store, talking directly to the on-disk NetCDF + SQLite artifact (read+write; no gRPC).
  Output mirrors the `../torc` CLI's global `-f/--format table|json|csv`.

**Current feature coverage:** `SingleTimeSeries` and `NonSequentialTimeSeries` are implemented
end-to-end (read+write in the Rust core, C ABI, Python, and Julia; read-only over gRPC).
`Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, and `Scenarios` support reading
values across the Rust core, C ABI, Python, Julia, and gRPC. Dense forecasts (`Deterministic`,
`Probabilistic`, `Scenarios`) are written through the generic `add_time_series` by passing the
matching forecast object across the Rust core, Python, and Julia (the C ABI keeps per-type
`castore_store_add_forecast` / `castore_store_add_probabilistic` as low-level transport);
`DeterministicSingleTimeSeries` is derived from stored `SingleTimeSeries` via
`transform_single_time_series` rather than added directly. Forecast writes are not exposed over the
read-only gRPC server. Arrays are dtype-generic (`f64`/`f32`/`i64`/`i32`/`u64`/`bool` in every
binding, including Python) and may have multidimensional per-timestep values. The columnar
simulation readers (`StaticReader`/`ForecastReader`) are bound across the Rust core, C ABI, Julia,
and Python. The discovery/maintenance surface (`get_intervals`, `list_names`, `list_owner_types`,
name-pattern filtering via `ListFilter::name_glob` (SQLite `GLOB`), `remove_by_filter`,
`remove_time_series_bulk`, `rename_time_series`, time-sliced `bulk_read`, `AddRequest`/`Store::add`
preserving `logical_type`, and serde on the core types) is available in the Rust core and threaded
through the C ABI/Julia and Python bindings. Two **association catalogs** are available in the Rust
core, C ABI, Julia, and Python, but not over gRPC or the CLI: `supplemental_attribute_associations`
(component ↔ supplemental attribute, the wider surface — counts, counts-by-type, grouped summary)
and `parent_child_associations` (directed component ↔ component edges, e.g. a generator connected to
a bus, deliberately narrower until a consumer needs more). Both are independent of time series in
both directions, and of each other. Metadata getters surface `element_shape` and `features` in every
binding. Python ships type stubs (`castore.pyi` + a pytest drift guard), a full exception hierarchy,
and keyword-only optional arguments; Julia overloads `Base` (`==`/`hash`/`show`/`length`/`iterate`)
and offers do-block `Store`/`open_store` forms. A stored `DeterministicSingleTimeSeries` always
reads back as a `Deterministic` (storage-level view, by design); the DST tag remains visible in
catalog surfaces (keys, metadata, counts). The CLI additionally has `export` (bulk read-direction
inverse of `add`, timestamped forecast CSV), `--name-glob` selectors, `--dry-run` on destructive
commands, store-creation `--compression` flags, shell `completions`, and a `CASTORE_STORE` env
fallback. The read-only gRPC server carries the full read surface too: full `TimeSeriesKey`s over
the wire plus `ListKeys`, `GetMetadata`, `BulkRead`, detailed/per-type counts, `ListOwnerIds`,
`GetIntervals`, static/forecast summaries, `CheckStaticConsistency`, and `ResolveForecastKey`. Auth
is `none` (default) or `api_key` via the `x-api-key` header. See `README.md` and
`docs/src/explanation/data-model.md` for the authoritative feature matrix.

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
- **Toolchain**: The workspace uses Rust edition 2024 and declares an MSRV of Rust 1.94 (see
  `rust-version` in the root `Cargo.toml`, which is the sole authority — there is no
  `rust-toolchain` file). Do not use APIs requiring a newer compiler without intentionally updating
  `rust-version` and CI.
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
  castore-core/    # Types, NetCDF + SQLite storage, hashing, public Rust API
    src/types/               #   array.rs (TypedArray/Dtype), key.rs, metadata.rs, period.rs,
                             #   time_series.rs
    src/storage/             #   memory.rs, netcdf.rs (storage backends)
    src/metadata/            #   schema.rs (SQLite catalog schema)
    src/store.rs             #   Store: the top-level public API
    src/reader.rs            #   StaticReader / ForecastReader: columnar bulk-read surface
    src/hash.rs              #   SHA-256 column hashing
  castore-proto/   # Protobuf service definition + tonic codegen, conversions
  castore-server/  # gRPC server binary (src/bin/server.rs) + Rust client
  castore-py/      # PyO3 bindings
  castore-ffi/     # C ABI cdylib (used by the Julia binding)
  castore-cli/     # `cas` CLI: CSV add/read against an on-disk store (clap, csv, tabled)
  castore-bench/   # `cas-bench` binary: bulk-ingest + simulation-read benchmarks
proto/                       # .proto sources
julia/Castore.jl/    # Julia package wrapping the C ABI
python/tests/                # pytest suite
examples/                    # Sample server config, basic_rust.rs, and cli/ (sample CSV + descriptor)
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

On Windows, CI installs prebuilt `libnetcdf` and `hdf5` from conda-forge and sets `NETCDF_DIR`,
`HDF5_DIR`, and `PKG_CONFIG_PATH` to the conda prefix's `Library` directory. Do not switch this back
to `vcpkg install netcdf-c`: vcpkg builds the stack from source, which fetches libaec from
gitlab.dkrz.de — an unmirrored host that rate-limits CI runners (HTTP 429) and has taken Windows CI
down for hours at a time. Keep these requirements in mind when changing native dependencies.

The workspace cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build --workspace` can link the PyO3 cdylib without `maturin`. On Linux and Windows those
flags are inert.

## Bindings

### Python (PyO3)

```bash
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy
maturin develop --manifest-path crates/castore-py/Cargo.toml
pytest python/tests
```

### Julia (C ABI)

```bash
cargo build -p castore-ffi --release
export CASTORE_LIB=$PWD/target/release/libcastore_ffi.dylib  # .so on Linux
julia --project=julia/Castore.jl -e 'using Pkg; Pkg.instantiate()'
julia --project=julia/Castore.jl julia/Castore.jl/test/runtests.jl
```

The FFI build script generates `crates/castore-ffi/include/castore.h` via `cbindgen`. Never
hand-edit the header. Any change to an exported `extern "C"` function must:

- include an accurate Rustdoc `# Safety` section covering pointer validity, ownership, lengths,
  concurrency, and the matching deallocator;
- regenerate and commit the header;
- update the Julia wrapper and tests when the ABI behavior changes.

## Server

```bash
cp examples/server.toml my_server.toml
# edit my_server.toml: point [data].files at your .nc, set [authentication]
cargo run -p castore-server -- --config my_server.toml
```

`auth = "api_key"` requires at least one entry in `keys`; clients must send the chosen key in the
`x-api-key` header.

## Storage Format

- A persisted store is a NetCDF file plus a SQLite catalog at `<netcdf-path>.sqlite`. They are one
  logical artifact and must be moved, copied, and deleted together.
- `DATA_FORMAT_VERSION` in `crates/castore-core/src/version.rs` is the on-disk compatibility
  contract. Any incompatible NetCDF layout, SQLite schema, dtype encoding, or hashing change must
  bump it and update format documentation and compatibility tests.
- Packed arrays use datasets named `sts_{dtype}_{shape}_{length}_{resolution}` with a companion
  `<dataset>_h` hash variable. Standalone arrays use `arr_{hex_hash}`. See
  `crates/castore-core/src/storage/netcdf.rs` for the implementation and
  `docs/src/reference/file-format.md` for the user-facing specification; keep them synchronized.
- Deletion creates reusable packed slots or unreachable standalone variables. Physical shrinking is
  not available in NetCDF, and compaction behavior must remain explicit.

## Conventions

- Keep the multi-language surface consistent: a change to the core public API usually needs matching
  updates across the proto definitions, the gRPC server/client, the PyO3 bindings, and the FFI/Julia
  binding. When adding a feature, check all bindings before considering it done.
- Treat `castore-core` as the source of truth. Binding crates depend on core; core must not depend
  on bindings.
- Use `TimeSeriesError` and the shared `Result` alias for core errors. Unsupported operations must
  return an explicit error rather than silently changing semantics.
- Preserve typed-array dtype, shape, byte order, timestamps, features, and hashes across every
  binding and persistence round trip.
- Do not manually edit generated artifacts. Besides the C header, protobuf output is generated by
  the proto crate's build script.
- Keep changes scoped. Do not commit local virtual environments, Python caches, generated NetCDF
  test data, or machine-specific library paths.
