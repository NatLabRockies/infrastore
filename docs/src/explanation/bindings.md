# Language Bindings

Every interface wraps the same `Store`. Understanding how each binding bridges to the core explains
why the APIs look the way they do, how errors propagate, and what each layer owns.

```mermaid
flowchart TB
    PYAPP["Python code"] --> PYO3["PyO3 classes<br/>(infrastore_py)"]
    JLAPP["Julia code"] --> JLPKG["InfraStore.jl"]
    JLPKG -->|"ccall"| CABI["C ABI<br/>(infrastore_ffi)"]
    RUSTAPP["Rust client code"] --> RC["RemoteClient"]
    RC -->|"gRPC / HTTP2"| GS["gRPC server"]

    PYO3 --> STORE["Store"]
    CABI --> STORE
    GS --> STORE

    style STORE fill:#28a745,color:#fff
    style PYO3 fill:#17a2b8,color:#fff
    style CABI fill:#9558b2,color:#fff
    style JLPKG fill:#9558b2,color:#fff
    style GS fill:#ffc107,color:#000
    style RC fill:#ffc107,color:#000
```

## Python (PyO3)

`infrastore-py` uses [PyO3](https://pyo3.rs) to expose `Store` as native Python classes in a module
importable as `infrastore`. The binding:

- Converts Python `datetime`/`timedelta` to `chrono` types and NumPy arrays (any shape) to
  `TypedArray`s at the boundary, supporting the full dtype set (`f64`, `f32`, the integer widths,
  `bool`).
- Translates the typed `TimeSeriesError` variants into a Python exception hierarchy rooted at
  `TimeSeriesError` (`NotFoundError`, `DuplicateTimeSeriesError`, `InvalidParameterError`,
  `IntegrityError`, `ReadOnlyStoreError`).
- Builds an `abi3-py311` wheel, so one wheel works across CPython 3.11+ without recompiling.

The metadata side is owned entirely by Rust; Python never touches SQLite directly. See the
[Python guide](../guides/python.md) and [Python API reference](../reference/python-api.md).

## Julia (C ABI)

Julia does not call Rust directly. Instead, `infrastore-ffi` compiles a C-compatible cdylib with an
opaque-handle API, and `InfraStore.jl` `ccall`s into it.

```mermaid
flowchart LR
    JL["InfraStore.jl<br/>structs hold Ptr{Cvoid}"] -->|"ccall infrastore_store_*"| LIB["libinfrastore_ffi"]
    LIB --> STORE["Store"]
    LIB -.->|"infrastore_last_error_message"| JL

    style JL fill:#9558b2,color:#fff
    style LIB fill:#6f42c1,color:#fff
    style STORE fill:#28a745,color:#fff
```

The conventions that shape the Julia API:

- **Opaque handles.** `InfraStore` and `InfraStoreKey` are pointers; the Julia structs wrap them and
  register finalizers (`close!`, `_finalize_key`) that call the matching `ts_*_free` function.
- **Status codes plus thread-local error messages.** Every C function returns an `int32_t` code. On
  a non-zero code, Julia calls `infrastore_last_error_message` to retrieve the detail string and
  raises the matching Julia exception type.
- **Out-parameters and caller-owned buffers.** Arrays come back through an out-pointer plus a length
  and a dtype code; Julia copies them into a `Vector{T}` for the requested element type and frees
  the Rust buffer with the deallocator matching the buffer's element type —
  `infrastore_buffer_free_f64`, `infrastore_buffer_free_u8`, `infrastore_buffer_free_i64`, or
  `infrastore_buffer_free_u64` (shape/dims buffers).
- **Features cross as JSON.** Julia serializes the feature dict to a JSON string, which the FFI
  layer parses into a `Features` map.
- **Forecasts are wrapped.** `InfraStore.jl` exposes `Deterministic` / `Probabilistic` / `Scenarios`
  structs passed to the generic `add_time_series!`, id-addressed `read_by_id` getters, and
  `transform_single_time_series!`, so all four forecast types are usable from Julia.
- **Bulk reads use a result handle.** `read_by_ids` reads many full `SingleTimeSeries` at once: the
  FFI fetches them in one decompress-once pass per dataset into a `InfraStoreBulkReadHandle`
  (`infrastore_store_bulk_read_single`), and Julia reads each element out, then frees the handle.
  Python's `store.read_by_ids` exposes the same operation directly. `read_by_ids` addresses the same
  read by catalog association id and fills the same handle, so both reads decode by the same route:
  `infrastore_bulk_result_item_name` hands each item's name back beside its values, as
  `infrastore_bulk_result_item_type` does its type. Managed bulk _writes_ already take the fast
  block-write path through the existing batch / `add_time_series_bulk` APIs.

`InfraStore.jl` loads the cdylib from the `INFRASTORE_LIB` environment variable when it is set, and
otherwise from the `libinfrastore_ffi` artifact its `Artifacts.toml` pins to the matching GitHub
Release (see [Integrate with Julia](../guides/julia.md#install)). See the
[Julia guide](../guides/julia.md), the [C ABI reference](../reference/c-abi.md), and the
[Julia API reference](../reference/julia-api.md).

### InfrastructureSystems.jl Integration

The model was shaped to drop into InfrastructureSystems.jl: owners are identified by integer
component identifiers (`i64`), owner categories map to `Component` / `SupplementalAttribute`, and
features accept string values so InfrastructureSystems.jl's feature dictionaries round-trip
unchanged. The FFI exposes attribute-based accessors (`infrastore_store_has_by_attrs`,
`infrastore_store_remove_by_ids`), a whole-record metadata read
(`infrastore_store_get_metadata_by_key`, reachable from attributes through
`infrastore_store_list_metadata`), and a hash-based array fetch
(`infrastore_store_get_array_by_hash`) so an InfrastructureSystems.jl-side store can keep its own
key objects and reach the array layer directly.

## gRPC Server and Client

`infrastore-server` wraps a `Store` in a `tonic` gRPC service generated from `infrastore-proto`. It
exposes a **read-only** slice of the API and adds optional API-key auth. The matching async
`RemoteClient` mirrors the read methods and maps gRPC `Status` codes back to
`TimeSeriesError::ConnectionError`, so remote calls surface the same error type as local ones.

Writes are deliberately not exposed over gRPC — they require local filesystem access. The server is
for fan-out reads of an existing store. See the [gRPC Server guide](../guides/server.md) and the
[gRPC API reference](../reference/grpc-api.md).

## CLI (`infrastore`)

`infrastore-cli` builds the `infrastore` binary, a thin wrapper over the core `Store` for use from a
terminal. Unlike the gRPC server it is **not** read-only: it opens the on-disk `.h5` + `.h5.sqlite`
pair directly and supports both reads and writes. Its shape:

- **CSV in, store out.** Numeric values come from a CSV; the metadata that does not fit a flat grid
  (owner, name, type, dtype, resolution, timestamps, units, features) is described in a descriptor
  JSON. All six dtypes and all six writable types are supported, forecasts included.
- **A global `-f/--format` selects `table` (default), `json`, `jsonl`, or `csv`.** Read commands
  render their results in it; write commands report their outcome in it (prose under `table`, a
  one-object status document under `json`/`jsonl`). Only `template` ignores it.
- **Store access is isolated.** All store opening lives behind one module, so a future remote/gRPC
  mode can be added without touching the command handlers; today there is no remote mode.

See the [CLI guide](../guides/cli.md) and the [CLI reference](../reference/cli.md).

## What Every Binding Shares

| Concern            | Single source of truth                                           |
| ------------------ | ---------------------------------------------------------------- |
| Types & validation | `infrastore-core` (`Store`, `TimeSeriesId`, `Features`)          |
| On-disk format     | `Hdf5Backend` + `MetadataStore` — identical regardless of caller |
| Hashing            | `array_hash` / `features_hash` — the cross-language contract     |
| Error taxonomy     | `TimeSeriesError`, re-projected into each language's idiom       |

A file written by Python reads identically from Julia, Rust, or the server, because none of the
bindings reimplement storage — they all funnel through the one core.

## Feature Coverage Varies by Binding

The bindings funnel through one core, and the surface is now broadly consistent. Both static series
types are available everywhere (read+write, except the read-only gRPC server), and
[forecasts](./time-series-types.md#forecasts) read back across every interface. The remaining
asymmetry is that the read-only gRPC server does not accept any writes:

| Capability                    | Rust core | C ABI | Python | Julia | CLI    | gRPC        |
| ----------------------------- | --------- | ----- | ------ | ----- | ------ | ----------- |
| `SingleTimeSeries` r/w        | ✅        | ✅    | ✅     | ✅    | ✅     | read-only   |
| `NonSequentialTimeSeries` r/w | ✅        | ✅    | ✅     | ✅    | ✅     | read-only   |
| `PersistentTimeSeries` r/w    | ✅        | ✅    | ✅     | ✅    | ✅     | read-only   |
| dtypes beyond `f64`           | ✅        | ✅    | ✅     | ✅    | ✅     | read-only   |
| Create forecasts              | ✅        | ✅    | ✅     | ✅    | ✅     | ❌          |
| Read forecast values          | ✅        | ✅    | ✅     | ✅    | ✅     | ✅          |
| Forecast metadata / counts    | ✅        | ✅    | ✅     | ✅    | ✅     | list/counts |
| Readers (columnar sweep)      | ✅        | ✅    | ✅     | ✅    | `grid` | ❌          |
| Association catalogs          | ✅        | ✅    | ✅     | ✅    | ✅     | ❌          |

The only gap is by design: writes (including forecasts added through `add_time_series`) require
local filesystem access, so the read-only gRPC server serves forecast reads but not writes.
