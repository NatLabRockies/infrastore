# infrastore

Rust library for managing time-series data in power-systems / energy simulations. Persistence is
split between numerical arrays in NetCDF4 and metadata associations in SQLite. Bindings: native
Rust, gRPC server + Rust client, Python (via PyO3), Julia (via C ABI), and the `infrastore` CLI.

Spec:
[NatLabRockies/time-series-store#1](https://github.com/NatLabRockies/time-series-store/issues/1).

Project status: under development

## Why the name

**infrastore** = **infra**structure + **store** — a store for the time series and metadata behind
infrastructure models. Persistence is deliberately split in two: a metadata/association catalog
(SQLite) alongside an array store (NetCDF). Those halves are twins, not alternatives — a persisted
store is a pair of files, the `.nc` and its `.sqlite` sidecar, that must always travel together.

## v0 scope

- **SingleTimeSeries** and **NonSequentialTimeSeries** are implemented end-to-end (read+write in the
  Rust core, C ABI, Python, and Julia; read-only over gRPC). The four forecast types
  (`Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, `Scenarios`) support reading
  values across the Rust core, C ABI, Python, Julia, and gRPC. Dense forecasts (`Deterministic`,
  `Probabilistic`, `Scenarios`) are written through the generic `add_time_series` by passing the
  matching forecast object across the Rust core, Python, and Julia (the C ABI keeps per-type
  `infrastore_store_add_forecast` / `infrastore_store_add_probabilistic` as low-level transport);
  `DeterministicSingleTimeSeries` is not added directly — it is derived from stored
  `SingleTimeSeries` via `transform_single_time_series`. Forecast writes are not exposed over the
  read-only gRPC server.
- Multi-dim per-step values (e.g. quadratic-curve coefficients) are supported: arrays carry an
  element `dtype` and a `(length, *element_shape)` shape, and the NetCDF backend persists the
  trailing element axes.
- Columnar **readers** (`StaticReader` / `ForecastReader`) for the simulation access pattern — every
  series' value at one timestamp — are exposed across the Rust core, C ABI, Julia, and Python.
- Discovery / maintenance surface across the bindings: `get_intervals`, `list_names`,
  `list_owner_types`, name-pattern filtering (`ListFilter::name_glob`, SQLite `GLOB` semantics),
  filtered/bulk delete (`remove_by_filter`, `remove_time_series_bulk`), `rename_time_series`,
  time-sliced `bulk_read`, and serde on the core types.
- **Association catalogs**: two tables recording relationships between catalog entities,
  independently of time series, so consumers no longer keep a SQLite database of their own for them.
  `supplemental_attribute_associations` records which attributes are attached to which components
  (the wider surface: add, bulk add, `has_`, `list_`, both id directions, remove, three counts,
  counts-by-type, a grouped summary, and a component rewrite). `parent_child_associations` records
  directed edges between components — a generator connected to a bus, say — with a narrower surface
  (`list_children` / `list_parents`, remove, count, rewrite). Both are available in the Rust core, C
  ABI, Julia, and Python; neither is exposed over gRPC or the CLI.
- Language-idiomatic bindings: the Python wheel ships type stubs (`.pyi`), a full exception
  hierarchy, keyword-only optional arguments, and `__eq__`/`__len__` on the value classes; the Julia
  package overloads `Base` (`==`/`hash` on keys via the core identity, `show`,
  `length`/`iterate`/`getindex` on values) and supports do-block `Store`/`open_store` forms.
- Read-only gRPC server. Writes require local filesystem access.
- The `infrastore` CLI covers inspection (`stats`, `summary`, `verify`, `check-consistency`,
  `resolutions`, `params`), bulk export (`export`: timestamped CSV or structured JSON, one file per
  series), and maintenance (`rename`, `copy`, `replace-owner`, `clear`, `persist`, `compact`,
  `remove --all` — destructive commands take `--dry-run`) in addition to add / list / get / info /
  transform, plus `completions` and a `INFRASTORE_STORE` env fallback for `--store`.
- Auth: `none` (default) or `api_key` via the `x-api-key` header.

## Repo layout

```
crates/
  infrastore-core/    # Types, NetCDF + SQLite storage, hashing, public Rust API
  infrastore-proto/   # Protobuf service definition + tonic codegen
  infrastore-server/  # gRPC server binary + Rust client
  infrastore-py/      # PyO3 bindings, abi3-py310 wheel
  infrastore-ffi/     # C ABI cdylib (used by the Julia binding)
  infrastore-cli/     # `infrastore` CLI: load CSV + inspect a store on disk
  infrastore-bench/   # `infrastore-bench` binary: ingestion + simulation-read benchmarks
proto/                       # .proto sources
julia/InfraStore.jl/    # Julia package wrapping the C ABI (InfraStore.jl)
python/tests/                # pytest suite
examples/                    # Sample server config, basic_rust.rs, and cli/ sample CSV + descriptor
```

## Prerequisites

NetCDF, HDF5, and zlib are **built from vendored sources and linked statically by default**, so you
do not need to install them. What the build does need is `cmake` and a C compiler, plus `protobuf`
for the gRPC codegen.

On macOS via `brew`:

```sh
brew install cmake protobuf maturin
```

On Linux (Debian/Ubuntu):

```sh
sudo apt-get install cmake protobuf-compiler
```

The first build compiles netcdf-c and HDF5 from source, which takes a few minutes; the result is
cached and later builds are unaffected.

The cdylib tests need a Python interpreter (3.10+) and Julia (1.10+).

### Linking against system NetCDF instead

Every crate enables a `vendored` feature by default. Turn it off to link the system libraries:

```sh
cargo build --workspace --no-default-features
```

That path needs the development packages — `brew install hdf5 netcdf` or
`sudo apt-get install libhdf5-dev libnetcdf-dev` — and the `hdf5-metno-sys` build script does not
always locate HDF5 on its own. If the build fails with
`Unable to locate HDF5 root directory and/or headers`, point it at the install explicitly:

```sh
export HDF5_DIR="$(brew --prefix hdf5)"           # macOS
export HDF5_DIR=/usr/lib/x86_64-linux-gnu/hdf5/serial   # Debian/Ubuntu
```

Because `netcdf-sys` declares `links = "netcdf"`, there is exactly one copy of it in any dependency
graph and Cargo unifies features across the whole graph. Vendored-vs-system is therefore an
all-or-nothing choice for a given build, not something an individual crate can pick.

## Build

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The workspace cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build --workspace` can link the PyO3 cdylib without `maturin`. On Linux, those flags are
inert.

## Python bindings

```sh
cd crates/infrastore-py
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy
maturin develop
pytest ../../python/tests
```

```python
from datetime import datetime, timedelta, timezone
import numpy as np
from infrastore import Store, SingleTimeSeries, OwnerCategory

store = Store.create(in_memory=True)
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc),
    timedelta(hours=1),
    np.arange(24, dtype=np.float64) + 100,
    "load",   # name (required)
)
key = store.add_time_series(
    owner_id=42, owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,   # name comes from ts
    features={"model_year": 2030}, units="MW",
)
got = store.get_time_series(key)
assert np.array_equal(np.asarray(got.data), np.asarray(ts.data))
```

## Julia bindings

```sh
cargo build -p infrastore-ffi --release
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib  # .so on Linux
julia --project=julia/InfraStore.jl -e 'using Pkg; Pkg.instantiate()'
julia --project=julia/InfraStore.jl julia/InfraStore.jl/test/runtests.jl
```

```julia
using Dates, InfraStore
store = Store(in_memory=true)
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0), "load")
key = add_time_series!(store, 42, "Generator", Component, ts;
                       features=Dict("model_year" => 2030), units="MW")
got = get_time_series(store, key)
@assert got.data == ts.data
```

## CLI (`infrastore`)

`infrastore` is a command-line tool that loads time series from CSV and inspects a store, talking
directly to the on-disk NetCDF + SQLite artifact (no gRPC). Output follows a convention of a global
`-f/--format` with `table` (default), `json`, and `csv`.

```sh
cargo build -p infrastore-cli   # builds the `infrastore` binary
CAS=target/debug/infrastore

# Numeric values live in a CSV; everything else is described in a descriptor JSON.
$CAS template single > load.json       # print an example descriptor to edit
$CAS --store demo.nc add --descriptor load.json
$CAS --store demo.nc list
$CAS --store demo.nc get  --owner-id 42 --name load              # pretty table
$CAS --store demo.nc -f csv  get  --owner-id 42 --name load      # round-trippable CSV
$CAS --store demo.nc -f json info --owner-id 42 --name load      # metadata + stats
```

The descriptor carries the metadata that does not fit a CSV grid (owner, name, type, dtype,
resolution, timestamps, units, features); the CSV holds only numbers, except `non_sequential`, whose
first column is the timestamp. All six dtypes (`f64|f32|i64|i32|u64|bool`) and all five writable
types (`single`, `non_sequential`, `deterministic`, `probabilistic`, `scenarios`) are supported —
forecast arrays are laid out as flat row-major values whose count equals the product of the type's
shape (see `infrastore template <type>`). `infrastore transform --horizon <D> --interval <D>`
derives `DeterministicSingleTimeSeries` from stored `SingleTimeSeries`. The store is created on
first `add`.

## Server

```sh
cp examples/server.toml my_server.toml
# edit my_server.toml: point [data].files at your .nc, set [authentication]
cargo run -p infrastore-server -- --config my_server.toml
```

`auth = "api_key"` requires at least one entry in `keys`. Clients must send the chosen key in the
`x-api-key` header.

## Storage format

NetCDF file with attribute `data_format_version = "0.10.0"`. Each packed dataset is named
`sts_{dtype}_{shape}_{length}_{resolution}` (per-timestep reads across all components are
contiguous). A sibling string variable `<dataset>_h` holds the SHA-256 hex hash for each column; an
empty string marks a free slot. Standalone arrays are stored as `arr_{hex_hash}`.

Metadata lives in a catalog SQLite file at `<path>.sqlite`. Two artifacts ship together; an
`archive` helper that bundles them is post-v0. The catalog also holds the two association tables,
which were added additively — a store written before they existed gains them on its first writable
open, and a read-only open of such a store reports no associations rather than failing, so no
`data_format_version` bump was needed.

## Open questions resolved for v0

|                    | Decision                                                  |
| ------------------ | --------------------------------------------------------- |
| Compaction trigger | Explicit `Store::compact()` only                          |
| Server auth        | `none` default, `api_key` implemented, `oauth` deferred   |
| Units              | `Option<String>` free-form label, no dimensional analysis |
| NetCDF chunking    | `(1, num_arrays)` — per-timestep reads contiguous         |

## Status

- Covered by Rust, Python, and Julia test suites across the core, bindings, and round trips.
- Workspace clippy-clean on edition 2024; MSRV 1.94.
