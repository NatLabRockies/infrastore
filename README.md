# time-series-store

Rust library for managing time-series data in power-systems / energy
simulations. Persistence is split between numerical arrays in NetCDF4 and
metadata associations in SQLite. Bindings: native Rust, gRPC server + Rust
client, Python (via PyO3), Julia (via C ABI).

Spec: [NatLabRockies/time-series-store#1](https://github.com/NatLabRockies/time-series-store/issues/1).

## v0 scope

- **SingleTimeSeries** is the only time-series type implemented end-to-end.
  Slots for the other five (NonSequentialTimeSeries, Deterministic,
  DeterministicSingleTimeSeries, Probabilistic, Scenarios) are reserved in the
  metadata schema and the `TimeSeriesType` enum so they can land later without
  breaking changes.
- 1-D `data` only. Multi-dim per-step values (e.g. quadratic-curve coefficients)
  are accepted by the in-memory backend but rejected with `InvalidParameter` by
  the NetCDF backend.
- Read-only gRPC server. Writes require local filesystem access.
- Auth: `none` (default) or `api_key` via the `x-api-key` header.

## Repo layout

```
crates/
  time-series-store-core/    # Types, NetCDF + SQLite storage, hashing, public Rust API
  time-series-store-proto/   # Protobuf service definition + tonic codegen
  time-series-store-server/  # gRPC server binary + Rust client
  time-series-store-py/      # PyO3 bindings, abi3-py310 wheel
  time-series-store-ffi/     # C ABI cdylib (used by the Julia binding)
proto/                       # .proto sources
julia/TimeSeries.jl/         # Julia package wrapping the C ABI
python/tests/                # pytest suite
examples/                    # Sample server config
```

## Prerequisites

System libraries (macOS via `brew`, equivalent on Linux):

```sh
brew install netcdf protobuf maturin
```

The cdylib tests need a Python interpreter (3.10+) and Julia (1.10+).

## Build

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The workspace cargo config (`.cargo/config.toml`) sets macOS linker flags so
`cargo build --workspace` can link the PyO3 cdylib without `maturin`. On Linux,
those flags are inert.

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
from time_series import TimeSeriesStore, SingleTimeSeries, OwnerCategory

store = TimeSeriesStore.create(in_memory=True)
ts = SingleTimeSeries(
    datetime(2024, 1, 1, tzinfo=timezone.utc),
    timedelta(hours=1),
    np.arange(24, dtype=np.float64) + 100,
)
key = store.add_time_series(
    owner_id=42, owner_type="Generator",
    owner_category=OwnerCategory.Component,
    name="load", time_series=ts,
    features={"model_year": 2030}, units="MW",
)
got = store.get_time_series(key)
assert np.array_equal(np.asarray(got.data), np.asarray(ts.data))
```

## Julia bindings

```sh
cargo build -p time-series-store-ffi --release
export TIME_SERIES_STORE_LIB=$PWD/target/release/libtime_series_store_ffi.dylib  # .so on Linux
julia --project=julia/TimeSeries.jl -e 'using Pkg; Pkg.instantiate()'
julia --project=julia/TimeSeries.jl julia/TimeSeries.jl/test/runtests.jl
```

```julia
using Dates, TimeSeries
store = TimeSeriesStore(in_memory=true)
ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(100.0:123.0))
key = add_time_series!(store, 42, "Generator", Component, "load", ts;
                       features=Dict("model_year" => 2030), units="MW")
got = get_time_series(store, key)
@assert got.data == ts.data
```

## Server

```sh
cp examples/server.toml my_server.toml
# edit my_server.toml: point [data].files at your .nc, set [authentication]
cargo run -p time-series-store-server -- --config my_server.toml
```

`auth = "api_key"` requires at least one entry in `keys`. Clients must send the
chosen key in the `x-api-key` header.

## Storage format

NetCDF file with attribute `data_format_version = "0.1.0"` and group
`time_series/single/`. Each compacted dataset is named
`sts_{length}_{resolution_seconds}` with shape `(length, 1000)` and chunking
`(1, 1000)` (per-timestep reads across all components are contiguous).
A sibling string variable `<dataset>_h` holds the SHA-256 hex hash for each
column; an empty string marks a free slot.

Metadata lives in a sidecar SQLite file at `<path>.sqlite`. Two artifacts ship
together; an `archive` helper that bundles them is post-v0.

## Open questions resolved for v0

| | Decision |
|---|---|
| Compaction trigger | Explicit `Store::compact()` only |
| Server auth | `none` default, `api_key` implemented, `oauth` deferred |
| `scaling_factor_multiplier` | Stored as opaque TEXT (e.g. `"x * 1.05"`); not evaluated |
| Units | `Option<String>` free-form label, no dimensional analysis |
| NetCDF chunking | `(1, num_arrays)` — per-timestep reads contiguous |

## Status

- 30 Rust tests + 10 Python tests + 11 Julia tests.
- Workspace clippy-clean on edition 2024 (Rust 1.95).
