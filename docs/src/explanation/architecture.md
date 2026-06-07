# Architecture

time-series-store is a Rust workspace with one core library and a ring of interface crates around
it. Every interface — native Rust, Python, Julia, and the gRPC server — ultimately drives the same
`Store` type in `time-series-store-core`, and every interface reads and writes the same on-disk
format.

## Crate Layout

```mermaid
flowchart TB
    subgraph ifaces["Interface crates"]
        PY["time-series-store-py<br/>PyO3 wheel"]
        FFI["time-series-store-ffi<br/>C ABI cdylib"]
        SRV["time-series-store-server<br/>gRPC server + Rust client"]
    end

    JL["TimeSeries.jl<br/>(Julia package)"]
    PROTO["time-series-store-proto<br/>protobuf + tonic"]

    subgraph core["time-series-store-core"]
        STORE["Store"]
        META["MetadataStore<br/>(SQLite)"]
        BACK["StorageBackend<br/>(trait)"]
        NC["NetCdfBackend"]
        MEM["MemoryBackend"]
        STORE --> META
        STORE --> BACK
        BACK --> NC
        BACK --> MEM
    end

    PY --> STORE
    FFI --> STORE
    SRV --> STORE
    JL -->|"ccall"| FFI
    SRV --> PROTO

    style STORE fill:#28a745,color:#fff
    style META fill:#28a745,color:#fff
    style BACK fill:#1e7e34,color:#fff
    style NC fill:#1e7e34,color:#fff
    style MEM fill:#1e7e34,color:#fff
    style PY fill:#17a2b8,color:#fff
    style FFI fill:#9558b2,color:#fff
    style JL fill:#9558b2,color:#fff
    style SRV fill:#ffc107,color:#000
    style PROTO fill:#ffc107,color:#000
```

| Crate / package            | Role                                                                   |
| -------------------------- | ---------------------------------------------------------------------- |
| `time-series-store-core`   | The whole engine: types, storage backends, hashing, the `Store` API    |
| `time-series-store-proto`  | The `.proto` service compiled with `tonic`; shared message types       |
| `time-series-store-server` | A `tonic` gRPC server wrapping a `Store`, plus an async `RemoteClient` |
| `time-series-store-py`     | PyO3 classes exposing `Store` to Python as the `time_series` module    |
| `time-series-store-ffi`    | A `extern "C"` cdylib with an opaque-handle API over `Store`           |
| `TimeSeries.jl`            | A Julia package that `ccall`s into the FFI cdylib                      |

## The Core: `Store`

`Store` is a thin orchestration layer that composes two collaborators:

- A **`StorageBackend`** that holds the numerical arrays, addressed by content hash.
- A **`MetadataStore`** (SQLite) that holds the associations between owners and arrays.

```mermaid
flowchart LR
    CALL["add_time_series(...)"] --> HASH["array_hash()"]
    HASH --> PUT["backend.put_array(hash, data)"]
    HASH --> INS["MetadataStore::insert(association)"]
    PUT --> NC[("NetCDF4")]
    INS --> SQL[("SQLite")]

    style CALL fill:#4a9eff,color:#fff
    style HASH fill:#6f42c1,color:#fff
    style PUT fill:#28a745,color:#fff
    style INS fill:#28a745,color:#fff
    style NC fill:#1e7e34,color:#fff
    style SQL fill:#1e7e34,color:#fff
```

The backend is chosen behind the [`StorageBackend`](../reference/rust-api.md#storagebackend-trait)
trait. v0 ships two implementations:

- **`MemoryBackend`** — arrays in a hash map; selected when `in_memory = true`. No filesystem I/O.
- **`NetCdfBackend`** — arrays in a NetCDF4 file; selected when a path is given.

Because the seam is a trait, the metadata layer, the hashing, and every binding are identical no
matter where the arrays live. Tests run against the memory backend; production uses NetCDF.

## Why Two Files

Numerical arrays and their descriptive metadata have different access patterns. Arrays are large,
append-mostly, and read by content; metadata is small, frequently queried, and benefits from indexes
and transactions. time-series-store puts each where it is strongest:

- **Arrays → NetCDF4.** Chunked, compressed, columnar storage that HDF5 tooling already understands.
- **Metadata → SQLite.** A queryable, transactional sidecar at `<path>.nc.sqlite`.

The [Storage Model](./storage-model.md) page covers the trade-offs and the consistency protocol that
keeps the two files in agreement.

## Read Paths: Local and Remote

Writes always require local filesystem access — they go straight through a `Store`. Reads can happen
two ways:

```mermaid
flowchart LR
    subgraph local["Local process"]
        APP["Your code"] --> STORE["Store"]
    end
    subgraph network["Over the network"]
        APP2["Reader"] --> RC["RemoteClient"]
        RC -->|"gRPC"| GS["gRPC server"]
        GS --> STORE2["Store (read-only)"]
    end

    style STORE fill:#28a745,color:#fff
    style STORE2 fill:#28a745,color:#fff
    style GS fill:#ffc107,color:#000
    style RC fill:#ffc107,color:#000
```

The gRPC server exposes a **read-only** subset of the API (list, get, keys, resolutions, counts,
existence checks, integrity). It never writes. See [Language Bindings](./bindings.md) and the
[gRPC Server guide](../guides/server.md).

## Concurrency

Within a process, `NetCdfBackend` guards its NetCDF handle with a `Mutex`, so the backend is
`Send +
Sync` and a `Store` can be shared across threads. The metadata `MetadataStore` wraps a single
SQLite connection and uses transactions for atomic multi-row writes. The library does not coordinate
multiple processes writing the same file concurrently — a single writer owns the files at a time.
