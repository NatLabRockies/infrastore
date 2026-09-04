# AGENTS.md

## Project Overview

**infrastore** is a Rust library for managing time-series data in power-systems / energy
simulations. Persistence is split between numerical arrays in HDF5 and metadata associations in
SQLite. It exposes multiple bindings over a shared core:

- **Native Rust** — `infrastore-core` public API
- **gRPC server + Rust client** — `infrastore-server` (read-only server; writes need local
  filesystem access)
- **Python** — `infrastore-py` via PyO3 (abi3-py311 wheel)
- **Julia** — `infrastore-ffi` C ABI cdylib, wrapped by `julia/InfraStore.jl`
- **CLI** — `infrastore-cli` (`infrastore` binary): loads time series from CSV + a descriptor JSON
  and inspects a store, talking directly to the on-disk HDF5 + SQLite artifact (read+write; no
  gRPC). Output uses a global `-f/--format table|json|csv`.

**Current feature coverage:** `SingleTimeSeries` and `NonSequentialTimeSeries` are implemented
end-to-end (read+write in the Rust core, C ABI, Python, and Julia; read-only over gRPC).
`Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, and `Scenarios` support reading
values across the Rust core, C ABI, Python, Julia, and gRPC. Dense forecasts (`Deterministic`,
`Probabilistic`, `Scenarios`) are written through the generic `add_time_series` by passing the
matching forecast object across the Rust core, Python, and Julia (the C ABI keeps per-type
`infrastore_store_add_forecast` / `infrastore_store_add_probabilistic` as low-level transport);
`DeterministicSingleTimeSeries` is derived from stored `SingleTimeSeries` via
`transform_single_time_series` rather than added directly. The gRPC service is read-only — every RPC
it defines is a read — so no time-series type can be written over it. Arrays are dtype-generic
(`f64`/`f32`/`i64`/`i32`/`u64`/`bool` in every binding, including Python) and may have
multidimensional per-timestep values. Auth is `none` (default) or `api_key` via the `x-api-key`
header. See `README.md` and `docs/src/explanation/data-model.md` for the authoritative feature
matrix.

The two association catalogs also round-trip as OpenAPI-row JSON in SiennaSchemas' wire spelling,
which is what lets an artifact be read back from **arrays plus a document alone**, with no `.sqlite`
carried along: `Store::open_without_catalog` opens the array half of such a bundle and mints an
empty catalog stamped to match, and the imports replay the rows into it, association ids preserved.
Incoming rows are validated against the schemas vendored at `crates/infrastore-core/sienna_schemas/`
and compiled into the crate. All six time-series types round-trip: a `NonSequentialTimeSeries`
locates its time axis with `timestamps_uri`, which it needs because content-addressed arrays are
shared across axes and so cannot imply one.

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
  infrastore-core/    # Types, HDF5 + SQLite storage, hashing, public Rust API
    src/types/               #   array.rs (TypedArray/Dtype), key.rs, metadata.rs, period.rs,
                             #   time_series.rs
    src/storage/             #   memory.rs, hdf5.rs (storage backends)
    src/metadata/            #   schema.rs (SQLite catalog schema)
    src/store.rs             #   Store: the top-level public API
    src/reader.rs            #   StaticReader / ForecastReader: columnar bulk-read surface
    src/hash.rs              #   SHA-256 column hashing
  infrastore-proto/   # Protobuf service definition (proto/) + tonic codegen, conversions
  infrastore-server/  # gRPC server binary (src/bin/server.rs) + Rust client
  infrastore-py/      # PyO3 bindings
  infrastore-ffi/     # C ABI cdylib (used by the Julia binding)
  infrastore-cli/     # `infrastore` CLI: CSV add/read against an on-disk store (clap, csv, tabled)
  infrastore-bench/   # `infrastore-bench` binary: bulk-ingest + simulation-read benchmarks
julia/InfraStore.jl/    # Julia package wrapping the C ABI
python/tests/                # pytest suite
examples/                    # Sample server config and cli/ (sample CSV + descriptor)
.github/workflows/           # Cross-platform tests, linting, security, wheel builds
```

## Build & Test

```bash
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

### Prerequisites

HDF5 and zlib are built from vendored sources and linked statically by default, via the `vendored`
feature that every crate enables (defined in `infrastore-core`, forwarded by the rest). The build
therefore needs `cmake` and a C compiler rather than a system HDF5, plus `protobuf` for the gRPC
codegen:

```bash
brew install cmake protobuf maturin                 # macOS
sudo apt-get install cmake protobuf-compiler        # Linux (Debian/Ubuntu)
```

The first build compiles HDF5 from source (a few minutes), then caches the result.

`--no-default-features` switches back to system libraries, which then need `brew install hdf5` /
`sudo apt-get install libhdf5-dev`. Never set `HDF5_DIR` to work around a vendored build failure: it
redirects the vendored build at an external HDF5 while still requesting static libraries, which
fails. Because `hdf5-metno-sys` declares `links = "hdf5"`, Cargo's feature unification makes
vendored-vs-system all-or-nothing across the whole dependency graph.

CI provisions no native libraries on any platform, Windows included — the vendored build covers all
three. Keep these requirements in mind when changing native dependencies.

The workspace cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build --workspace` can link the PyO3 cdylib without `maturin`. On Linux and Windows those
flags are inert.

## Bindings

### Python (PyO3)

```bash
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy
maturin develop --manifest-path crates/infrastore-py/Cargo.toml
pytest python/tests
```

### Julia (C ABI)

```bash
cargo build -p infrastore-ffi --release
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib  # .so on Linux
julia --project=julia/InfraStore.jl -e 'using Pkg; Pkg.instantiate()'
julia --project=julia/InfraStore.jl julia/InfraStore.jl/test/runtests.jl
```

The FFI build script generates `crates/infrastore-ffi/include/infrastore.h` via `cbindgen`. Never
hand-edit the header. Any change to an exported `extern "C"` function must:

- include an accurate Rustdoc `# Safety` section covering pointer validity, ownership, lengths,
  concurrency, and the matching deallocator;
- regenerate and commit the header;
- update the Julia wrapper and tests when the ABI behavior changes.

## Server

```bash
cp examples/server.toml my_server.toml
# edit my_server.toml: point [data].files at your .h5, set [authentication]
cargo run -p infrastore-server -- --config my_server.toml
```

`auth = "api_key"` requires at least one entry in `keys`; clients must send the chosen key in the
`x-api-key` header.

## Storage Format

- A persisted store is an HDF5 file plus a SQLite catalog at `<store-path>.sqlite`. They are one
  logical artifact and must be moved, copied, and deleted together. The file is written directly
  against libhdf5 (via `hdf5-metno`), not through netcdf-c; the extension is conventionally `.h5`
  but nothing enforces it. Identity comes from the root attribute `storage_backend = "hdf5"`, and
  `Store::open` rejects a file that lacks it — including stores written by the removed netcdf
  backend.
- `DATA_FORMAT_VERSION` in `crates/infrastore-core/src/version.rs` is the on-disk compatibility
  contract, checked in **three tiers** (`Current` / `Upgradable` / `Incompatible`), not by equality.
  Any incompatible HDF5 layout, dtype encoding, timestamp encoding, or hashing change must bump it,
  raise `MIN_UPGRADABLE_VERSION` to match, and update format documentation and compatibility tests.
- **`CATALOG_SCHEMA_REVISION` (`crates/infrastore-core/src/metadata/migrate.rs`) is the SQLite
  half's own contract. Any catalog change the idempotent DDL cannot make to an existing table — a
  new column, a changed CHECK, a rebuilt table, a backfill — now requires a
  `CATALOG_SCHEMA_REVISION` bump plus an append-only `MIGRATIONS` entry, not a re-created store.**
  Never edit a landed migration; add a new one. A writable open climbs the ladder and then re-stamps
  the HDF5 half; a read-only open of a stale catalog reports `CatalogMigrationRequired` (open it
  once for writing). A purely additive new _table_ or _index_ still needs neither bump.
- Packed arrays use datasets named `sts_{dtype}_{shape}_{length}_{resolution}` with a companion
  `<dataset>_h` hash dataset. Standalone arrays use `arr_{hex_hash}`. See
  `crates/infrastore-core/src/storage/hdf5.rs` for the implementation and
  `docs/src/reference/file-format.md` for the user-facing specification; keep them synchronized.
- Deletion creates reusable packed slots or tombstoned standalone datasets. HDF5 cannot reclaim the
  space in place, and compaction behavior must remain explicit.

## Conventions

- Keep the multi-language surface consistent: a change to the core public API usually needs matching
  updates across the proto definitions, the gRPC server/client, the PyO3 bindings, and the FFI/Julia
  binding. When adding a feature, check all bindings before considering it done.
- Treat `infrastore-core` as the source of truth. Binding crates depend on core; core must not
  depend on bindings.
- Use `TimeSeriesError` and the shared `Result` alias for core errors. Unsupported operations must
  return an explicit error rather than silently changing semantics.
- Preserve typed-array dtype, shape, byte order, timestamps, features, and hashes across every
  binding and persistence round trip.
- Do not manually edit generated artifacts. Besides the C header, protobuf output is generated by
  the proto crate's build script.
- Keep changes scoped. Do not commit local virtual environments, Python caches, generated HDF5 test
  data, or machine-specific library paths.
