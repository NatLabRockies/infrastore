# Parquet Layout

What `infrastore export -f parquet` writes and `infrastore add --parquet` reads: a **normalized,
partitioned** layout. Per partition, two files sharing a stem — one holding every distinct array
once, one holding the catalog rows that name them.

Two things it is not. Not one file per series: a store with thousands of series would become
thousands of files, which defeats every reader worth exporting for. And not one table with the
catalog row beside every value: the store is content-addressed, so a thousand components sharing one
profile hold **one** array, and a denormalized table would write that profile a thousand times.
Parquet's compression does not find repeats across pages, so the file really would be a thousand
times larger.

> This is a **file interchange** format, not the store's own. The on-disk store is
> [HDF5 plus SQLite](file-format.md), and nothing here is load-bearing for it. Python's `to_arrow()`
> and `from_arrow()` are a different thing again: per-series, in-memory conveniences. The
> relationship is one sentence — a series file's columns are `to_arrow()`'s schema-metadata keys
> turned into columns, and the values file is its two columns keyed by the array — and neither side
> depends on the other.

## The two files

```text
parquet/
  SingleTimeSeries.f64.utc.values.parquet     one row per value, each array once
  SingleTimeSeries.f64.utc.series.parquet     one row per series
```

| File            | Rows           | Carries                                        |
| --------------- | -------------- | ---------------------------------------------- |
| `<stem>.values` | one per value  | the array key, the time coordinates, the value |
| `<stem>.series` | one per series | the array key and the whole catalog row        |

They join on the **array key**, and both are sorted by it. That is the entire relationship: a values
row belongs to whichever series rows carry its key, and a series row reads whichever values group
carries its key.

## The array key

```text
(data_hash, time_axis)
```

`data_hash` alone will not do. It covers the array's bytes and not the axis those bytes sit on, and
the store pools irregular series by time axis precisely because two series with identical values on
different timelines are one stored array. So the key is the pair.

`time_axis` is a string spelling whatever decides where a value sits in time, per type:

| Type                                              | `time_axis`                                                      |
| ------------------------------------------------- | ---------------------------------------------------------------- |
| `SingleTimeSeries`                                | `R<length>/<initial>/<resolution>`, an ISO 8601 repeat           |
| `NonSequentialTimeSeries`, `PersistentTimeSeries` | the axis's `timestamps_hash`, hex — the catalog's own key for it |
| dense forecasts                                   | `R<count>/<initial>/<interval>/<horizon>/<resolution>`           |

```text
R24/2024-01-01T00:00:00Z/PT1H
9f0c4e...                                        (an irregular axis)
R24/2024-01-01T00:00:00Z/PT1H/PT2H/PT1H          (a forecast)
```

The instant is spelled in **UTC** whatever the partition's `time_reference`, because the reference
is a partition key and repeating it would say nothing. A forecast needs its horizon as well as its
interval: the horizon's own step decides how many target times a window has, so two forecasts
sharing an array, an anchor and an interval but not a horizon are different tables.

The axis is read off the **values being exported**, not off the catalog row. The two agree for a
whole-series export; where they disagree — `export --time-range` writes a slice whose anchor and
length are its own — the values are what the file holds, and `data_hash` beside them is computed
from the same slice.

## Partitioning

Three things cannot vary within one Parquet file without nullable or ill-typed columns: the **set of
key columns** (a forecast has an `issue_time`, a static series does not), the **Arrow type of
`value`**, and the **zone of `timestamp`**. So an export partitions its selection by the triple

```text
(time_series_type, value type, time_reference)
```

and writes one file **pair** per distinct triple. The payoff is that **every column in both files is
required**: there are no nullable columns anywhere in this format, and a reader never has to ask
whether a null means "absent" or "unknown".

The **value type** is the element type with its per-step shape, except that the four composite kinds
(`linear_function`, `quadratic_function`, `piecewise_linear`, `piecewise_step`) partition by **kind
alone**. Their stored width varies per series — a `piecewise_linear` row is `[n, x₁, y₁, …]`
zero-padded to the widest timestep in that series — so keying by width would scatter one kind across
a file per width. Instead every composite row in a partition is re-padded to the widest series in
it, which the layout allows: the leading count `n` keeps each row self-describing whatever the
padding. The cost is a `data_hash` caveat, below.

An **unspecified** `time_reference` is a partition of its own. A series that declared no spelling
must not pool with one that declared UTC, even though both write a UTC-zoned column.

### File names

`<type>.<value-slug>.<reference-slug>` plus `.values.parquet` or `.series.parquet`:

```text
SingleTimeSeries.f64.utc.values.parquet
SingleTimeSeries.f64_2x3.America_Denver.series.parquet
Deterministic.tuple3_f64.offset_minus07_00.values.parquet
NonSequentialTimeSeries.piecewise_linear.zoneless.series.parquet
PersistentTimeSeries.f64.unspecified.values.parquet
```

Slugs avoid every character Windows forbids (`<>:"/\|?*`), which matters because a zone name carries
`/` and a fixed offset carries `:`; a leading `-` is avoided too, since it reads as a flag wherever
the name is passed to a command. Trailing dots go (Windows strips them, so `a.` and `a` would be one
file there), and a fragment that would be a device name (`CON`, `NUL`, `LPT9`, …) gets a prefix.

**The name is a convenience, not the truth.** The slug function is one-way — the zones `a/b` and
`a_b` both flatten to `a_b` — so nothing parses a partition back out of a filename, and two
partitions that would collide get a numeric suffix in the keys' own sort order, so a re-run of the
same export produces the same names. The footer carries the exact key.

## Values file columns

| Column       | Type                  | Files           | Notes                                                                        |
| ------------ | --------------------- | --------------- | ---------------------------------------------------------------------------- |
| `data_hash`  | `utf8` (hex)          | all             | Half the array key; also a checksum on import.                               |
| `time_axis`  | `utf8`                | all             | The other half.                                                              |
| `timestamp`  | `timestamp(ms, zone)` | all             | The target time for a forecast, the breakpoint for a `PersistentTimeSeries`. |
| `issue_time` | `timestamp(ms, zone)` | forecasts       | Which window the row belongs to. Same zone as `timestamp`.                   |
| `percentile` | `float64`             | `Probabilistic` | One row per (issue, target, percentile).                                     |
| `scenario`   | `int64`               | `Scenarios`     | Zero-based trajectory index.                                                 |
| `value`      | see below             | all             |                                                                              |

Nothing about who owns the array is here, which is exactly what stops a shared profile being written
once per component.

## Series file columns

| Column                                                                         | Type                 | Files                         | Notes                                                                       |
| ------------------------------------------------------------------------------ | -------------------- | ----------------------------- | --------------------------------------------------------------------------- |
| `data_hash`, `time_axis`                                                       | `utf8`               | all                           | The array this series reads.                                                |
| `id`                                                                           | `int64`              | all                           | Provenance only; ignored on import.                                         |
| `owner_id`                                                                     | `int64`              | all                           |                                                                             |
| `owner_type`                                                                   | `utf8`               | all                           |                                                                             |
| `owner_category`                                                               | `utf8`               | all                           | `Component` or `SupplementalAttribute`.                                     |
| `time_series_type`                                                             | `utf8`               | all                           | Constant per partition.                                                     |
| `name`                                                                         | `utf8`               | all                           |                                                                             |
| `initial_timestamp`                                                            | `timestamp(ms,zone)` | `SingleTimeSeries`, forecasts | The anchor the values start at.                                             |
| `resolution`                                                                   | `utf8` (ISO-8601)    | `SingleTimeSeries`, forecasts | Absent from the irregular types, which have no constant step.               |
| `length`                                                                       | `int64`              | `SingleTimeSeries`            |                                                                             |
| `interval`, `horizon`                                                          | `utf8` (ISO-8601)    | forecasts                     |                                                                             |
| `count`                                                                        | `int64`              | forecasts                     | Windows.                                                                    |
| `features`                                                                     | `utf8` (JSON object) | all                           | `{}` when empty. Plain scalars: `{"model_year":2030}`.                      |
| `element_type`, `element_shape`                                                | `utf8`               | all                           | Constant per partition except a composite's width.                          |
| `time_reference`                                                               | `utf8`               | all                           | Constant per partition; the literal `unspecified` when the series has none. |
| `units`, `quantity_kind`, `unit_system`, `component_field`, `application_data` | `utf8`               | all                           | **Empty string when absent**, which is what keeps every column required.    |

The grid columns come from the values being exported, for the same reason `time_axis` does. Every
string column is dictionary-encoded; compression is zstd on both files.

### The `value` column

Its Arrow type follows the element type alone. Nesting is innermost-first, which is the order the
flat row-major buffer is already in.

| `element_type` and shape            | Arrow type                                                                    |
| ----------------------------------- | ----------------------------------------------------------------------------- |
| scalar dtype, shape `[]`            | primitive (`double`, `int32`, `bool`, …)                                      |
| `tuple(N,T)`                        | `FixedSizeList<T>[N]`                                                         |
| scalar dtype with dense shape `[N]` | `FixedSizeList<T>[N]`                                                         |
| scalar dtype with shape `[M,N]`     | `FixedSizeList<FixedSizeList<T>[N]>[M]`                                       |
| composite kinds                     | `FixedSizeList<double>[w]`, stored packing, `w` the partition's widest series |

Composites keep their **stored packing** rather than being decoded into `Struct`/`List`. That
packing is the documented cross-language wire form, held to `conformance/element_type_vectors.json`,
and every binding has a decoder for it; a `--decode` option is a possible follow-up. Casting to a
common dtype is ruled out — the dtype round trip is a project promise.

### Row order and row groups

Both files are sorted by `data_hash`, `time_axis`; the series file then sorts by `id`. Within one
array the values rows are sorted by `issue_time`, then `timestamp`, then `percentile` or `scenario`,
so a forecast comes out window-major and `GROUP BY issue_time` scans contiguously.

Row groups in the values file target roughly one million rows and are cut at an **array boundary**
whenever the coming array would carry the group past the target, so row-group statistics on
`data_hash` mean something and a reader can skip whole groups. An array larger than the target on
its own spans several. The series file is one row group: it has one row per series rather than one
per value, so even a store with a million series is a file a reader loads whole.

## Footer

Both files carry it.

| Key                      | Value                                                                                                 |
| ------------------------ | ----------------------------------------------------------------------------------------------------- |
| `infrastore.format`      | `normalized_v1`. Read first, so a later format is refused by version rather than by a missing column. |
| `infrastore.role`        | `values` or `series`.                                                                                 |
| `time_series_type`       | The partition's type.                                                                                 |
| `element_type`           | The partition's element type.                                                                         |
| `element_shape`          | JSON list; a composite's is the width the partition settled on.                                       |
| `time_reference`         | The partition's spelling, `unspecified` included.                                                     |
| `rows_contiguous_by_key` | `true`.                                                                                               |

The three partition keys are here **exactly**, because the filename is one-way.

## Reading it back

`add --parquet` takes a **file, a directory, or a partition stem** — `out/SingleTimeSeries.f64.utc`
names the pair — and commits **one transaction per partition**, so a partition that fails leaves the
ones already committed alone.

The import is a **merge join**. Both files are sorted by the array key, so it walks them together:
read the next values group into one array, file every series row carrying that key, move on. Peak
memory is one values group plus one row group of each file, never a partition. The store's write
path already recognizes an array it holds, so the second through thousandth adds of one array are
catalog inserts only.

Both dangling sides are errors, each naming the key:

- a **series row whose key names no values group** has no array to read;
- a **values group no series row claims** is an array nothing would file.

Either means the two halves came from different exports, or one was truncated. A `.series.parquet`
with no `.values.parquet` beside it is refused for the same reason, before anything is read.

**Keys must be contiguous.** A key that reappears after another key's rows, in either file, is
refused with a message saying to sort by `data_hash`, `time_axis`. Stitching it back together would
mean holding the whole file, which is what the layout exists to avoid. A file you have re-sorted in
a query engine must meet the same rule.

`add` never accepts an id, so the `id` column is reported at `--dry-run` and then dropped. Identity
on import is the row's own `KeyIdentity` columns, as for any add.

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
hole to leave, and two rows for one slot means they disagree. Its grid comes from the series file's
`resolution`, `interval` and `horizon` rather than being reverse-engineered — a merely
self-consistent set of rows would give a plausible wrong answer, since a one-window forecast is
indistinguishable from a static series and overlapping windows make the interval ambiguous.

### `data_hash` is a checksum

The import re-encodes each group, hashes it, and refuses a mismatch naming the key. If you edited
values in a query engine, either recompute the hash or pass **`--no-checksum`**, which waives the
comparison and leaves the pair as nothing but a join key. Dropping the column is not the remedy: a
values file without it has no key, and no key means no series file to join to.

For composite kinds the hash is over the **decoded points**, not the packed bytes: a partition
re-pads them to its widest series, so hashing the padding would make an untouched export fail its
own checksum — and it is what lets two composite series whose curves are the same points at
different paddings share one values group. One consequence: for a composite series this column is
not the `data_hash` the catalog holds, and `id` is the way back to that.

### Foreign files

A values file with **no series file beside it** is a foreign file — anything with a `timestamp` and
a `value` column. It carries no catalog rows, so the inline flags supply what a series row would
have. It is read as one series per distinct `(data_hash, time_axis)` if those columns exist, and as
exactly one series if they do not.

What the columns do not say is inferred, and each inference takes the reading that assumes least:

| Missing                          | Read as                                                                                                                                                    |
| -------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `time_series_type`               | `SingleTimeSeries` when the timestamps walk a grid, `NonSequentialTimeSeries` otherwise. `PersistentTimeSeries` is never inferred — name it with `--type`. |
| `element_type`                   | The leaf Arrow type. A `FixedSizeList<double>[3]` becomes `f64` with element shape `[3]` — _dense_, not `tuple(3,f64)`, because the bytes cannot say.      |
| `time_reference`                 | The timestamp column's Arrow zone; a column with no zone reads as `zoneless`, since a naive timestamp is a wall clock.                                     |
| `name`, `owner_id`, `owner_type` | Nothing. All three are refused rather than defaulted — owner 0 is a real owner, not a sentinel — so pass `--name`, `--owner-id`, `--owner-type`.           |

### Inline flags

They fall into three groups, and the split follows the project's usual rule.

**Overrides** replace a column for every series in the partition: `--owner-id`, `--owner-type`,
`--owner-category`, `--name`, `--feature`, `--time-reference`, and the five free-form descriptors
`--units`, `--quantity-kind`, `--unit-system`, `--component-field`, `--application-data`. Passing an
empty string clears a descriptor, since the empty string is how this format spells "absent" anyway.

**Assertions** state something a file cannot, so a file that contradicts one is an error rather than
being silently replaced: `--element-type` (`tuple(3,f64)` states the reading the bytes cannot),
`--element-shape`, `--resolution`, and `--type`. On a foreign file, which says nothing to
contradict, an assertion is simply the answer — `--resolution PT1H` names the grid, and the rows are
then checked against the grid it generates.

**Refused** with `--parquet`, by name rather than silently dropped: `--initial-timestamp`,
`--interval`, `--horizon`, `--count`, `--percentile`, `--scenario-count`, `--layout`, `--owner-map`,
`--owner-id-from`. The values imply the grid — a `SingleTimeSeries`' anchor is its first timestamp
and a forecast's windows are its `issue_time` column — so a flag naming one is either redundant or a
contradiction nothing should have to adjudicate, and `--layout` describes a CSV's columns. A foreign
**forecast** — a values file with an `issue_time` column and no series file — is consequently not
supported; export one and keep both halves.

## Querying it

The join is the whole idiom:

```sql
SELECT s.name, s.owner_id, s.units, max(v.value) AS peak
FROM 'parquet/SingleTimeSeries.f64.utc.values.parquet' v
JOIN 'parquet/SingleTimeSeries.f64.utc.series.parquet' s USING (data_hash, time_axis)
GROUP BY s.name, s.owner_id, s.units;
```

A view flattens it once and hides the join from everything after:

```sql
CREATE VIEW load AS
SELECT s.*, v.timestamp, v.value
FROM 'parquet/SingleTimeSeries.f64.utc.values.parquet' v
JOIN 'parquet/SingleTimeSeries.f64.utc.series.parquet' s USING (data_hash, time_axis);

SELECT timestamp, value FROM load WHERE owner_id = 42 AND name = 'load' ORDER BY timestamp;
```

That view is the denormalized table this format deliberately does not write: materializing it costs
exactly what the layout saves, and a query engine that keeps it as a view pays nothing.

## What a round trip does not preserve

- **The catalog id.** `add` never accepts one — "never reissued" is a guarantee of the catalog's
  `AUTOINCREMENT`, and a caller free to name an id could re-file a retired one — so the `id` column
  is reported at `--dry-run` and then ignored. The destination assigns fresh ids.
- **An empty-string descriptor.** The empty string is how the format writes "absent", so a stored
  empty string reads back as absent. `unit_system`'s unset state means _unspecified_ rather than
  natural units, and the empty string preserves that.
- **A composite series' padding.** Values round-trip exactly — an improvement over CSV, where floats
  pass through decimal text — but a composite is re-padded on the way out and shrunk to its own
  width on the way back, so its stored `data_hash` may differ from the original's.

An **empty series** is not a round-trip caveat but a refusal: the export **fails**, naming every
empty series it was asked for and writing nothing. A values file has one row per value, so an empty
series would be a series row whose key matches no values group — which is exactly what a truncated
export looks like, and there is no way to write one a reader could tell apart from damage. Narrow
the selection past it.
