# CLI Reference

`infrastore-cli` builds the `infrastore` binary, which reads and writes a store directly on disk
(HDF5 + SQLite). For a task-oriented walkthrough, see
[Use the `infrastore` CLI](../how-to/use-cli.md).

The CLI covers time series, plus read-only views of the
[association catalogs](../explanation/data-model.md#associations-between-entities) (`attributes`,
`links`). Writing an association means writing the consumer's object graph alongside it, so that
direction stays with the Rust, Python, and Julia APIs.

## Synopsis

```text
infrastore [--store <PATH.h5>] [-f <FORMAT>] [--log-level <FILTER>] <COMMAND>
```

### Global options

| Option           | Description                                                                                                                      |
| ---------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `--store <PATH>` | Path to the HDF5 store file. The `<PATH>.sqlite` catalog is implicit. Falls back to the `INFRASTORE_STORE` environment variable. |
| `-f`, `--format` | Output format: `table` (default), `json`, or `csv`.                                                                              |
| `--log-level`    | Tracing filter; also read from `RUST_LOG`. Defaults to `warn`.                                                                   |

`--store` (or `INFRASTORE_STORE`) is required by every command except `template` and `completions`.

`-f`/`--format` affects the read/inspection commands (`list`, `get`, `info`, `export`, `stats`,
`summary`, `verify`, `check-consistency`, `resolutions`, `params`, `compact`). It is accepted
anywhere because it is global, but the write commands (`add`, `remove`, `rename`, `copy`,
`replace-owner`, `clear`, `transform`, `persist`) ignore it and print plain text; `template` always
prints a JSON descriptor. `export` requires `-f csv` or `-f json` (there is no table export).

## Commands

`infrastore --help` lists these under the same six headings used below. The grouping is a display
aid only — every command is invoked flat, as `infrastore <command>`; there are no subcommand
namespaces to type.

### Read data

| Command  | Purpose                                                                   |
| -------- | ------------------------------------------------------------------------- |
| `list`   | List stored series matching the selector filters.                         |
| `get`    | Read and display a single series' values.                                 |
| `info`   | Metadata, content hash, HDF5 location, and stats for one series.          |
| `export` | Write series values to CSV/JSON files (`--dir`), or stdout for one match. |

### Write data

| Command         | Purpose                                                                         |
| --------------- | ------------------------------------------------------------------------------- |
| `add`           | Add one or more series from a descriptor JSON + CSV.                            |
| `transform`     | Derive `DeterministicSingleTimeSeries` from stored `SingleTimeSeries`.          |
| `remove`        | Delete a single series, or every match with `--all` (prompts unless `--force`). |
| `rename`        | Rename the single series a selector resolves to (`--new-name`).                 |
| `copy`          | Copy the single series a selector resolves to onto another owner.               |
| `replace-owner` | Reassign every series from one owner to another.                                |
| `clear`         | Remove all series, or all for one owner (prompts unless `--force`).             |

### Inspect the store

| Command       | Purpose                                                                       |
| ------------- | ----------------------------------------------------------------------------- |
| `stats`       | Association, owner, and distinct-array counts.                                |
| `store-info`  | HDF5 + SQLite paths and sizes, on-disk format version, compression.           |
| `arrays`      | Distinct stored arrays: content hash, HDF5 location, series sharing each.     |
| `summary`     | Grouped static and/or forecast summaries (`--static-only`/`--forecast-only`). |
| `resolutions` | List distinct resolutions and forecast intervals.                             |
| `params`      | Show the store's forecast parameters (`--resolution`/`--interval`).           |

### Associations

| Command      | Purpose                                                                     |
| ------------ | --------------------------------------------------------------------------- |
| `attributes` | Component <-> supplemental-attribute associations (`--summary` for counts). |
| `links`      | Directed parent -> child component associations.                            |

### Integrity & maintenance

| Command             | Purpose                                                              |
| ------------------- | -------------------------------------------------------------------- |
| `verify`            | Verify store integrity; nonzero exit if errors are present.          |
| `check-consistency` | Verify the per-resolution static grid (`--resolution`).              |
| `compact`           | Reclaim reusable space (prompts unless `--force`); print the report. |
| `persist`           | Write the store to a new HDF5 + SQLite artifact (`--dest`).          |

### Scaffolding

| Command       | Purpose                                                 |
| ------------- | ------------------------------------------------------- |
| `template`    | Print an example descriptor for a given type to stdout. |
| `completions` | Generate shell completions to stdout (bash/zsh/fish/…). |

```text
infrastore --store <PATH> add --descriptor <FILE.json> [--csv <FILE.csv>] [--compression <none|deflate[:LEVEL]>] [--no-shuffle]
infrastore --store <PATH> list    [SELECTOR...] [--limit N] [--wide]
infrastore --store <PATH> get     [SELECTOR...] [--time-range START..END] [--limit N | --full]
infrastore --store <PATH> info    [SELECTOR...] [--no-stats]
infrastore --store <PATH> -f csv|json export [SELECTOR...] [--dir <DIR>]
infrastore --store <PATH> remove  [SELECTOR...] [--all] [--force] [--dry-run]
infrastore --store <PATH> rename  [SELECTOR...] --new-name <NAME> [--dry-run]
infrastore --store <PATH> copy    [SELECTOR...] --dst-owner-id <I> --dst-owner-type <T> [--new-name <NAME>] [--dry-run]
infrastore --store <PATH> replace-owner --old <I> --new <I> --owner-category <C> [--dry-run]
infrastore --store <PATH> clear   [--owner-id <I> --owner-category <C>] [--force] [--dry-run]
infrastore --store <PATH> transform --horizon <DUR> --interval <DUR> [--owner-category <C>] [--resolution <DUR>]
infrastore --store <PATH> persist --dest <PATH.h5>
infrastore --store <PATH> compact [--force]
infrastore --store <PATH> stats
infrastore --store <PATH> store-info
infrastore --store <PATH> arrays [SELECTOR...] [--data-hash <HEX>]
infrastore --store <PATH> attributes [--component-id <I>] [--attribute-id <I>] [--component-type <T>] [--attribute-type <T>] [--summary]
infrastore --store <PATH> links [--parent-id <I>] [--child-id <I>] [--parent-type <T>] [--child-type <T>]
infrastore completions <SHELL>
infrastore --store <PATH> summary [--static-only | --forecast-only]
infrastore --store <PATH> verify
infrastore --store <PATH> check-consistency [--resolution <DUR>]
infrastore --store <PATH> resolutions
infrastore --store <PATH> params [--resolution <DUR>] [--interval <DUR>]
infrastore template <single|non_sequential|deterministic|probabilistic|scenarios>
```

`--csv` overrides the `csv` path inside the descriptor, and only works for a descriptor that
describes a single series. Passing it alongside a descriptor array holding more than one object
fails with `--csv cannot be used with an array descriptor`.

`transform` takes no selector: it rewrites **every** `SingleTimeSeries` in the store, deriving a
`DeterministicSingleTimeSeries` from each. `--owner-category` and `--resolution` optionally scope it
to one category and/or resolution. `--horizon` must not exceed the shortest matched series
(`horizon / resolution` steps must fit within its `length`), or the command fails.

`remove --all` uses the selector as a filter that may match several series, removing them all in one
transaction; without `--all`, the selector must resolve to exactly one series. `stats`, `summary`,
`verify`, `check-consistency`, `resolutions`, and `params` are read-only inspection commands and
honor `-f/--format`; `verify` exits nonzero when the integrity report lists any errors.

`export` is the read-direction inverse of the batch `add`: the selector may match many series, and
each is written to `<owner_id>_<owner_type>_<name>_<type>.csv|json` inside `--dir`. Without `--dir`
the selector must match exactly one series, which goes to stdout. CSV output carries real timestamps
(see the CSV Layout section); JSON output is one structured object per series, including its
`features` and `data_hash`.

That plain filename omits resolution, interval, and features, all of which are part of a series'
identity. When two matched series would share one filename, each gains a suffix naming the fields
that distinguish them (`..._PT1H_model_year-2030.csv`), so an export never silently overwrites part
of its own output. Filenames are compared case-insensitively, so an export produces the same set of
files on Linux, macOS, and Windows.

`--dry-run` on `remove`, `clear`, `replace-owner`, `rename`, and `copy` prints what would change and
exits without opening the store for writing. `add --compression` sets the HDF5 compression policy
for a store this command creates (`none`, `deflate`, or `deflate:LEVEL` with `--no-shuffle` to
disable byte-shuffle); passing it for an existing store is an error, since the persisted policy
governs.

### Selectors

`get`, `info`, and `remove` identify exactly one series with these flags; `list` accepts the same
flags as filters. Every flag is optional. Only `--feature` may be repeated; the rest take a single
value:

| Flag                   | Meaning                                                                    |
| ---------------------- | -------------------------------------------------------------------------- |
| `--owner-id <I>`       | Owner identifier (`i64` integer).                                          |
| `--owner-category <C>` | Restrict to `component` or `supplemental_attribute`; omit to match either. |
| `--name <N>`           | Series name (exact match).                                                 |
| `--name-glob <P>`      | Name pattern (SQLite `GLOB`: case-sensitive `*`/`?`). ANDed with `--name`. |
| `--type <T>`           | See the type spellings below.                                              |
| `--resolution <DUR>`   | Resolution, e.g. `1h`, `15min`, or ISO-8601 like `PT1H`, `P1M`.            |
| `--feature key=value`  | Feature filter; repeatable. Values are inferred as int/float/bool/string.  |

If a selector matches more than one series, `infrastore` errors and lists the candidates so the
query can be narrowed. Each candidate line spells out every field that is part of identity —
including `features` and a short `data_hash` — so the flag that separates them is always visible.
Long candidate lists are truncated after ten entries; rerun the same flags under `list` to see them
all. The owner identity is the pair `(owner_id, owner_category)`, so a component and a supplemental
attribute may share a numeric `owner_id`; add `--owner-category` to disambiguate when both exist.

### Type Spellings

`--type` (and the descriptor's `type` key) accepts six concrete types. Matching is case-insensitive
and ignores underscores, so each has a short form and a full form:

| Type                            | Accepted spellings                                      |
| ------------------------------- | ------------------------------------------------------- |
| `SingleTimeSeries`              | `single`, `SingleTimeSeries`                            |
| `NonSequentialTimeSeries`       | `non_sequential`, `NonSequentialTimeSeries`             |
| `Deterministic`                 | `deterministic`                                         |
| `DeterministicSingleTimeSeries` | `deterministic_single`, `DeterministicSingleTimeSeries` |
| `Probabilistic`                 | `probabilistic`                                         |
| `Scenarios`                     | `scenarios`                                             |

`--type deterministic` matches a stored `Deterministic` _and_ the `DeterministicSingleTimeSeries`
rows that `transform` produces — how a forecast came to exist is not something you need to know to
select it. Listed rows still report their own stored type, so you can see which are synthetic, and
`--type deterministic_single` selects only those.

`deterministic_single` is not writable from a descriptor (use `transform`), but it _is_ selectable,
and it is often required: `transform` derives a series that shares `(owner_id, name, resolution)`
with its source `SingleTimeSeries`, so after a transform a query like
`infrastore get --owner-id 42 --name load` matches two series and errors. `--type single` or
`--type deterministic_single` is the only way to pick one.

Note that inputs and outputs use different spellings. You _pass_ the short, lowercase forms
(`--type single`, `--owner-category component`), but `list`, `get`, and `info` _render_ the
canonical CamelCase names (`SingleTimeSeries`, `Component`). Piping `-f json list` output straight
back into a filter therefore needs no translation — the CamelCase form is accepted as input too —
but string-comparing rendered output against the short form will not match.

## Durations and Timestamps

- **Durations** (`resolution`, `horizon`, `interval`): an integer plus a unit — `ms`, `s`, `min`,
  `h`, `d` (e.g. `500ms`, `15min`, `24h`, `7d`). A bare integer is milliseconds. These three also
  accept ISO-8601 duration strings (e.g. `PT1H`, `P1M`, `P1Y`); a calendar grid
  (monthly/quarterly/annual) can only be expressed this way, since the human-unit form is always a
  fixed span.
- **Timestamps** (`initial_timestamp`, non-sequential timestamp column): RFC3339 (e.g.
  `2024-01-01T00:00:00Z`) or a bare integer of epoch milliseconds.
- **`--time-range`** is a pair of _timestamps_, not a duration: `START..END` (half-open), where each
  side is parsed as a timestamp. For example
  `--time-range 2024-01-01T01:00:00Z..2024-01-01T03:00:00Z`. A duration such as `--time-range 1h` is
  rejected with `invalid --time-range '1h' (expected START..END)`.

## Descriptor Schema

A descriptor JSON file is either a single object (one series) or an array of objects (batch add).
The CSV holds only numbers (plus a leading timestamp column for `non_sequential`).

| Key                            | Required for                | Notes                                                      |
| ------------------------------ | --------------------------- | ---------------------------------------------------------- |
| `owner_id`                     | all                         | Integer component identifier (`i64`).                      |
| `owner_type`                   | all                         |                                                            |
| `owner_category`               | optional                    | `component` (default) or `supplemental_attribute`.         |
| `name`                         | all                         |                                                            |
| `type`                         | all                         | One of the five writable types.                            |
| `dtype`                        | all                         | `f64`, `f32`, `i64`, `i32`, `u64`, `bool`.                 |
| `csv`                          | unless `--csv` is passed    | Path relative to the descriptor; `--csv` overrides it.     |
| `has_header`                   | optional                    | Skip the first CSV row. Default `true`.                    |
| `element_shape`                | optional                    | Trailing per-step dims; default scalar (`[]`).             |
| `units`                        | optional                    | Free-form label.                                           |
| `ext`                          | optional                    | Opaque package-owned payload (e.g. JSON), stored verbatim. |
| `features`                     | optional                    | JSON object; int/float/bool/string values. See below.      |
| `initial_timestamp`            | all except `non_sequential` |                                                            |
| `resolution`                   | all except `non_sequential` |                                                            |
| `horizon`, `interval`, `count` | forecasts                   |                                                            |
| `percentiles`                  | `probabilistic`             | Strictly increasing list of floats.                        |
| `scenario_count`               | `scenarios` (optional)      | Inferred from the data length if omitted.                  |

Unknown keys are rejected. Any key not in the table above — including a typo like `resolutionn` — is
a hard parse error listing the accepted fields, so hand-edited templates fail loudly rather than
silently dropping a setting.

Inside `features`, a name that shadows a time-series or key field (`name`, `resolution`, `owner_id`,
…) is rejected when the series is added — see
[reserved feature names](../explanation/data-model.md#reserved-feature-names).

## CSV Layout

`infrastore` computes the full array shape from the descriptor and reads the CSV's value cells in
**row-major** order to fill it. The total cell count must equal the product of the shape.

| Type             | Shape                             | CSV                                                                    |
| ---------------- | --------------------------------- | ---------------------------------------------------------------------- |
| `single`         | `[length, *element_shape]`        | One value column (or `prod(element_shape)` columns), one row per step. |
| `non_sequential` | `[length, *element_shape]`        | First column is the timestamp, then value columns.                     |
| `deterministic`  | `[H, count, *E]`                  | Flat row-major values; `H = horizon / resolution`.                     |
| `probabilistic`  | `[num_percentiles, H, count, *E]` | Flat row-major values.                                                 |
| `scenarios`      | `[scenario_count, H, count, *E]`  | Flat row-major values.                                                 |

`bool` cells accept `true`/`false`/`1`/`0`. The table above is the **write** layout: the rawest
form, which is what `template` prints and what a hand-authored CSV should look like.

### Reading back, and re-adding

Every CSV `infrastore` writes carries timestamps, because they are the useful part of the output and
because a piped file otherwise loses the time axis entirely — `initial_timestamp` and `resolution`
live in the catalog, not in the file.

| Type                       | `get -f csv` / `export -f csv` header                               |
| -------------------------- | ------------------------------------------------------------------- |
| `single`, `non_sequential` | `timestamp,value...`                                                |
| `deterministic`            | `issue_time,target_time,value...`                                   |
| `probabilistic`            | `issue_time,target_time,value[p10],...` (one column per percentile) |
| `scenarios`                | `issue_time,target_time,value[s0],...` (one column per scenario)    |

`add` reads both layouts. It picks between them from the header row, so a file written by `export`
can be handed straight back to `add` with no column surgery:

- a first column named `timestamp` is read as the time axis;
- leading `issue_time` + `target_time` columns mark the timestamped forecast layout, whose rows run
  window-major with the percentiles/scenarios spread across columns — `add` transposes them back
  into the stored `[series, horizon, count, element]` order;
- anything else is the flat write layout above.

Detection needs `has_header: true` (the default). With `has_header: false` there is no header to
read, so the flat write layout is assumed.

The round trip is exact for every type, including forecasts: values come back in the order they went
in. What a CSV cannot carry is the descriptor metadata — owner, name, features, units — so re-adding
still needs a descriptor supplying those. `export -f json` carries all of it, plus `data_hash`.

## Content Addressing

Arrays are stored by the SHA-256 of their contents, so two series holding identical values share one
array on disk. Three commands surface that, which is what makes an HDF5 file with fewer columns than
the catalog has rows explicable rather than alarming:

- `list` shows a 12-character `Hash` column — equal hashes mean a shared array.
- `info` shows the full 64-character `data_hash`, the `location` it maps to, and how many
  `SingleTimeSeries` / `DeterministicSingleTimeSeries` associations reference it.
- `arrays` groups by hash: one row per distinct array with its location and the series sharing it.
  `--data-hash <HEX>` narrows to one array and accepts any prefix, in either case.

`location` is what lets you go look at the same bytes with an outside tool:

```text
$ infrastore --store store.h5 info --name load --type single
  data_hash     2018057b75043a0b2716c36cbf6c183f909edf96952894f06fdad000abf45952
  location      /time_series/single/sts_f64_s_6_PT1H[:, 0]
  hdf5_dataset  /time_series/single/sts_f64_s_6_PT1H
  hdf5_column   0
```

The hash alone would not be enough. A packed array is one _column_ of a dataset shared with other
same-shaped arrays, and the column index is only recoverable by scanning that dataset's companion
`_h` hash dataset; a packed pool that fills up also spills into `{name}__1`, `{name}__2`, so even
the dataset name is not derivable from metadata. A standalone array (irregular series, dense
forecast) reports its own dataset and no column.

`info` reads the array to compute its min/max/mean. `--no-stats` skips that, leaving a purely
catalog-side query that never touches the HDF5 file.

### Reading the SQLite catalog by hand

The catalog stores both hashes as `BLOB`, which `sqlite3` renders as raw bytes in its default `list`
mode and in `.mode box` / `.mode json` — mangling the terminal, and in box mode the table borders
too. The store therefore ships a **`time_series_readable`** view with both hashes hex-encoded:

```sql
sqlite> SELECT name, data_hash FROM time_series_readable LIMIT 1;
load|2018057b75043a0b2716c36cbf6c183f909edf96952894f06fdad000abf45952
```

The view spells hashes in lowercase, matching what every binding and the CLI print, so a value
copied out of it pastes straight into `arrays --data-hash`. Querying the base table directly works
too, via `hex(data_hash)` or `.mode quote`; note that SQLite's `hex()` returns **uppercase**, which
`--data-hash` accepts.

The view is created on a store's first writable open, so a store last written by an older build
gains it as soon as anything opens it for writing. It is a projection only — nothing in the store
reads it — so its absence never affects reads.

## Exit Status

| Code | Meaning                                                                      |
| ---- | ---------------------------------------------------------------------------- |
| `0`  | Success.                                                                     |
| `1`  | Runtime error. The message is printed to stderr, prefixed with `Error:`.     |
| `2`  | Usage error from argument parsing (unknown flag, missing `--descriptor`, …). |
