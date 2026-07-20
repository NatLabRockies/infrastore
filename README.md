# time-series-store

Rust library for managing time-series data in power-systems / energy simulations. Persistence is
split between numerical arrays in NetCDF4 and metadata associations in SQLite. Bindings: native
Rust, gRPC server + Rust client, Python (via PyO3), Julia (via C ABI), and the `tss` CLI.

Spec:
[NatLabRockies/time-series-store#1](https://github.com/NatLabRockies/time-series-store/issues/1).

## v0 scope

- **SingleTimeSeries** and **NonSequentialTimeSeries** are implemented end-to-end (read+write in the
  Rust core, C ABI, Python, and Julia; read-only over gRPC). The four forecast types
  (`Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, `Scenarios`) support reading
  values across the Rust core, C ABI, Python, Julia, and gRPC. Dense forecasts (`Deterministic`,
  `Probabilistic`, `Scenarios`) are written through the generic `add_time_series` by passing the
  matching forecast object across the Rust core, Python, and Julia (the C ABI keeps per-type
  `ts_store_add_forecast` / `ts_store_add_probabilistic` as low-level transport);
  `DeterministicSingleTimeSeries` is not added directly — it is derived from stored
  `SingleTimeSeries` via `transform_single_time_series`. Forecast writes are not exposed over the
  read-only gRPC server.
- Multi-dim per-step values (e.g. quadratic-curve coefficients) are supported: arrays carry an
  element `dtype` and a `(length, *element_shape)` shape, and the NetCDF backend persists the
  trailing element axes.
- Columnar **readers** (`StaticReader` / `ForecastReader`) for the simulation access pattern — every
  series' value at one timestamp — are exposed across the Rust core, C ABI, Julia, and Python.
- Discovery / maintenance surface across the bindings: `get_intervals`, `list_names`,
  `list_owner_types`, filtered/bulk delete (`remove_by_filter`, `remove_time_series_bulk`),
  `rename_time_series`, time-sliced `bulk_read`, and serde on the core types.
- Read-only gRPC server. Writes require local filesystem access.
- The `tss` CLI covers inspection (`stats`, `summary`, `verify`, `check-consistency`, `resolutions`,
  `params`) and maintenance (`rename`, `copy`, `replace-owner`, `clear`, `persist`, `compact`,
  `remove --all`) in addition to add / list / get / info / transform.
- Auth: `none` (default) or `api_key` via the `x-api-key` header.

## Repo layout

```
crates/
  time-series-store-core/    # Types, NetCDF + SQLite storage, hashing, public Rust API
  time-series-store-proto/   # Protobuf service definition + tonic codegen
  time-series-store-server/  # gRPC server binary + Rust client
  time-series-store-py/      # PyO3 bindings, abi3-py310 wheel
  time-series-store-ffi/     # C ABI cdylib (used by the Julia binding)
  time-series-store-cli/     # `tss` CLI: load CSV + inspect a store on disk
  time-series-store-bench/   # `tss-bench` binary: ingestion + simulation-read benchmarks
proto/                       # .proto sources
julia/TimeSeriesStore.jl/    # Julia package wrapping the C ABI (TimeSeriesStore.jl)
python/tests/                # pytest suite
examples/                    # Sample server config, basic_rust.rs, and cli/ sample CSV + descriptor
```

## Prerequisites

System libraries (macOS via `brew`):

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

On Linux (Debian/Ubuntu), install the equivalent packages:

```sh
sudo apt-get install libhdf5-dev libnetcdf-dev protobuf-compiler
# if the build script can't find HDF5:
export HDF5_DIR=/usr/lib/x86_64-linux-gnu/hdf5/serial
```

The cdylib tests need a Python interpreter (3.10+) and Julia (1.10+).

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
cd crates/time-series-store-py
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy
maturin develop
pytest ../../python/tests
```

```python
from datetime import datetime, timedelta, timezone
import numpy as np
from time_series_store import TimeSeriesStore, SingleTimeSeries, OwnerCategory

store = TimeSeriesStore.create(in_memory=True)
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
cargo build -p time-series-store-ffi --release
export TIME_SERIES_STORE_LIB=$PWD/target/release/libtime_series_store_ffi.dylib  # .so on Linux
julia --project=julia/TimeSeriesStore.jl -e 'using Pkg; Pkg.instantiate()'
julia --project=julia/TimeSeriesStore.jl julia/TimeSeriesStore.jl/test/runtests.jl
```

```julia
using Dates, TimeSeriesStore
store = Store(in_memory=true)
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0), "load")
key = add_time_series!(store, 42, "Generator", Component, ts;
                       features=Dict("model_year" => 2030), units="MW")
got = get_time_series(store, key)
@assert got.data == ts.data
```

## CLI (`tss`)

`tss` is a command-line tool that loads time series from CSV and inspects a store, talking directly
to the on-disk NetCDF + SQLite artifact (no gRPC). Output follows the `torc` convention of a global
`-f/--format` with `table` (default), `json`, and `csv`.

```sh
cargo build -p time-series-store-cli   # builds the `tss` binary
TSS=target/debug/tss

# Numeric values live in a CSV; everything else is described in a descriptor JSON.
$TSS template single > load.json       # print an example descriptor to edit
$TSS --store demo.nc add --descriptor load.json
$TSS --store demo.nc list
$TSS --store demo.nc get  --owner-id 42 --name load              # pretty table
$TSS --store demo.nc -f csv  get  --owner-id 42 --name load      # round-trippable CSV
$TSS --store demo.nc -f json info --owner-id 42 --name load      # metadata + stats
```

The descriptor carries the metadata that does not fit a CSV grid (owner, name, type, dtype,
resolution, timestamps, units, features); the CSV holds only numbers, except `non_sequential`, whose
first column is the timestamp. All six dtypes (`f64|f32|i64|i32|u64|bool`) and all five writable
types (`single`, `non_sequential`, `deterministic`, `probabilistic`, `scenarios`) are supported —
forecast arrays are laid out as flat row-major values whose count equals the product of the type's
shape (see `tss template <type>`). `tss transform --horizon <D> --interval <D>` derives
`DeterministicSingleTimeSeries` from stored `SingleTimeSeries`. The store is created on first `add`.

## Server

```sh
cp examples/server.toml my_server.toml
# edit my_server.toml: point [data].files at your .nc, set [authentication]
cargo run -p time-series-store-server -- --config my_server.toml
```

`auth = "api_key"` requires at least one entry in `keys`. Clients must send the chosen key in the
`x-api-key` header.

## Storage format

NetCDF file with attribute `data_format_version = "0.10.0"`. Each packed dataset is named
`sts_{dtype}_{shape}_{length}_{resolution}` (per-timestep reads across all components are
contiguous). A sibling string variable `<dataset>_h` holds the SHA-256 hex hash for each column; an
empty string marks a free slot. Standalone arrays are stored as `arr_{hex_hash}`.

Metadata lives in a catalog SQLite file at `<path>.sqlite`. Two artifacts ship together; an
`archive` helper that bundles them is post-v0.

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
