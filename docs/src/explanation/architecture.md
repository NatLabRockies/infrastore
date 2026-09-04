# Architecture

infrastore is a Rust workspace with one core library and a ring of interface crates around it. Every
interface — native Rust, Python, Julia, the `infrastore` CLI, and the gRPC server — ultimately
drives the same `Store` type in `infrastore-core`, and every interface reads and writes the same
on-disk format.

## Crate Layout

```mermaid
flowchart TB
    subgraph ifaces["Interface crates"]
        PY["infrastore-py<br/>PyO3 wheel"]
        FFI["infrastore-ffi<br/>C ABI cdylib"]
        SRV["infrastore-server<br/>gRPC server + Rust client"]
        CLI["infrastore-cli<br/>infrastore binary"]
    end

    PYMOD["infrastore<br/>(Python module)"]
    JL["InfraStore.jl<br/>(Julia package)"]
    PROTO["infrastore-proto<br/>protobuf + tonic"]

    subgraph core["infrastore-core"]
        STORE["Store"]
        META["MetadataStore<br/>(SQLite)"]
        BACK["StorageBackend<br/>(trait)"]
        NC["Hdf5Backend"]
        MEM["MemoryBackend"]
        STORE --> META
        STORE --> BACK
        BACK --> NC
        BACK --> MEM
    end

    PY --> STORE
    FFI --> STORE
    SRV --> STORE
    CLI --> STORE
    PYMOD -->|"import"| PY
    JL -->|"ccall"| FFI
    SRV --> PROTO

    style STORE fill:#28a745,color:#fff
    style META fill:#28a745,color:#fff
    style BACK fill:#1e7e34,color:#fff
    style NC fill:#1e7e34,color:#fff
    style MEM fill:#1e7e34,color:#fff
    style PY fill:#17a2b8,color:#fff
    style PYMOD fill:#17a2b8,color:#fff
    style FFI fill:#9558b2,color:#fff
    style JL fill:#9558b2,color:#fff
    style SRV fill:#ffc107,color:#000
    style PROTO fill:#ffc107,color:#000
    style CLI fill:#fd7e14,color:#fff
```

| Crate / package     | Role                                                                           |
| ------------------- | ------------------------------------------------------------------------------ |
| `infrastore-core`   | The whole engine: types, storage backends, hashing, the `Store` API            |
| `infrastore-proto`  | The `.proto` service compiled with `tonic`; shared message types               |
| `infrastore-server` | A `tonic` gRPC server wrapping a `Store`, plus an async `RemoteClient`         |
| `infrastore-py`     | PyO3 classes exposing `Store` as the `infrastore` module                       |
| `infrastore`        | The importable Python module — user-facing surface of the PyO3 wheel           |
| `infrastore-ffi`    | A `extern "C"` cdylib with an opaque-handle API over `Store`                   |
| `InfraStore.jl`     | A Julia package that `ccall`s into the FFI cdylib                              |
| `infrastore-cli`    | The `infrastore` binary: read+write access to an on-disk store from a terminal |
| `infrastore-bench`  | The `infrastore-bench` binary: ingestion and simulation-read benchmarks        |

## The Core: `Store`

`Store` is a thin orchestration layer that composes two collaborators:

- A **`StorageBackend`** that holds the numerical arrays, addressed by content hash.
- A **`MetadataStore`** (SQLite) that holds the associations between owners and arrays.

```mermaid
flowchart LR
    CALL["add_time_series(...)"] --> HASH["array_hash()"]
    HASH --> PUT["backend.put_array(hash, data)"]
    HASH --> INS["MetadataStore::insert(association)"]
    PUT --> NC[("HDF5")]
    INS --> SQL[("SQLite")]

    style CALL fill:#4a9eff,color:#fff
    style HASH fill:#6f42c1,color:#fff
    style PUT fill:#28a745,color:#fff
    style INS fill:#28a745,color:#fff
    style NC fill:#1e7e34,color:#fff
    style SQL fill:#1e7e34,color:#fff
```

The backend is chosen behind the [`StorageBackend`](../reference/rust-api.md#storagebackend-trait)
trait. There are two implementations:

- **`MemoryBackend`** — arrays in a hash map; selected when `in_memory = true`. No filesystem I/O.
- **`Hdf5Backend`** — arrays in an HDF5 file; selected when a path is given.

Because the seam is a trait, the metadata layer, the hashing, and every binding are identical no
matter where the arrays live. Tests run against the memory backend; production uses HDF5.

## Why Two Files

Numerical arrays and their descriptive metadata have different access patterns. Arrays are large,
append-mostly, and read by content; metadata is small, frequently queried, and benefits from indexes
and transactions. infrastore puts each where it is strongest:

- **Arrays → HDF5.** Chunked, compressed, columnar storage that the whole HDF5 ecosystem reads.
- **Metadata → SQLite.** A queryable, transactional catalog at `<path>.h5.sqlite`.

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

Within a process, `Hdf5Backend` guards its HDF5 handle with a `Mutex`, so the storage backend itself
is `Send + Sync`. The `Store` as a whole, however, is **`Send` but not `Sync`**: its `MetadataStore`
wraps a single `rusqlite::Connection`, which is internally a `RefCell` and therefore cannot be
shared between threads. In practice this means a `Store` can be **moved** to another thread, but
sharing one across threads requires external synchronization — the gRPC server holds its store as an
`Arc<Mutex<Store>>`, and the PyO3 binding marks the class `unsendable` so a Python `Store` stays on
the thread that created it.

`MetadataStore` uses transactions for atomic multi-row writes. The library does not coordinate
multiple processes writing the same file concurrently — a single writer owns the files at a time.

Within one process the rule is enforced: opening an artifact that another `Store` in the process
already holds — read-only or not — fails with `StoreInUse`, and so do `create_replacing`,
`open_without_catalog`, `persist_to`, and `persist_arrays_to` aimed at a held path. Each handle
indexes the HDF5 file's packed columns once at open, and libhdf5 shares one file object between two
opens of a file, so a second handle would read and write the wrong columns. Drop the handle before
opening another.
