# Parquet Layout

What `infrastore export -f parquet` writes and `infrastore add --parquet` reads: **partitioned long
tables**. Many series per file, one row per value, and every catalog column a table column — so a
reader opens the directory and has the whole row without attaching the SQLite catalog.

Not one file per series. A store with thousands of series would become thousands of files, which
defeats every reader worth exporting for.

> This is a **file interchange** format, not the store's own. The on-disk store is
> [HDF5 plus SQLite](file-format.md), and nothing here is load-bearing for it. Python's `to_arrow()`
> and `from_arrow()` are a different thing again: per-series, in-memory conveniences. The
> relationship is one sentence — a long table's columns are `to_arrow()`'s schema-metadata keys
> turned into columns — and neither side depends on the other.

## Partitioning

Three things cannot vary within one Parquet table without nullable or ill-typed columns: the **set
of key columns** (a forecast has an `issue_time`, a static series does not), the **Arrow type of
`value`**, and the **zone of `timestamp`**. So an export partitions its selection by the triple

```text
(time_series_type, value type, time_reference)
```

and writes one file per distinct triple. The payoff is that **every column in every file is
required**: there are no nullable columns anywhere in this format, and a reader never has to ask
whether a null means "absent" or "unknown".

The **value type** is the element type with its per-step shape, except that the four composite kinds
(`linear_function`, `quadratic_function`, `piecewise_linear`, `piecewise_step`) partition by **kind
alone**. Their stored width varies per series — a `piecewise_linear` row is `[n, x₁, y₁, …]`
zero-padded to the widest timestep in that series — so keying by width would scatter one kind across
a file per width. Instead every composite row in a file is re-padded to the widest series in it,
which the layout allows: the leading count `n` keeps each row self-describing whatever the padding.
The cost is a `data_hash` caveat, below.

An **unspecified** `time_reference` is a partition of its own. A series that declared no spelling
must not pool with one that declared UTC, even though both write a UTC-zoned column.

### File names

`<type>.<value-slug>.<reference-slug>.parquet`:

```text
SingleTimeSeries.f64.utc.parquet
SingleTimeSeries.f64_2x3.America_Denver.parquet
Deterministic.tuple3_f64.offset_minus07_00.parquet
NonSequentialTimeSeries.piecewise_linear.zoneless.parquet
PersistentTimeSeries.f64.unspecified.parquet
```

Slugs avoid every character Windows forbids (`<>:"/\|?*`), which matters because a zone name carries
`/` and a fixed offset carries `:`; a leading `-` is avoided too, since it reads as a flag wherever
the name is passed to a command. Trailing dots go (Windows strips them, so `a.` and `a` would be one
file there), and a fragment that would be a device name (`CON`, `NUL`, `LPT9`, …) gets a prefix.

**The name is a convenience, not the truth.** The slug function is one-way — the zones `a/b` and
`a_b` both flatten to `a_b` — so nothing parses a partition back out of a filename, and two
partitions that would collide get a numeric suffix in the keys' own sort order, so a re-run of the
same export produces the same names. The footer carries the exact key.

## Columns

| Column                                                                         | Type                  | Files                         | Notes                                                                        |
| ------------------------------------------------------------------------------ | --------------------- | ----------------------------- | ---------------------------------------------------------------------------- |
| `timestamp`                                                                    | `timestamp(ms, zone)` | all                           | The target time for a forecast, the breakpoint for a `PersistentTimeSeries`. |
| `issue_time`                                                                   | `timestamp(ms, zone)` | forecasts                     | Which window the row belongs to. Same zone as `timestamp`.                   |
| `percentile`                                                                   | `float64`             | `Probabilistic`               | One row per (issue, target, percentile).                                     |
| `scenario`                                                                     | `int64`               | `Scenarios`                   | Zero-based trajectory index.                                                 |
| `value`                                                                        | see below             | all                           |                                                                              |
| `id`                                                                           | `int64`               | all                           | Provenance only; ignored on import.                                          |
| `data_hash`                                                                    | `utf8` (hex)          | all                           | A checksum on import; see below.                                             |
| `owner_id`                                                                     | `int64`               | all                           |                                                                              |
| `owner_type`                                                                   | `utf8`                | all                           |                                                                              |
| `owner_category`                                                               | `utf8`                | all                           | `Component` or `SupplementalAttribute`.                                      |
| `time_series_type`                                                             | `utf8`                | all                           | Constant per file.                                                           |
| `name`                                                                         | `utf8`                | all                           |                                                                              |
| `resolution`                                                                   | `utf8` (ISO-8601)     | `SingleTimeSeries`, forecasts | Absent from the two irregular types' files, which have no constant step.     |
| `interval`, `horizon`                                                          | `utf8` (ISO-8601)     | forecasts                     |                                                                              |
| `features`                                                                     | `utf8` (JSON object)  | all                           | `{}` when empty. Plain scalars: `{"model_year":2030}`.                       |
| `element_type`                                                                 | `utf8`                | all                           | Constant per file except a composite's width.                                |
| `time_reference`                                                               | `utf8`                | all                           | Constant per file; the literal `unspecified` when the series records none.   |
| `units`, `quantity_kind`, `unit_system`, `component_field`, `application_data` | `utf8`                | all                           | **Empty string when absent**, which is what keeps every column required.     |

Not written, because the rows imply them: `initial_timestamp`, `length`, `count`, `percentiles`, the
timestamp vector. Every string column is dictionary-encoded, so a per-series constant costs one
dictionary entry per row group; compression is zstd.

### The `value` column

Its Arrow type follows the element type alone. Nesting is innermost-first, which is the order the
flat row-major buffer is already in.

| `element_type` and shape            | Arrow type                                                               |
| ----------------------------------- | ------------------------------------------------------------------------ |
| scalar dtype, shape `[]`            | primitive (`double`, `int32`, `bool`, …)                                 |
| `tuple(N,T)`                        | `FixedSizeList<T>[N]`                                                    |
| scalar dtype with dense shape `[N]` | `FixedSizeList<T>[N]`                                                    |
| scalar dtype with shape `[M,N]`     | `FixedSizeList<FixedSizeList<T>[N]>[M]`                                  |
| composite kinds                     | `FixedSizeList<double>[w]`, stored packing, `w` the file's widest series |

Composites keep their **stored packing** rather than being decoded into `Struct`/`List`. That
packing is the documented cross-language wire form, held to `conformance/element_type_vectors.json`,
and every binding has a decoder for it; a `--decode` option is a possible follow-up. Casting to a
common dtype is ruled out — the dtype round trip is a project promise.

### Row order and row groups

Each series' rows are **contiguous** and sorted by the key columns: `issue_time`, then `timestamp`,
then `percentile` or `scenario`. A forecast therefore comes out window-major, so
`GROUP BY
issue_time` scans contiguously and one instant's percentiles sit together.

Row groups target roughly one million rows and are cut at a series boundary whenever a group is
within reach of the target, so row-group statistics on `id`, `owner_id`, and `name` let a reader
skip whole groups. A series larger than the target spans several.

## Footer

| Key                         | Value                                                                                                 |
| --------------------------- | ----------------------------------------------------------------------------------------------------- |
| `infrastore.format`         | `long_table_v1`. Read first, so a later format is refused by version rather than by a missing column. |
| `time_series_type`          | The partition's type.                                                                                 |
| `element_type`              | The partition's element type.                                                                         |
| `element_shape`             | JSON list; a composite's is the width the file settled on.                                            |
| `time_reference`            | The partition's spelling, `unspecified` included.                                                     |
| `rows_contiguous_by_series` | `true`.                                                                                               |

The three partition keys are here **exactly**, because the filename is one-way.

## Reading a file back

`add --parquet` takes a file or a directory (every `.parquet` in it, sorted), and commits **one
transaction per file** — so a partition that fails leaves the ones already committed alone.

Rows are streamed a row group at a time and grouped by the `KeyIdentity` columns — `owner_id`,
`owner_category`, `time_series_type`, `name`, `resolution`, `interval`, `features` — because `add`
never accepts an id. `owner_type` and every descriptor must be constant within a group; a
contradiction is an error naming the series.

**A series must be contiguous.** A key that reappears after another series' rows is refused, with a
message saying to sort by the identity columns and then by time. Stitching it back together would
mean holding the whole file, which is what a long table exists to avoid. A file you have re-sorted
in a query engine must meet the same rule.

Refused rather than coerced:

- **Nulls**, in any column. The store holds none, and NaN is a value rather than an absence.
- **Timestamps finer than a millisecond.** Seconds and milliseconds cross as they are; microseconds
  and nanoseconds only when every value is a whole millisecond, the rule the store's write path
  enforces on every instant it records.
- **Rows that leave a declared grid.** A `SingleTimeSeries` whose `resolution` says `PT1H` must walk
  one — checked against the grid that resolution _generates_, not against successive differences,
  since `P1M` clamps to month end.
- **`Struct` and `List` value columns.** Those are the decoded form this version does not write.
- **A `DeterministicSingleTimeSeries` partition.** The type is derived from a stored
  `SingleTimeSeries` rather than added; import that and run `transform`.

A dense forecast is placed by its **coordinates**, not its row order, so a file a query engine
sorted or partitioned still reads correctly. Every slot must be filled exactly once: a cube has no
hole to leave, and two rows for one slot means they disagree. Its grid comes from the `resolution`,
`interval` and `horizon` columns rather than being reverse-engineered — a merely self-consistent set
of rows would give a plausible wrong answer, since a one-window forecast is indistinguishable from a
static series and overlapping windows make the interval ambiguous.

### `data_hash` is a checksum

The import re-encodes each group, hashes it, and refuses a mismatch naming the series. If you edited
values in DuckDB, **drop the column** — a file without it imports fine, which is also what a foreign
file looks like.

For composite kinds the hash is over the **decoded points**, not the packed bytes: a file re-pads
them to its widest series, so hashing the padding would make an untouched export fail its own
checksum. One consequence: for a composite series this column is not the `data_hash` the catalog
holds, and `id` is the way back to that.

### Foreign files

Anything with a `timestamp` and a `value` column reads. What the columns do not say is inferred, and
each inference takes the reading that assumes least:

| Missing                          | Read as                                                                                                                                                    |
| -------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `time_series_type`               | `SingleTimeSeries` when the timestamps walk a grid, `NonSequentialTimeSeries` otherwise. `PersistentTimeSeries` is never inferred — name it with `--type`. |
| `element_type`                   | The leaf Arrow type. A `FixedSizeList<double>[3]` becomes `f64` with element shape `[3]` — _dense_, not `tuple(3,f64)`, because the bytes cannot say.      |
| `time_reference`                 | The timestamp column's Arrow zone; a column with no zone reads as `zoneless`, since a naive timestamp is a wall clock.                                     |
| `name`, `owner_id`, `owner_type` | Nothing. All three are refused rather than defaulted — owner 0 is a real owner, not a sentinel — so pass `--name`, `--owner-id`, `--owner-type`.           |

The inline flags fill those in and override a column for every series in the file, with one
exception that follows the project's usual rule: `--element-type` is an **assertion**.
`--element-type
'tuple(3,f64)'` states the reading the bytes cannot, and a value contradicting the
file is an error rather than a silent replacement. `--type` behaves the same way.

## What a round trip does not preserve

- **The catalog id.** `add` never accepts one — "never reissued" is a guarantee of the catalog's
  `AUTOINCREMENT`, and a caller free to name an id could re-file a retired one — so the `id` column
  is reported at `--dry-run` and then ignored. The destination assigns fresh ids.
- **An empty series.** A long table has one row per value, so a series with none contributes no rows
  and is in no file. The export **warns, naming it**.
- **An empty-string descriptor.** The empty string is how the format writes "absent", so a stored
  empty string reads back as absent. `unit_system`'s unset state means _unspecified_ rather than
  natural units, and the empty string preserves that.
- **A composite series' padding.** Values round-trip exactly — an improvement over CSV, where floats
  pass through decimal text — but a composite is re-padded on the way out and shrunk to its own
  width on the way back, so its stored `data_hash` may differ from the original's.
