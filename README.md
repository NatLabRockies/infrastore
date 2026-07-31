# infrastore

[![Test](https://github.com/NatLabRockies/infrastore/actions/workflows/test.yml/badge.svg)](https://github.com/NatLabRockies/infrastore/actions/workflows/test.yml)
[![codecov](https://codecov.io/gh/NatLabRockies/infrastore/branch/main/graph/badge.svg)](https://codecov.io/gh/NatLabRockies/infrastore)

Rust library for managing time-series data in power-systems and energy simulations. Numerical arrays
are persisted in HDF5, and the metadata associating each array with its owning component lives in
SQLite. Identical arrays are stored once and shared through content addressing.

It ships native Rust, Python (PyO3), and Julia (C ABI) interfaces, the `infrastore` command-line
tool, and a read-only gRPC server with a Rust client.

**Documentation:** <https://natlabrockies.github.io/infrastore/latest/> — start with the
[Quick Start](https://natlabrockies.github.io/infrastore/latest/getting-started/quick-start-python.html)
or the
[Architecture](https://natlabrockies.github.io/infrastore/latest/explanation/architecture.html).

## Status

Under development, unstable API, integrating with parent packages

## Features

- **One array, stored once** — arrays are addressed by a SHA-256 content hash, so a series shared
  across components is written to disk a single time.
- **Typed, N-dimensional values** — `f64`, `f32`, `i64`, `i32`, `u64`, and `bool`, with an optional
  per-timestep element shape (a cost curve's coefficient tuple, say). Dtype, shape, byte order,
  timestamps, features, and hashes survive every binding and round trip.
- **Six time-series types** — `SingleTimeSeries` and `NonSequentialTimeSeries` read+write;
  `Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, and `Scenarios` for forecasts.
- **Feature-tagged associations** — each association carries a map of typed features
  (`int`/`float`/`bool`/`str`), so several variants of a series can coexist under one owner.
- **Columnar simulation readers** — `StaticReader` / `ForecastReader` serve the access pattern that
  drives a simulation: every series' value at one timestamp. `StaticReader` covers both static
  types, sweeping a `SingleTimeSeries` grid or a cohort of `NonSequentialTimeSeries` sharing one
  timestamp vector.
- **Association catalogs** — `supplemental_attribute_associations` (component ↔ supplemental
  attribute) and `parent_child_associations` (directed component ↔ component edges) record
  relationships independently of time series, so consumers need not keep a SQLite database of their
  own.
- **Discovery and maintenance** — `get_intervals`, `list_names`, `list_owner_types`, glob name
  filters, filtered and bulk delete, rename, time-sliced `bulk_read`, and serde on the core types.
- **Read-only gRPC service** — serve a store to remote readers, with optional API-key auth. Writes
  require local filesystem access.
- **Built for power-systems data** — the data model maps onto
  [InfrastructureSystems.jl](https://github.com/NREL-Sienna/InfrastructureSystems.jl) and
  [infrasys](https://github.com/NatLabRockies/infrasys) owners, categories, and time-series
  concepts.

Not every type is available in every binding — the
[feature matrix](https://natlabrockies.github.io/infrastore/latest/explanation/data-model.html) is
authoritative.

## Installation

| Language | Install                        |
| -------- | ------------------------------ |
| Rust     | `cargo add infrastore-core`    |
| Python   | `pip install infrastore`       |
| Julia    | `Pkg.add("InfraStore")`        |
| CLI      | `cargo install infrastore-cli` |

Every channel statically links HDF5 and zlib, so there are no system libraries to install. Building
the crates from source needs `cmake` and a C compiler; the Python wheels and the Julia binary
(`InfraStore_jll`) are prebuilt — see [Releasing](docs/src/releasing.md) for why the Julia package
vendors its own HDF5 rather than linking `HDF5_jll`.

To work against a checkout instead:

| Language | From source                                                       |
| -------- | ----------------------------------------------------------------- |
| Rust     | path or git dependency on `infrastore-core`                       |
| Python   | `maturin develop --manifest-path crates/infrastore-py/Cargo.toml` |
| Julia    | `Pkg.develop(path="julia/InfraStore.jl")` plus `INFRASTORE_LIB`   |
| CLI      | `cargo install --path crates/infrastore-cli`                      |

See [Building from source](#building-from-source) below for the toolchain prerequisites.

## Quick start

### Rust

```rust
use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    Features, OwnerCategory, SingleTimeSeries, TimeSeriesData, TypedArray, create_store,
};

let mut store = create_store(None, true)?;

let values: Vec<f64> = (0..24).map(|i| 100.0 + i as f64).collect();
let data = TypedArray::from_f64(vec![24], &values);
let ts = SingleTimeSeries::new(
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
    Duration::hours(1),
    data,
    "load",
);

let key = store.add_time_series(
    42,
    "Generator",
    OwnerCategory::Component,
    TimeSeriesData::SingleTimeSeries(ts),
    Features::new(),
    Some("MW".into()),
)?;
let got = store.get_time_series(key.identity(), None)?;
```

The full program is
[`crates/infrastore-core/examples/basic.rs`](crates/infrastore-core/examples/basic.rs); run it with
`cargo run -p infrastore-core --example basic`.

### Python

```sh
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy
maturin develop --manifest-path crates/infrastore-py/Cargo.toml
pytest python/tests
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

The wheel ships type stubs (`.pyi`), a full exception hierarchy, keyword-only optional arguments,
and `__eq__` / `__len__` on the value classes.

### Julia

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

The package overloads `Base` (`==` / `hash` on keys via the core identity, `show`, and `length` /
`iterate` / `getindex` on values) and supports do-block `Store` / `open_store` forms.

## CLI

`infrastore` loads time series from CSV and inspects a store, talking directly to the on-disk HDF5
and SQLite artifact (no gRPC). A global `-f/--format` selects `table` (default), `json`, or `csv`,
and `--store` falls back to the `INFRASTORE_STORE` environment variable.

```sh
cargo build -p infrastore-cli   # builds the `infrastore` binary
IS=target/debug/infrastore

# Numeric values live in a CSV; everything else is described in a descriptor JSON.
$IS template single > load.json                              # example descriptor to edit
$IS --store demo.h5 add --descriptor load.json               # creates the store on first add
$IS --store demo.h5 list
$IS --store demo.h5 get  --owner-id 42 --name load           # pretty table
$IS --store demo.h5 -f csv  get  --owner-id 42 --name load   # round-trippable CSV
$IS --store demo.h5 -f json info --owner-id 42 --name load   # metadata + stats
```

The descriptor carries the metadata that does not fit a CSV grid (owner, name, type, dtype,
resolution, timestamps, units, features); the CSV holds only numbers, except `non_sequential`, whose
first column is the timestamp. All six dtypes and all five writable types (`single`,
`non_sequential`, `deterministic`, `probabilistic`, `scenarios`) are supported — forecast arrays are
flat row-major values whose count equals the product of the type's shape (see
`infrastore template <type>`).

Beyond add / list / get / info / transform, the CLI covers inspection (`stats`, `store-info`,
`summary`, `verify`, `check-consistency`, `resolutions`, `params`), content addressing (`arrays`,
and the `data_hash` + HDF5 location on `list`/`info`), the association catalogs (`attributes`,
`links`, read-only), bulk export (`export`, one timestamped CSV or JSON file per series, re-readable
by `add`), and maintenance (`rename`, `copy`, `replace-owner`, `clear`, `persist`, `compact`,
`remove --all`). Destructive commands take `--dry-run`. `infrastore completions <shell>` emits shell
completions. Full reference:
[CLI](https://natlabrockies.github.io/infrastore/latest/reference/cli.html).

## Server

```sh
cp examples/server.toml my_server.toml
# edit my_server.toml: point [data].files at your .h5, set [authentication]
cargo run -p infrastore-server -- --config my_server.toml
```

The server is read-only. `auth` is `none` (default) or `api_key`; `api_key` requires at least one
entry in `keys`, and clients must send the chosen key in the `x-api-key` header.

## Storage format

A persisted store is **two files that travel together**: an HDF5 file and a SQLite catalog at
`<store-path>.sqlite`. Copying, moving, or deleting one without the other corrupts the store.

The HDF5 file carries the attributes `data_format_version = "0.11.0"` and
`storage_backend = "hdf5"`; a file without the latter is not opened. Packed datasets are named
`sts_{dtype}_{shape}_{length}_{resolution}`, chunked `(1, num_arrays)` so per-timestep reads across
all components are contiguous; a sibling `u8` dataset `<dataset>_h` holds each column's SHA-256 hex
hash as raw bytes, with an all-zero row marking a free slot. Standalone arrays are stored as
`arr_{hex_hash}`.

Deletion frees packed slots for reuse rather than shrinking the file — HDF5 cannot reclaim the space
in place, so reclaiming it is an explicit `Store::compact()`. The exact bytes are specified in the
[On-Disk File Format](https://natlabrockies.github.io/infrastore/latest/reference/file-format.html).

## Repo layout

```
crates/
  infrastore-core/     # Types, HDF5 + SQLite storage, hashing, public Rust API
  infrastore-proto/    # Protobuf service definition (proto/) + tonic codegen
  infrastore-server/   # gRPC server binary + Rust client
  infrastore-py/       # PyO3 bindings, abi3-py311 wheel
  infrastore-ffi/      # C ABI cdylib (used by the Julia binding)
  infrastore-cli/      # `infrastore` CLI: load CSV + inspect a store on disk
  infrastore-bench/    # `infrastore-bench` binary: ingestion + simulation-read benchmarks
julia/InfraStore.jl/   # Julia package wrapping the C ABI
python/tests/          # pytest suite
docs/                  # mdBook sources for the documentation site
examples/              # Sample server config and cli/ sample CSV + descriptor
```

## Building from source

### Prerequisites

HDF5 and zlib are **built from vendored sources and linked statically by default**, so you do not
need to install them. The build needs `cmake` and a C compiler, plus `protobuf` for the gRPC
codegen:

```sh
brew install cmake protobuf maturin              # macOS
sudo apt-get install cmake protobuf-compiler     # Linux (Debian/Ubuntu)
```

The first build compiles HDF5 from source, which takes a few minutes; the result is cached and later
builds are unaffected. The cdylib tests additionally need Python 3.11+ and Julia 1.10+.

### Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The workspace cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build --workspace` can link the PyO3 cdylib without `maturin`. On Linux and Windows those
flags are inert.

### Linking against system HDF5 instead

Every crate enables a `vendored` feature by default. Turn it off to link the system libraries:

```sh
cargo build --workspace --no-default-features
```

That path needs the development package — `brew install hdf5` or `sudo apt-get install libhdf5-dev`
— and the `hdf5-metno-sys` build script does not always locate HDF5 on its own. If the build fails
with `Unable to locate HDF5 root directory and/or headers`, point it at the install explicitly:

```sh
export HDF5_DIR="$(brew --prefix hdf5)"                  # macOS
export HDF5_DIR=/usr/lib/x86_64-linux-gnu/hdf5/serial    # Debian/Ubuntu
```

Because `hdf5-metno-sys` declares `links = "hdf5"`, there is exactly one copy of it in any
dependency graph and Cargo unifies features across the whole graph. Vendored-versus-system is
therefore an all-or-nothing choice for a given build, not something an individual crate can pick.

## Contributing

Changes must pass `cargo fmt`, `cargo clippy -D warnings`, `cargo test`, `dprint check`, and
`cargo deny check` — see
[Contributing](https://natlabrockies.github.io/infrastore/latest/contributing.html) for the
conventions, including the rules that govern the on-disk format contract.

## License

BSD 3-Clause. See [LICENSE](LICENSE).

## Disclaimer

This software was generated using artificial intelligence and may contain errors. See
[DISCLAIMER.md](DISCLAIMER.md) before relying on it.
