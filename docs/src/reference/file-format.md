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

The NetCDF root carries two global attributes:

```text
data_format_version = "0.10.0"
compression         = "deflate:3:shuffle"
```

`data_format_version` is the semver of the on-disk format (`DATA_FORMAT_VERSION`). It is bumped when
the NetCDF layout, the SQLite schema, or the [hashing domain](../explanation/content-addressing.md)
changes in a backward-incompatible way.

`compression` records the filter policy the store was created with, so that appends made after
reopening reuse the same filter. It is **not** part of the compatibility contract — see
[Compression](#compression) below.

Opening a store whose recorded version differs from the version this build reads fails with
`IncompatibleFormat`, naming both versions. Every bump is backward-incompatible by definition, so
the check is exact equality and there is no in-place upgrade path: regenerate the store with the
matching build.

(`0.10.0` replaced the per-association `features` table with the content-addressed `feature_sets`
table below, so a feature map is stored once and shared by every association that uses it — dropping
the `association_id` foreign key and its `ON DELETE CASCADE`; `0.9.0` changed the packed-dataset
chunking to timestamp-major `(1, cols, *element_shape)` and made the column count `cols` per-dataset
(sized to the writing batch) instead of a fixed 1,000, optimizing reads across series by timestamp
and bulk writes; `0.8.0` added the forecast `interval` to the association uniqueness key — so two
forecasts of one variable that differ only by interval are now distinct series — widening both
unique indexes (the `NULL`-folding index now `COALESCE`s `interval` as well as `resolution`);
`0.7.0` made `resolution`/`horizon`/`interval` calendar-aware
[periods](../explanation/data-model.md): they are now encoded as ISO-8601 duration strings (e.g.
`PT1H`, `P1M`, `P1Y`) rather than integer milliseconds, in both the packed dataset names and the
SQLite columns, so irregular periods (`Month`/`Quarter`/`Year`) can be represented distinctly from
fixed spans; `0.6.0` added `owner_category` to the association uniqueness key (so the owner identity
is the pair `(owner_id, owner_category)`), widening the unique indexes and `ix_owner`; `0.5.0`
changed the owner identifier to a signed 64-bit integer (`owner_id`); `0.4.0` is the baseline the
Rust port of InfrastructureSystems.jl shipped with — the version that introduced
`DATA_FORMAT_VERSION` itself; `0.3.0` switched the time unit from nanoseconds to milliseconds,
renaming the SQLite `*_ns` columns to `*_ms` and encoding the packed dataset name's `{res}` field in
milliseconds instead of whole seconds; `0.2.0` introduced typed, multi-dimensional arrays and the
two-mode NetCDF layout below; `0.1.0` stored only 1-D `f64`.)

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
├── attribute  data_format_version = "0.10.0"
├── attribute  compression         = "deflate:3:shuffle"
└── group      time_series/
    └── group  single/
        ├── var  sts_{dtype}_{shape}_{length}_{res}      packed dataset  (length, cols, *element_shape)
        ├── var  sts_{dtype}_{shape}_{length}_{res}_h    str  (cols,)     # per-column hex hashes
        ├── var  sts_{dtype}_{shape}_{length}_{res}__1   packed spill dataset
        ├── var  arr_{hex_hash}                          standalone array  [length, *element_shape]
        └── ...
```

Every dimension is private to the variable that owns it and is named after that variable, so
`ncdump -h` shows one set of dimensions per dataset:

- A packed dataset `{dataset}` is dimensioned `({dataset}_t, {dataset}_c, {dataset}_e0, …)`:
  `{dataset}_t` = `length` (the time axis), `{dataset}_c` = `cols` (the column axis), and one
  `{dataset}_e{i}` per trailing axis, sized `element_shape[i]`. A scalar-per-step dataset has no
  `_e{i}` dimensions.
- Its hash companion `{dataset}_h` is dimensioned on `{dataset}_c` alone — one hash slot per column.
- A standalone array `arr_{hex_hash}` is dimensioned `arr_{hex_hash}_d{i}`, one per axis of its full
  shape `[length, *element_shape]`.

`{dataset}_c` is load-bearing, not decorative: on open the backend recovers each packed dataset's
column count from the length of its second dimension, which is how per-dataset widths round-trip.

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

The dataset shape is `(length, cols, *element_shape)` and chunking is `(1, cols, *element_shape)`,
so one HDF5 chunk holds a single timestamp across every column — making a read across series by
timestamp one chunk, and a buffered bulk write fill whole chunks. `cols` is chosen per dataset: a
managed bulk write sizes it to the batch, while an incremental one-at-a-time write path uses a
default width (`DEFAULT_COLS_PER_DATASET = 1000`). In both cases `cols` is capped so one chunk stays
within a byte budget (`MAX_CHUNK_BYTES = 1 MiB`); a batch wider than the cap spills across datasets.

- **Rows are timesteps, columns are series.** Column `i` holds one complete series.
- **Hash companion variable.** Each packed dataset has a sibling **string** variable `{dataset}_h`
  of shape `(cols,)`. Slot `i` holds the lowercase hex SHA-256 (64 chars) of column `i`, or an empty
  string if the column is free. This is the on-disk index: on open, the backend scans every `…_h`,
  decodes the non-empty hashes, and rebuilds its `hash → (dataset, column)` map. (The backend also
  recovers each dataset's `cols` from its column dimension length, so per-dataset widths
  round-trip.)
- **Spill.** When a family's current dataset is full — a batch exceeds the column cap, or
  incremental writes fill a default-width dataset — the next write creates a spill dataset `…__1`,
  then `…__2`, and so on.

### Standalone mode

Used for **`NonSequentialTimeSeries`** and the dense forecast arrays (**`Deterministic`**,
**`Probabilistic`**, **`Scenarios`**). Each array is its own typed, multi-dimensional variable named
`arr_{hex_hash}` of shape `[length, *element_shape]` in the `time_series/single` group. There is no
column packing and no companion hash variable — the variable name carries the hash.

`NonSequentialTimeSeries` stores its explicit, strictly-increasing timestamps in the association's
`timestamps_json` metadata field, not in the array.

### Compression

Compression is chosen at store creation and applies to every data variable, packed and standalone
alike (the `…_h` hash variables are strings and are not compressed). The **default** is DEFLATE
(zlib) level 3 with the byte-shuffle filter; the level (0–9) and shuffle can be changed, or
compression turned off entirely. The choice is persisted in the `compression` global attribute and
restored when the store is reopened, so later appends reuse the same filter:

| Attribute value             | Meaning                                    |
| --------------------------- | ------------------------------------------ |
| `none`                      | No compression filter                      |
| `deflate:{level}:shuffle`   | DEFLATE at `level` (0–9), byte-shuffle on  |
| `deflate:{level}:noshuffle` | DEFLATE at `level` (0–9), byte-shuffle off |

An absent or unparseable attribute falls back to the default (`deflate:3:shuffle`), which is what
such a file was written with. Compression is a storage detail only: arrays decode transparently
regardless of the filter, so stores written with different settings stay mutually readable and
`data_format_version` is unaffected by the choice.

### Deletion and compaction

- **Packed:** deletion writes an empty string to the column's hash slot and zero-fills the column's
  data, so no stale values are readable through a reused slot. The slot becomes reusable by the next
  compatible write. The dataset does not shrink.
- **Standalone:** deletion drops the array from the in-memory index; the NetCDF variable lingers as
  dead space (NetCDF cannot delete variables in place).
- `compact()` reports reclaimable slots but does not physically resize datasets or remove dead
  standalone variables in this release (netcdf-c cannot resize a dimension in place).
- **Feature sets:** because they are shared, deleting an association never deletes its feature set;
  removing the last association that referenced one leaves it unreachable. `compact()` deletes
  unreachable sets and reports the row count as `feature_sets_reclaimed`. This is the one thing
  compaction physically removes.

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
| `data_hash`         | BLOB    | 32-byte SHA-256 of the array; links to a NetCDF column/variable |
| `features_hash`     | BLOB    | 32-byte SHA-256 of the feature map                              |

The two content-address hashes are the last two columns. Column order is not load-bearing — every
statement names its columns — so the layout is chosen for readability.

### `feature_sets`

The expanded feature map, one row per key. The typed columns are populated according to
`value_kind`.

Feature sets are **content-addressed**, exactly as arrays are: the table is keyed by the SHA-256 of
the feature map, and one set is stored once and shared by every association whose `features_hash`
matches. The association row already carries that hash, so no join column is needed. Two
associations with the same features therefore reference the same rows here — including a
`DeterministicSingleTimeSeries` and the `SingleTimeSeries` it was derived from, which is why
[`transform_single_time_series`](../explanation/data-model.md) writes no feature rows at all.

| Column          | Type    | Notes                                      |
| --------------- | ------- | ------------------------------------------ |
| `key`           | TEXT    | Feature name                               |
| `value_kind`    | TEXT    | `CHECK` in (`int`, `float`, `bool`, `str`) |
| `value_int`     | INTEGER | Set when `value_kind = 'int'`              |
| `value_float`   | REAL    | Set when `value_kind = 'float'`            |
| `value_bool`    | INTEGER | 0/1, set when `value_kind = 'bool'`        |
| `value_str`     | TEXT    | Set when `value_kind = 'str'`              |
| `features_hash` | BLOB    | 32-byte SHA-256 of the feature map         |
|                 |         | `PRIMARY KEY (features_hash, key)`         |

An empty feature map stores no rows.

There is deliberately **no foreign key** to `time_series_associations` and **no cascade**: rows here
are shared, so deleting one association must not delete a set another association still uses.
Removing the last association that referenced a set instead leaves it unreachable — the same
deletion semantics as the NetCDF side's unreachable standalone variables. `Store::compact` sweeps
unreachable sets and reports the count as `feature_sets_reclaimed`; clearing a store drops them all
outright.

### `schema_version`

```sql
CREATE TABLE schema_version (version INTEGER NOT NULL);
```

A single-column table holding the catalog schema version. Creating the catalog inserts `version = 1`
if the table is empty; `1` is the current value. This tracks the SQLite schema alone and is distinct
from the NetCDF `data_format_version` attribute, which governs the artifact as a whole and is the
value `open` validates.

### Indexes

```sql
CREATE UNIQUE INDEX uq_assoc ON time_series_associations
    (owner_id, owner_category, time_series_type, name, resolution, interval, features_hash);
CREATE UNIQUE INDEX uq_assoc_coalesced ON time_series_associations
    (owner_id, owner_category, time_series_type, name,
     COALESCE(resolution, ''), COALESCE(interval, ''), features_hash);

CREATE INDEX ix_hash       ON time_series_associations(data_hash);
CREATE INDEX ix_owner      ON time_series_associations(owner_id, owner_category);
CREATE INDEX ix_resolution ON time_series_associations(resolution);
```

Together the two unique indexes enforce [key uniqueness](../explanation/data-model.md#keys); a
violation surfaces as `DuplicateTimeSeries`. Both `owner_id` and `owner_category` are part of the
key, so a component and a supplemental attribute that share an `owner_id` are independent owners.
`interval` is part of the key, so two forecasts of one variable at the same resolution but different
intervals are distinct series. SQLite treats `NULL` values as distinct in a `UNIQUE` index, so
`uq_assoc` does not constrain rows with a `NULL` `resolution` or `interval` (e.g.
`NonSequentialTimeSeries`, or any static series, which carry no interval). `uq_assoc_coalesced`
covers that case by folding `NULL` to the empty-string sentinel via `COALESCE` before enforcing
uniqueness (the empty string is never a valid ISO-8601 period).

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
