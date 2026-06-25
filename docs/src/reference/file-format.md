# On-Disk File Format

A persisted store is a pair of files that must be kept together:

```text
<name>.nc          # NetCDF4 — numerical arrays
<name>.nc.sqlite   # SQLite  — metadata associations
```

The SQLite catalog path is the NetCDF path with `.sqlite` appended to the file name. This page is
the authoritative description of both. For the rationale behind the split, see the
[Storage Model](../explanation/storage-model.md).

## Format Version

The NetCDF root carries a global attribute:

```text
data_format_version = "0.7.0"
```

This is the semver of the on-disk format (`DATA_FORMAT_VERSION`). It is bumped when the NetCDF
layout, the SQLite schema, or the [hashing domain](../explanation/content-addressing.md) changes in
a backward-incompatible way. Readers should check it before trusting a file. (`0.7.0` made
`resolution`/`horizon`/`interval` calendar-aware [periods](../explanation/data-model.md): they are
now encoded as ISO-8601 duration strings (e.g. `PT1H`, `P1M`, `P1Y`) rather than integer
milliseconds, in both the packed dataset names and the SQLite columns, so irregular periods
(`Month`/`Quarter`/`Year`) can be represented distinctly from fixed spans; `0.6.0` added
`owner_category` to the association uniqueness key (so the owner identity is the pair
`(owner_id, owner_category)`), widening the unique indexes and `ix_owner`; `0.5.0` changed the owner
identifier to a signed 64-bit integer (`owner_id`); `0.2.0` introduced typed, multi-dimensional
arrays and the two-mode NetCDF layout below; `0.1.0` stored only 1-D `f64`.)

## Arrays Are Typed and N-Dimensional

Every stored array is a **`TypedArray`**: an element `dtype`, a `shape` `[length, k1, k2, …]` whose
first axis is time and whose trailing axes are a fixed per-step element shape, and the raw
row-major, little-endian element bytes. The supported dtypes and their stable integer codes (shared
with the bindings and the C ABI):

| Code | dtype | Width | Code | dtype  | Width |
| ---- | ----- | ----- | ---- | ------ | ----- |
| 0    | `f64` | 8     | 3    | `i32`  | 4     |
| 1    | `f32` | 4     | 4    | `u64`  | 8     |
| 2    | `i64` | 8     | 5    | `bool` | 1     |

A scalar-per-step series has an empty element shape; a per-step tuple (e.g. the 3 coefficients of a
quadratic cost curve) has element shape `[3]`.

## NetCDF Layout

Arrays live under a two-level group hierarchy, in one of **two storage modes**:

```text
<name>.nc
├── attribute  data_format_version = "0.7.0"
└── group      time_series/
    └── group  single/
        ├── var  sts_{dtype}_{shape}_{length}_{res}      packed dataset  (length, 1000, *element_shape)
        ├── var  sts_{dtype}_{shape}_{length}_{res}_h    str  (1000,)    # per-column hex hashes
        ├── var  sts_{dtype}_{shape}_{length}_{res}__1   packed spill dataset
        ├── var  arr_{hex_hash}                          standalone array  [length, *element_shape]
        └── ...
```

### Packed mode

Used for **`SingleTimeSeries`** and the underlying array of a **`DeterministicSingleTimeSeries`**.
Many arrays that share a `(dtype, element_shape, length, resolution)` are column-packed into one
dataset:

| Element    | Meaning                                                           |
| ---------- | ----------------------------------------------------------------- |
| `sts_`     | Prefix for packed `SingleTimeSeries` datasets                     |
| `{dtype}`  | Element dtype string (`f64`, `i64`, …)                            |
| `{shape}`  | Element shape: `s` = scalar, `3` = `[3]`, `3x2` = `[3, 2]`        |
| `{length}` | Number of timesteps (size of the time axis)                       |
| `{res}`    | Resolution as an ISO-8601 duration (`PT1H`, `P1M`, `P1Y`; no `_`) |
| `__{n}`    | Spill suffix; absent for the first dataset, `__1`, `__2`, … after |

The dataset shape is `(length, 1000, *element_shape)` (`MAX_COLS_PER_DATASET = 1000` columns) and
chunking is `(length, 1, *element_shape)`, so each column (one complete series) occupies exactly one
HDF5 chunk. Data variables use zlib level 3 with shuffle.

- **Rows are timesteps, columns are series.** Column `i` holds one complete series.
- **Hash companion variable.** Each packed dataset has a sibling **string** variable `{dataset}_h`
  of shape `(1000,)`. Slot `i` holds the lowercase hex SHA-256 (64 chars) of column `i`, or an empty
  string if the column is free. This is the on-disk index: on open, the backend scans every `…_h`,
  decodes the non-empty hashes, and rebuilds its `hash → (dataset, column)` map.
- **Spill.** When all 1,000 columns of a family are occupied, the next write creates a spill dataset
  `…__1`, then `…__2`, and so on.

### Standalone mode

Used for **`NonSequentialTimeSeries`** and the dense forecast arrays (**`Deterministic`**,
**`Probabilistic`**, **`Scenarios`**). Each array is its own typed, multi-dimensional variable named
`arr_{hex_hash}` of shape `[length, *element_shape]` in the `time_series/single` group. There is no
column packing and no companion hash variable — the variable name carries the hash.

`NonSequentialTimeSeries` stores its explicit, strictly-increasing timestamps in the association's
`timestamps_json` metadata field, not in the array.

### Deletion and compaction

- **Packed:** deletion writes an empty string to the column's hash slot; the slot becomes reusable
  by the next compatible write. The dataset does not shrink.
- **Standalone:** deletion drops the array from the in-memory index; the NetCDF variable lingers as
  dead space (NetCDF cannot delete variables in place).
- `compact()` reports reclaimable slots but does not physically resize datasets or remove dead
  standalone variables in this release (netcdf-c cannot resize a dimension in place).

## SQLite Schema

The catalog database is created with `PRAGMA foreign_keys = ON` and the following DDL (idempotent —
`CREATE TABLE IF NOT EXISTS`).

### `time_series_associations`

One row per association between an owner and a stored array.

| Column              | Type    | Notes                                                           |
| ------------------- | ------- | --------------------------------------------------------------- |
| `id`                | INTEGER | Primary key                                                     |
| `owner_id`          | INTEGER | Owner identity; signed 64-bit integer identifier (part of key)  |
| `owner_type`        | TEXT    | Owner's concrete type, descriptive                              |
| `owner_category`    | TEXT    | `CHECK` in (`Component`, `SupplementalAttribute`); part of key  |
| `time_series_type`  | TEXT    | One of the six `TimeSeriesType` names                           |
| `name`              | TEXT    | Series name                                                     |
| `data_hash`         | BLOB    | 32-byte SHA-256 of the array; links to a NetCDF column/variable |
| `initial_timestamp` | TEXT    | RFC 3339 string; `NULL` for `NonSequentialTimeSeries`           |
| `resolution`        | TEXT    | ISO-8601 duration (`PT1H`, `P1M`, …); `NULL` for non-sequential |
| `length`            | INTEGER | Number of timesteps                                             |
| `horizon`           | TEXT    | ISO-8601 forecast horizon; `NULL` for non-forecasts             |
| `interval`          | TEXT    | ISO-8601 forecast interval; `NULL` for non-forecasts            |
| `count`             | INTEGER | Forecast window count; `NULL` for non-forecasts                 |
| `timestamps_json`   | TEXT    | JSON array of RFC 3339 timestamps (`NonSequentialTimeSeries`)   |
| `units`             | TEXT    | Free-form units label                                           |
| `percentiles_json`  | TEXT    | JSON array of percentiles for `Probabilistic`; `NULL` else      |
| `dtype`             | TEXT    | Element dtype string (`NOT NULL DEFAULT 'f64'`)                 |
| `element_shape`     | TEXT    | JSON array of per-step dims (`[]` = scalar)                     |
| `logical_type`      | TEXT    | Opaque binding-owned domain label; `NULL` if unset              |
| `features_hash`     | BLOB    | 32-byte SHA-256 of the feature map                              |

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

A single-column table holding the metadata schema version.

### Indexes

```sql
CREATE UNIQUE INDEX uq_assoc ON time_series_associations
    (owner_id, owner_category, time_series_type, name, resolution, features_hash);
CREATE UNIQUE INDEX uq_assoc_null_resolution ON time_series_associations
    (owner_id, owner_category, time_series_type, name,
     COALESCE(resolution, ''), features_hash);

CREATE INDEX ix_hash       ON time_series_associations(data_hash);
CREATE INDEX ix_owner      ON time_series_associations(owner_id, owner_category);
CREATE INDEX ix_resolution ON time_series_associations(resolution);
```

Together the two unique indexes enforce [key uniqueness](../explanation/data-model.md#keys); a
violation surfaces as `DuplicateTimeSeries`. Both `owner_id` and `owner_category` are part of the
key, so a component and a supplemental attribute that share an `owner_id` are independent owners.
SQLite treats `NULL` values as distinct in a `UNIQUE` index, so `uq_assoc` does not constrain rows
with a `NULL` `resolution` (e.g. `NonSequentialTimeSeries`). `uq_assoc_null_resolution` covers that
case by folding `NULL` to the empty-string sentinel via `COALESCE` before enforcing uniqueness (the
empty string is never a valid ISO-8601 period).

## Field Encoding Notes

- **Timestamps** are RFC 3339 strings in UTC.
- **Periods** (`resolution`, `horizon`, `interval`) are canonical **ISO-8601 duration strings** in
  SQLite (`PT1H`, `P1M`, `P1Y`), and the packed dataset name's `{res}` field uses the same encoding.
  Calendar periods (`Month`/`Quarter`/`Year`) are stored distinctly from fixed spans.
- **Hashes** are raw 32-byte `BLOB`s in SQLite, lowercase hex in NetCDF (the `…_h` variable for
  packed arrays, the `arr_` variable name for standalone arrays).
- **`element_shape`** is the per-step shape only (the trailing axes); the time `length` is a
  separate column.

## Inspecting a Store by Hand

```sh
ncdump -h system.nc                      # groups, variables, dtypes, shapes
sqlite3 system.nc.sqlite '.schema'
sqlite3 system.nc.sqlite \
  'SELECT name, time_series_type, dtype, element_shape, length FROM time_series_associations;'
```

To map an association to its values: read its `data_hash`. For a packed array, hex-encode it and
find the matching column in the relevant `sts_…_h` variable; for a standalone array, read the
variable named `arr_<hex_hash>` directly.
