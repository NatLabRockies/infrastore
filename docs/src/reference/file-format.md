# On-Disk File Format

A persisted store is a pair of files that must be kept together:

```text
<name>.nc          # NetCDF4 — numerical arrays
<name>.nc.sqlite   # SQLite  — metadata associations
```

The SQLite sidecar path is the NetCDF path with `.sqlite` appended to the file name. This page is
the authoritative description of both. For the rationale behind the split, see the
[Storage Model](../explanation/storage-model.md).

## Format Version

The NetCDF root carries a global attribute:

```text
data_format_version = "0.1.0"
```

This is the semver of the on-disk format (`DATA_FORMAT_VERSION`). It is bumped when the NetCDF
layout, the SQLite schema, or the [hashing domain](../explanation/content-addressing.md) changes in
a backward-incompatible way. Readers should check it before trusting a file.

## NetCDF Layout

Arrays live under a two-level group hierarchy:

```text
<name>.nc
├── attribute  data_format_version = "0.1.0"
└── group      time_series/
    └── group  single/
        ├── var  sts_{length}_{resolution_s}        f64  (length, 1000)  chunks (1, 1000)
        ├── var  sts_{length}_{resolution_s}_h      str  (1000,)         # hex hashes
        ├── var  sts_{length}_{resolution_s}__1     f64  ...             # spill dataset
        ├── var  sts_{length}_{resolution_s}__1_h   str  ...
        └── ...
```

### Dataset naming

Every `SingleTimeSeries` is stored as one column of a 2-D dataset. Series are grouped into datasets
by `(length, resolution)`:

| Element          | Meaning                                                           |
| ---------------- | ----------------------------------------------------------------- |
| `sts_`           | Prefix for `SingleTimeSeries` datasets                            |
| `{length}`       | Number of timesteps (size of dimension 0)                         |
| `{resolution_s}` | Resolution in **whole seconds** (`Duration::num_seconds`)         |
| `__{n}`          | Spill suffix; absent for the first dataset, `__1`, `__2`, … after |

Example: a series of 8,760 hourly values lands in `sts_8760_3600`.

### Dimensions and shape

Each data variable has shape `(length, MAX_COLS_PER_DATASET)` where `MAX_COLS_PER_DATASET = 1000`.
The two dimensions are named `{dataset}_t` (time, size `length`) and `{dataset}_c` (column, size
1000).

- **Rows are timesteps, columns are series.** Column `i` holds one complete series.
- **Chunking is `(1, 1000)`.** One chunk is a single timestep across all 1,000 columns, so reading
  one timestep for every series in the dataset is contiguous on disk.
- **Compression** is zlib level 3 with the shuffle filter enabled.

When all 1,000 columns of a `(length, resolution)` family are occupied, the next write creates a
spill dataset `sts_{length}_{resolution_s}__1`, then `__2`, and so on.

### Hash companion variable

For each data variable `sts_…` there is a sibling **string** variable `sts_…_h` of shape `(1000,)`.
Slot `i` holds the lowercase hex SHA-256 (64 characters) of the array in column `i`, or an **empty
string** if that column is free.

This companion variable is the on-disk index. When a store is opened, the backend scans every
`sts_…_h`, decodes the non-empty hashes, and rebuilds its in-memory `hash → (dataset, column)` map.

### Free slots, deletion, and compaction

- A **free** column is marked by an empty string in the hash variable.
- **Deleting** a series writes an empty string to its hash slot and zeroes the column's values. The
  dataset does not shrink; the slot becomes reusable.
- The **next compatible write** reuses the first free slot before allocating a new column.
- `compact()` reports the count of reusable slots but does not physically resize datasets in v0
  (netcdf-c cannot resize a dimension in place).

### v0 data constraints

The NetCDF backend stores **rank-1 `f64` arrays only**. The element dtype is fixed at `f64`. Writing
a multi-dimensional per-step array is rejected with `InvalidParameter`. Only `SingleTimeSeries` data
(the `single/` group) is produced in v0; the schema leaves room for forecast groups later.

## SQLite Schema

The sidecar database is created with `PRAGMA foreign_keys = ON` and the following DDL (idempotent —
`CREATE TABLE IF NOT EXISTS`).

### `time_series_associations`

One row per association between an owner and a stored array.

| Column              | Type    | Notes                                                        |
| ------------------- | ------- | ------------------------------------------------------------ |
| `id`                | INTEGER | Primary key                                                  |
| `owner_uuid`        | TEXT    | Owner identity (e.g. an IS.jl UUID string)                   |
| `owner_type`        | TEXT    | Owner's concrete type, descriptive                           |
| `owner_category`    | TEXT    | `CHECK` in (`Component`, `SupplementalAttribute`)            |
| `time_series_type`  | TEXT    | One of the six `TimeSeriesType` names                        |
| `name`              | TEXT    | Series name                                                  |
| `data_hash`         | BLOB    | 32-byte SHA-256 of the array; links to a NetCDF column       |
| `initial_timestamp` | TEXT    | RFC 3339 string; `NULL` for types without one                |
| `resolution_ns`     | INTEGER | Resolution in nanoseconds; `NULL` if unset                   |
| `length`            | INTEGER | Number of timesteps                                          |
| `horizon_ns`        | INTEGER | Forecast horizon (reserved)                                  |
| `interval_ns`       | INTEGER | Forecast interval (reserved)                                 |
| `count`             | INTEGER | Forecast window count (reserved)                             |
| `timestamps_json`   | TEXT    | JSON array of RFC 3339 timestamps (non-sequential, reserved) |
| `scaling_factor`    | TEXT    | Opaque scaling expression, stored verbatim, never evaluated  |
| `units`             | TEXT    | Free-form units label                                        |
| `features_hash`     | BLOB    | 32-byte SHA-256 of the feature map                           |

### `features`

The expanded feature map, one row per key. The typed columns are populated according to
`value_kind`.

| Column           | Type    | Notes                                                   |
| ---------------- | ------- | ------------------------------------------------------- |
| `association_id` | INTEGER | FK → `time_series_associations(id)` `ON DELETE CASCADE` |
| `key`            | TEXT    | Feature name                                            |
| `value_kind`     | TEXT    | `CHECK` in (`int`, `float`, `bool`, `str`)              |
| `value_int`      | INTEGER | Set when `value_kind = 'int'`                           |
| `value_float`    | REAL    | Set when `value_kind = 'float'`                         |
| `value_bool`     | INTEGER | 0/1, set when `value_kind = 'bool'`                     |
| `value_str`      | TEXT    | Set when `value_kind = 'str'`                           |
|                  |         | `PRIMARY KEY (association_id, key)`                     |

### `schema_version`

A single-column table holding the metadata schema version. v0 inserts `version = 1`.

### Indexes

```sql
CREATE UNIQUE INDEX uq_assoc ON time_series_associations
    (owner_uuid, time_series_type, name, resolution_ns, features_hash);

CREATE INDEX ix_hash       ON time_series_associations(data_hash);
CREATE INDEX ix_owner      ON time_series_associations(owner_uuid);
CREATE INDEX ix_resolution ON time_series_associations(resolution_ns);
```

The unique index `uq_assoc` enforces [key uniqueness](../explanation/data-model.md#keys): a
violation surfaces as `DuplicateTimeSeries`. `ix_hash` accelerates the reference-count check that
decides whether deleting an association also frees the underlying array.

## Field Encoding Notes

- **Timestamps** are stored as RFC 3339 strings in UTC (`DateTime::to_rfc3339`).
- **Durations** (`resolution`, `horizon`, `interval`) are stored as integer **nanoseconds**. Note
  that the NetCDF dataset name uses **seconds**, while the SQLite columns use nanoseconds.
- **Hashes** are stored as raw 32-byte `BLOB`s in SQLite, but as 64-character lowercase hex
  **strings** in the NetCDF hash variable.
- **Features** are stored both expanded (the `features` table, for querying) and digested (the
  `features_hash` column, for the uniqueness constraint). The two are always derived from the same
  map; see [Content Addressing](../explanation/content-addressing.md#the-features-hash).

## Inspecting a Store by Hand

Standard tools work on both halves:

```sh
# Arrays: structure, attributes, dataset shapes
ncdump -h system.nc
ncdump -v time_series/single/sts_24_3600_h system.nc   # see the column hashes

# Metadata: associations and features
sqlite3 system.nc.sqlite '.schema'
sqlite3 system.nc.sqlite \
  'SELECT owner_uuid, name, length, resolution_ns, hex(data_hash) FROM time_series_associations;'
```

To map an association to its values: read its `data_hash`, hex-encode it, find the column whose hash
matches in the relevant `sts_…_h` variable, and read that column of the corresponding `sts_…`
dataset.
