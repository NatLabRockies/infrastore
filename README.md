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
- **Seven time-series types** — `SingleTimeSeries`, `NonSequentialTimeSeries`, and
  `PersistentTimeSeries` (a sparse step function: breakpoints plus hold-last) read+write;
  `Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`, and `Scenarios` for forecasts.
- **Feature-tagged associations** — each association carries a map of typed features
  (`int`/`float`/`bool`/`str`), so several variants of a series can coexist under one owner.
- **Columnar simulation readers** — `StaticReader` / `ForecastReader` serve the access pattern that
  drives a simulation: every series' value at one timestamp. `StaticReader` covers all three static
  types, sweeping a `SingleTimeSeries` grid, a cohort of `NonSequentialTimeSeries` sharing one
  timestamp vector, or a set of `PersistentTimeSeries` on breakpoints of their own — the last being
  the one case whose columns need not share a timeline, since a step function has a value at every
  instant from its first breakpoint on.
- **Association catalogs** — `supplemental_attribute_associations` (component ↔ supplemental
  attribute) and `parent_child_associations` (directed component ↔ component edges) record
  relationships independently of time series, so consumers need not keep a SQLite database of their
  own.
- **A stable handle for every series** — each catalog row carries an `id` a consumer can store in
  its own model (a generator's cost function naming the series that varies it). It is never
  reissued, so a reference can go stale but can never come to mean a different series, and it
  survives a rename, a reassignment, a compaction, and a save-and-reopen.
- **Timestamps that round-trip as written** — every series records a `time_reference`: an instant in
  UTC, an instant at a fixed offset, an instant in a named IANA zone, or a wall clock naming no
  instant at all. Each binding infers it from the input type — a naive `datetime` or a bare
  `DateTime` is a wall clock, a `ZoneInfo`/`ZonedDateTime` keeps its zone — and gives the spelling
  back on read instead of relabelling everything UTC at the boundary. Python returns the timestamp
  already in that spelling; Julia returns the instant as a `DateTime` with the reference beside it,
  which `zoned_timestamp` fuses back into a `ZonedDateTime` (a `DateTime` is what its consumers
  destructure today).
- **Discovery and maintenance** — `get_intervals`, `list_names`, `list_owner_types`, glob name
  filters, filtered and bulk delete, rename, time-sliced `read_by_ids_range`, and serde on the core
  types.
- **Read-only gRPC service** — serve a store to remote readers, with optional API-key auth. Writes
  require local filesystem access.
- **Built for power-systems data** — the data model maps onto
  [InfrastructureSystems.jl](https://github.com/Sienna-Platform/InfrastructureSystems.jl) and
  [infrasys](https://github.com/NatLabRockies/infrasys) owners, categories, and time-series
  concepts.

Not every type is available in every binding — the
[feature matrix](https://natlabrockies.github.io/infrastore/latest/explanation/data-model.html) is
authoritative.

## Installation

| Language | Install                                                                                                      |
| -------- | ------------------------------------------------------------------------------------------------------------ |
| Rust     | `cargo add infrastore-core`                                                                                  |
| Python   | `pip install infrastore`                                                                                     |
| CLI      | [download a binary](https://github.com/NatLabRockies/infrastore/releases), or `cargo install infrastore-cli` |
| Julia    | `pkg> add InfraStore`                                                                                        |

Every channel statically links HDF5 and zlib, so there are no system libraries to install. Building
the crates from source needs `cmake` and a C compiler; the Python wheels and the release binaries
are prebuilt.

Each tagged release attaches per-platform archives holding the `infrastore` CLI, the
`infrastore-server` binary, and the `libinfrastore_ffi` C library with its header. `InfraStore.jl`
is registered in the Julia General registry and fetches that C library as a `Pkg` artifact pinned by
its `Artifacts.toml`; set `INFRASTORE_LIB` only to run against a locally built cdylib. See
[Installation](docs/src/getting-started/installation.md) for the per-platform list, and
[Releasing](docs/src/releasing.md) for why the Julia package vendors its own HDF5 rather than
linking `HDF5_jll`.

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
    Features, OwnerCategory, ReadWindow, SingleTimeSeries, TimeSeriesData, TypedArray,
    create_store,
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

// An add hands back the catalog id, which is how every read addresses the
// series from then on.
let id = store.add_time_series(
    42,
    "Generator",
    OwnerCategory::Component,
    TimeSeriesData::SingleTimeSeries(ts),
    Features::new(),
)?;
let got = store.read_by_id(id, ReadWindow::full())?;
```

The full program is
[`crates/infrastore-core/examples/basic.rs`](crates/infrastore-core/examples/basic.rs); run it with
`cargo run -p infrastore-core --example basic`.

### Python

```sh
python3 -m venv .venv && source .venv/bin/activate
pip install maturin pytest numpy tzdata  # tzdata: zoneinfo on Windows
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
# The add returns the catalog id -- the handle every read takes, and the one
# to record in your own object model.
series_id = store.add_time_series(
    owner_id=42, owner_type="Generator",
    owner_category=OwnerCategory.Component,
    time_series=ts,   # name comes from ts
    features={"model_year": 2030}, units="MW",
)
got = store.read_by_id(series_id)
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
# The ZonedDateTime tests need the TimeZones weak dependency, which is only
# loadable through the test target; the run above skips them with a warning:
julia --project=julia/InfraStore.jl -e 'using Pkg; Pkg.test()'
```

```julia
using Dates, InfraStore
store = Store(in_memory=true)
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0), "load")
id = add_time_series!(store, 42, "Generator", Component, ts;
                      features=Dict("model_year" => 2030), units="MW")
got = read_by_id(store, id)
@assert got.data == ts.data
```

The package overloads `Base` (`show`, and `length` / `iterate` / `getindex` on values) and supports
do-block `Store` / `open_store` forms.

## CLI

`infrastore` loads time series from CSV and inspects a store, talking directly to the on-disk HDF5
and SQLite artifact (no gRPC). A global `-f/--format` selects `table` (default), `json`, `jsonl`, or
`csv`; `--store` falls back to the `INFRASTORE_STORE` environment variable; and `-y/--yes` answers
every confirmation prompt.

```sh
cargo build -p infrastore-cli   # builds the `infrastore` binary
IS=target/debug/infrastore

# Numeric values live in a CSV; everything else is described in a descriptor JSON.
$IS template SingleTimeSeries > load.json                    # example descriptor to edit
$IS --store demo.h5 add --descriptor load.json               # creates the store on first add
$IS --store demo.h5 list
$IS --store demo.h5 get  --owner-id 42 --name load           # pretty table
$IS --store demo.h5 -f csv  get  --owner-id 42 --name load   # round-trippable CSV
$IS --store demo.h5 -f json info --owner-id 42 --name load   # metadata + stats
$IS --store demo.h5 get  --name load --plot                  # a terminal sparkline
$IS --store demo.h5 plot --name load --out load.svg          # a self-contained chart
```

The descriptor carries the metadata that does not fit a CSV grid (owner, name, type, dtype,
resolution, timestamps, units, features); the CSV holds only numbers, plus a mandatory header row,
except `NonSequentialTimeSeries` and `PersistentTimeSeries`, whose first column is the timestamp.
Durations are ISO-8601 (`PT1H`, `P1M`). All six dtypes and all six writable types
(`SingleTimeSeries`, `NonSequentialTimeSeries`, `PersistentTimeSeries`, `Deterministic`,
`Probabilistic`, `Scenarios`) are supported — forecast arrays are flat row-major values whose count
equals the product of the type's shape (see `infrastore template <type>`). A descriptor may also set
`"layout": "wide"` to load the canonical `timestamp,gen_001,gen_002,...` file as one scalar series
per column, mapping headers to owner ids through a sidecar CSV, an inline object, or the headers
themselves; `infrastore grid` writes that same shape back out, so the two are an inverse pair. `add`
additionally takes the descriptor fields as flags for a one-off, reads `--descriptor -` from stdin,
and has `--dry-run`, `--replace`, `--batch-size`, and `--quiet`.

Beyond add / list / get / grid / info / transform, the CLI covers discovery (`names`, `owner-types`,
`owners`, `exists` — the last as an exit status), visualization
(`plot --kind
line|duration|heatmap|fan|overlay`, writing one self-contained SVG or HTML file),
inspection (`stats`, `store-info`, `summary`, `verify`, `check-consistency`, `resolutions`,
`params`), content addressing (`arrays`, and the `data_hash` + HDF5 location on `list`/`info`), both
association catalogs read _and_ write (`attributes`, `links`, `attach`, `detach`, `link`, `unlink`,
`reassign`), bulk export (`export`, one timestamped CSV or JSON file per series, re-readable by
`add`), cross-store work (`diff`, which exits nonzero when two catalogs differ, and `merge`), and
maintenance (`init`, `rename`, `copy`, `replace-owner`, `clear`, `persist`, `compact`,
`remove --all`). Destructive commands take `--dry-run`, and `persist` refuses an existing
destination without `--force`. `infrastore completions <shell>` emits shell completions. Full
reference: [CLI](https://natlabrockies.github.io/infrastore/latest/reference/cli.html).

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

The HDF5 file carries the attributes `data_format_version = "0.19.0"` and
`storage_backend = "hdf5"`; a file without the latter is not opened. Packed datasets are named
`sts_{dtype}_{shape}_{length}_{resolution}`, chunked `(1, num_arrays)` so per-timestep reads across
all components are contiguous; a sibling `u8` dataset `<dataset>_h` holds each column's SHA-256 hex
hash as raw bytes, with an all-zero row marking a free slot. Standalone arrays are stored as
`arr_{hex_hash}`, and the explicit time axis a cohort of `NonSequentialTimeSeries` shares as
`timestamps/tsv_{hex_hash}` — one `i64` dataset of unix milliseconds per distinct vector.

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
