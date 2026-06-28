# Storage Model

A persistent store is **two files that travel together**:

```text
system.nc          # NetCDF4 — the numerical arrays
system.nc.sqlite   # SQLite  — the metadata associations
```

The catalog path is derived by appending `.sqlite` to the NetCDF file name. This page explains the
split and how the two halves stay consistent. For the exact bytes, dataset names, and table columns,
see the [On-Disk File Format reference](../reference/file-format.md).

## Why Split Arrays From Metadata

Arrays and metadata pull in opposite directions:

| Concern        | Arrays                                            | Metadata                                  |
| -------------- | ------------------------------------------------- | ----------------------------------------- |
| Size           | Large (thousands of values each)                  | Small (a row plus a few feature rows)     |
| Access pattern | Bulk read by content                              | Filtered queries by owner, name, features |
| Mutation       | Immutable; whole-array add/delete, dedup-on-write | Insert / delete with constraints          |
| Best tool      | NetCDF4 (chunked, compressed HDF5)                | SQLite (indexes, transactions)            |

Forcing both into one format would compromise one of them. Instead, each lives where it is
strongest, and the `Store` layer coordinates them.

## The Array Side: NetCDF4

Arrays live under `time_series/single/` in the NetCDF file, in one of **two storage modes**.

Stored arrays are **immutable**: a value array is added or deleted as a whole and is never edited in
place — there is no API to mutate a row, slice, or column of an array already in the store. Changing
data means writing a new array (content-addressed and deduplicated on write) and, if desired,
deleting the old one.

**Packed mode** holds `SingleTimeSeries` (and the backing array of a
`DeterministicSingleTimeSeries`). Arrays that share a `(dtype, element_shape, length, resolution)`
are packed together as columns of one dataset named `sts_{dtype}_{shape}_{length}_{res}`, with shape
`(length, cols, *element_shape)`. The column count `cols` is sized to the batch that created the
dataset (capped so one chunk stays within a byte budget); an incremental, one-at-a-time write path
uses a default width of 1,000:

```mermaid
flowchart TB
    subgraph ds["dataset&nbsp;sts_f64_s_8760_3600&nbsp;&nbsp;shape&nbsp;(8760,&nbsp;cols)"]
        direction LR
        C0["col 0<br/>series A"]
        C1["col 1<br/>series B"]
        C2["col 2<br/>(free)"]
        CN["col cols-1<br/>(free)"]
    end
    H["companion sts_f64_s_8760_3600_h<br/>cols hash strings"]
    C0 -.hash.-> H
    C1 -.hash.-> H

    style C0 fill:#28a745,color:#fff
    style C1 fill:#28a745,color:#fff
    style C2 fill:#6c757d,color:#fff
    style CN fill:#6c757d,color:#fff
    style H fill:#6f42c1,color:#fff
```

- **Columns are series, rows are timesteps.** Chunking is `(1, cols, *element_shape)`, so one HDF5
  chunk holds a single timestamp across every column. The layout favors **bulk writes** (a batch
  fills whole chunks in one pass) and **reads across series by timestamp** (one timestamp is one
  chunk). The reverse directions are the slow ones, by design: reading a single series in full
  touches every chunk band, and adding one series at a time rewrites a chunk band per timestep.
- **A companion string variable holds the hashes.** For each packed dataset there is a sibling
  `{dataset}_h`; slot `i` holds the SHA-256 hex of column `i`, or an empty string if the slot is
  free. This is the on-disk index the backend rebuilds on open.
- **Datasets spill when full** into `…__1`, `…__2`, and so on — when a batch exceeds the per-dataset
  column cap, or when incremental writes fill a default-width (1,000-column) dataset.
- **Compression is configurable** at store creation and applies to every data variable (packed and
  standalone). The default is DEFLATE (zlib) level 3 with the byte-shuffle filter; you may change
  the level (0–9), disable shuffle, or turn compression off entirely. The choice is recorded in a
  `compression` global attribute and restored when the store is reopened for appends, so later
  writes reuse the same filter. Compression is a storage detail only — arrays decode transparently
  regardless of the filter, so stores written with different settings stay mutually readable and the
  data-format version is unaffected. In-memory stores ignore the setting.

**Standalone mode** holds `NonSequentialTimeSeries` and the dense forecast arrays (`Deterministic`,
`Probabilistic`, `Scenarios`). Each is its own typed, multi-dimensional variable `arr_{hex_hash}` of
shape `[length, *element_shape]` — no column packing and no companion hash (the variable name
carries the hash).

The [file-format reference](../reference/file-format.md#netcdf-layout) gives the precise naming and
dimension scheme. Nothing on the array side distinguishes a forecast from a static series of the
same physical shape — the type, timestamps, and windowing parameters all live in metadata.

## The Metadata Side: SQLite

The catalog holds two tables:

- **`time_series_associations`** — one row per
  `(owner_id, owner_category, name, resolution, features)` association, including the `data_hash`
  that links it to a packed column or standalone variable, the array typing (`dtype`,
  `element_shape`, `logical_type`), plus temporal fields, forecast parameters (`horizon`,
  `interval`, `count`, `percentiles`), and units.
- **`features`** — the expanded key/value pairs for each association, one row per feature, typed by
  a `value_kind` discriminator.

A unique index over
`(owner_id, owner_category, time_series_type, name, resolution, interval, features_hash)` enforces
the [key uniqueness](./data-model.md#keys) invariant at the database level. `owner_category` is part
of the key, so a component and a supplemental attribute that share an `owner_id` are independent;
`interval` is part of the key too, so forecasts of one variable that differ only by interval are
distinct. Because SQLite treats `NULL` as distinct in a `UNIQUE` index, a second index folds `NULL`
`resolution` and `interval` to a sentinel so series without them (e.g. `NonSequentialTimeSeries`, or
any static series, which carry no interval) are still constrained. Indexes on `data_hash`,
`(owner_id, owner_category)`, and `resolution` keep lookups fast.

## Keeping the Two Files Consistent

Because a write touches both files, `Store` follows a careful ordering so a failure cannot leave a
dangling reference:

```mermaid
sequenceDiagram
    participant C as add_time_series
    participant B as NetCDF backend
    participant M as SQLite
    C->>B: put_array(hash, data)
    Note over C,B: idempotent on hash — staged for rollback
    C->>M: BEGIN
    C->>M: INSERT association
    alt insert succeeds
        C->>M: COMMIT
        C-->>C: return TimeSeriesKey
    else insert fails (duplicate etc.)
        C->>M: ROLLBACK
        C->>B: remove_array(staged hashes)
        C-->>C: return Err
    end
```

- **Array first, metadata second.** `put_array` is idempotent — calling it with an already-present
  hash is a no-op — so staging the array before committing metadata is safe.
- **Metadata commit is the point of no return.** If the SQLite insert fails (most commonly a
  `DuplicateTimeSeries` constraint violation), the transaction rolls back and any array column
  staged _in this call_ is removed, returning the store to its prior state.
- **Bulk writes are all-or-nothing.** `add_time_series_bulk` (and the buffered `bulk_add` session)
  group packed series by shape and stage each group as one batch-sized block — filling whole chunks
  — then insert every association in one transaction; any error rolls the whole batch back and
  removes the staged arrays.

On delete, the order reverses and is reference-counted: the association rows are removed inside a
transaction, then an array column is only zeroed/freed if no remaining association references that
hash. This is what lets two keys [share one array](./content-addressing.md) safely.

## Persistence and Copying

The NetCDF backend buffers writes. Call `flush()` (which issues `nc_sync`) before copying the files
for backup or archival; afterward both `system.nc` and `system.nc.sqlite` can be copied as a pair
without closing the handle. The two files must always be kept together — neither is usable alone.

## Compaction

Deleting a series frees its column slot but does not shrink the NetCDF dataset. The freed slot is
transparently reused by the next compatible write (`first_free`). `compact()` reports how many slots
are reclaimable; v0 does not physically shrink datasets, because netcdf-c cannot resize a dimension
in place — that is a follow-up. See [`compact`](../reference/rust-api.md#store).
