# CLI Reference

`infrastore-cli` builds the `infrastore` binary, which reads and writes a store directly on disk
(HDF5 + SQLite). For a task-oriented walkthrough, see
[Use the `infrastore` CLI](../how-to/use-cli.md).

The CLI covers time series and both
[association catalogs](../explanation/data-model.md#associations-between-entities), read and write.
The store holds only the _relationship_ — the components and supplemental attributes themselves live
in the consumer's object graph — which is why the association flags are bare ids and type names.

## Synopsis

```text
infrastore [--store <PATH.h5>] [-f <FORMAT>] [--log-level <FILTER>] [-y]
           [--assume-timezone <ZONE> | --zoneless] <COMMAND>
```

Every global option is accepted after the command too (`infrastore add --store demo.h5 …`).

### Global options

| Option                     | Description                                                                                                                                                       |
| -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--store <PATH>`           | Path to the HDF5 store file. The `<PATH>.sqlite` catalog is implicit. Falls back to the `INFRASTORE_STORE` environment variable.                                  |
| `-f`, `--format`           | Output format: `table` (default), `json`, `jsonl`, or `csv`.                                                                                                      |
| `--log-level`              | Tracing filter; also read from `RUST_LOG`. Defaults to `warn`.                                                                                                    |
| `-y`, `--yes`              | Answer every confirmation prompt with yes.                                                                                                                        |
| `--assume-timezone <ZONE>` | Read timestamps that carry no time zone as being in this one: `UTC` (or `Z`), a fixed offset (`-07:00`, `-0700`, `-07`), or an IANA zone name (`America/Denver`). |
| `--zoneless`               | Store timestamps that carry no time zone as the wall clocks they are, naming no instant. Mutually exclusive with `--assume-timezone`.                             |

`--store` (or `INFRASTORE_STORE`) is required by every command except `template` and `completions`.

`-f`/`--format` applies to every command, read and write alike. The read/inspection commands
(`list`, `get`, `grid`, `info`, `export`, `names`, `owner-types`, `owners`, `exists`, `stats`,
`store-info`, `arrays`, `summary`, `attributes`, `links`, `diff`, `verify`, `check-consistency`,
`resolutions`, `params`, `compact`, and `add --dry-run`) render their results in it. The write
commands (`init`, `add`, `merge`, `remove`, `rename`, `copy`, `replace-owner`, `clear`, `transform`,
`persist`, `plot`, `attach`, `detach`, `link`, `unlink`, `reassign`) report their outcome in it:
prose under `table`, and a one-object status document under `json`/`jsonl`, so a scripted mutation
pipes into `jq` the way a scripted query does.

```console
$ infrastore --store s.h5 -f json --yes remove --all --owner-id 42 | jq .removed
3
```

Each command's document names what it did — `{"removed": N}`, `{"added": N, "store": …}`,
`{"merged": N, …}` — and a `--dry-run` reports `{"dry_run": true, "would_remove": N, …}` instead. A
filter that matches nothing still reports its zero rather than printing nothing, so `jq .removed`
reads `0` instead of failing on an empty document.

`csv` renders these status lines as prose alongside `table`: a status line has no rows to tabulate,
and a one-row CSV of it would give scripts a shape that changes every time the message is reworded.
JSON is the machine-readable channel for mutations. `template` always prints a JSON descriptor, and
`plot` writes the chart to its `--out` file — `-f json` shapes only the line reporting where it
went, and `--out -` puts the chart itself on stdout with no status line at all.

Diagnostics stay off stdout in every format: errors, the interactive `[y/N]` prompts, the `Aborted.`
notice, and `add`'s progress counter all go to stderr, so `-f json` output is only ever the
document. Errors follow the format too — `-f json` renders them as
`{"status": "error", "message": …}` on stderr, so a caller parsing one stream can parse both:

```console
$ infrastore --store missing.h5 -f json list 2>&1 >/dev/null | jq -r .message
store not found: missing.h5
```

`jsonl` is `json` line-delimited: one compact object per line with no enclosing `{"items": [...]}`,
so a 100 000-row `list` streams into `jq` instead of having to be buffered whole.

`export` has no table form; with `-f table` (the global default) it writes CSV, which is both what
`--dir` is for and what `add` reads back. Its `--dir` files are named for the format they hold —
`.csv`, `.json`, or `.jsonl` — and under `-f jsonl` each series is one compact line rather than a
pretty document.

`-y`/`--yes` answers every prompt, so a script no longer has to know which commands prompt or which
flag each spells it with. The per-command `--force` flags still work and are what a one-off reaches
for.

### Zoneless timestamps

A timestamp with no offset — `2024-01-01T00:00:00`, or the `2024-01-01 00:00:00` that most CSV
writers produce — names a wall-clock reading, not an instant. The CLI will not guess which of the
two you mean, so such a timestamp is refused and the error names both flags that resolve it:

```console
$ infrastore --store s.h5 add --csv load.csv ...
Error: timestamp '2024-01-01 00:00:00' names no time zone, so it names no instant. Give it an
offset (RFC3339, like 2024-01-01T00:00:00Z), pass --assume-timezone UTC (a fixed offset like
-07:00, or an IANA name like America/Denver) to read every zoneless timestamp with it, or pass
--zoneless to store them as the wall clocks they are.
```

#### `--assume-timezone`: resolve them to instants

```console
$ infrastore --store s.h5 --assume-timezone UTC             add --csv load.csv ...
$ infrastore --store s.h5 --assume-timezone -07:00          add --csv load.csv ...
$ infrastore --store s.h5 --assume-timezone America/Denver  add --csv load.csv ...
```

Three things to know:

- It applies **only** where an offset is missing. A timestamp that carries its own offset is never
  overridden, so a mixed file loads correctly and a fully-offset file is unaffected.
- It is global, so it also covers `--time-range` bounds and `--issue-time`, which hit the same
  parser.
- Whatever it resolves is also **recorded**, as the series' `time_reference` (see
  [Time references](../explanation/data-model.md#time-references)) — so a read hands the same
  spelling back rather than relabelling everything UTC. `--assume-timezone -07:00` over a midnight
  column stores `07:00Z` and prints `2024-01-01T00:00:00-07:00`.

**Prefer a named zone to a fixed offset** for anything that crosses a daylight-saving transition. A
year of Denver data read as `-07:00` renders every timestamp after March an hour wrong; the same
data read as `America/Denver` renders all of it correctly, because the zone is applied per instant
rather than baked in once.

A named zone is the one place in the system that runs local → instant, and it has two wall clocks it
cannot resolve. Both are **errors naming the row**, not guesses:

```console
$ infrastore --store s.h5 --assume-timezone America/Denver add --csv fold.csv ...
Error: timestamp '2024-11-03 01:30:00' is ambiguous in America/Denver: daylight saving repeats
that wall clock, so it names two instants (2024-11-03T07:30:00+00:00 and
2024-11-03T08:30:00+00:00). The file has to say which — give the row an explicit offset, or
re-read the column with --assume-timezone -06:00 or -07:00.
```

#### `--zoneless`: keep them as wall clocks

For data that has no time zone and wants none — modeled profiles on 24-hour days, say — `--zoneless`
stores the fields as written, converts nothing, and reads them back unlabelled:

```console
$ infrastore --store s.h5 --zoneless add --csv profile.csv ...
$ infrastore --store s.h5 get --owner-id 42 --name load -f csv
2024-01-01T00:00:00,1
```

The store then holds the series to that claim. An instant-bearing `--time-range` bound against it is
refused rather than coerced, and it cannot share one `grid` axis or one ranged bulk read with series
that do record instants — there is no single meaning either could carry for both. `list --wide`,
`info`, and `export -f json` all report the `time_reference`, and `store-info` lists the catalog's
distinct spellings with any unrecognized zone name flagged:

```console
$ infrastore --store s.h5 store-info
...
time_references   ["America/Denver", "utc", "America/Dever (unrecognized zone?)"]
```

A zone the store has never heard of is _reported_, never refused: the store does not gate on zone
existence, because that would refuse legitimate data whenever IANA moves ahead of this build's
database.

## Commands

`infrastore --help` lists these under the same eight headings used below. The grouping is a display
aid only — every command is invoked flat, as `infrastore <command>`; there are no subcommand
namespaces to type.

Each group's examples are below its table, and every command carries the same ones in its own help:
`infrastore <command> --help` ends with a worked invocation. A test parses all of them, so an
example can never name a flag the command does not have.

### Read data

| Command  | Purpose                                                                   |
| -------- | ------------------------------------------------------------------------- |
| `list`   | List stored series matching the selector filters.                         |
| `get`    | Read and display a single series' values.                                 |
| `grid`   | Render N series as N columns against one shared time axis.                |
| `info`   | Metadata, content hash, HDF5 location, and stats for one series.          |
| `export` | Write series values to CSV/JSON files (`--dir`), or stdout for one match. |

```sh
infrastore --store demo.h5 list                                   # everything in the store
infrastore --store demo.h5 list --name-glob 'load_*' --limit 20   # filtered, bounded
infrastore --store demo.h5 get --owner-id 42 --name load --full   # every row, not just 50
infrastore --store demo.h5 get --name load --plot                 # a terminal sparkline
infrastore --store demo.h5 get --name load --tail --limit 24      # the last day
infrastore --store demo.h5 -f csv grid --name-glob 'load_*' --resolution PT1H
infrastore --store demo.h5 info --name load --no-stats            # catalog only, no array read
infrastore --store demo.h5 -f csv export --name-glob 'load_*' --dir out/
```

#### Bounding the rows

`get` and `grid` separate the flags that _select data_ from the flags that _bound a display_, and
the two reach different formats:

| Flag                                                                    | Applies to     | Why                                                                                                                                 |
| ----------------------------------------------------------------------- | -------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `--time-range START..END`                                               | every format   | A time slice is a different span of the series, so a pipe should carry exactly the rows asked for.                                  |
| `--stride N` (`get`)                                                    | every format   | Every `N`th row is a different series, not a shorter view of one. `-f json` reports the strided `shape` and echoes `stride`.        |
| `--limit N`, `--full`, `--tail` (`get`); `--limit N`, `--full` (`grid`) | the table only | A CSV or JSON stream is read by another program, and a silently short one is a data bug in whatever reads it — not a shorter table. |

So `-f csv get --limit 3` still writes every row; thin a pipe with `--stride`, or slice it with
`--time-range`. The table's own default cap is 50 rows, lifted by `--full`.

### Write data

| Command         | Purpose                                                                         |
| --------------- | ------------------------------------------------------------------------------- |
| `init`          | Create an empty store with an explicit compression and catalog policy.          |
| `add`           | Add one or more series from a descriptor JSON + CSV, or from flags.             |
| `merge`         | Copy matching series from another store into this one.                          |
| `transform`     | Derive `DeterministicSingleTimeSeries` from stored `SingleTimeSeries`.          |
| `remove`        | Delete a single series, or every match with `--all` (prompts unless `--force`). |
| `rename`        | Rename the single series a selector resolves to (`--new-name`).                 |
| `copy`          | Copy the single series a selector resolves to onto another owner.               |
| `replace-owner` | Reassign every series from one owner to another.                                |
| `clear`         | Remove all series, or all for one owner (prompts unless `--force`).             |

```sh
infrastore --store demo.h5 init --compression deflate:6
infrastore --store demo.h5 add --descriptor load.json
infrastore --store demo.h5 add --descriptor batch.json --dry-run
infrastore --store demo.h5 add --descriptor batch.json --replace --batch-size 500
infrastore --store demo.h5 add --csv load.csv --owner-id 42 --owner-type Generator \
    --name load --type SingleTimeSeries --element-type f64 \
    --resolution PT1H --initial-timestamp 2024-01-01T00:00:00Z
infrastore --store demo.h5 merge --from other.h5 --name-glob 'load_*'
infrastore --store demo.h5 transform --horizon PT24H --interval PT1H
infrastore --store demo.h5 remove --owner-id 42 --name load --type SingleTimeSeries
infrastore --store demo.h5 remove --all --name-glob 'scratch_*' --dry-run
infrastore --store demo.h5 rename --owner-id 42 --name load --new-name demand
infrastore --store demo.h5 copy --name load --dst-owner-id 43 --dst-owner-type Generator
infrastore --store demo.h5 replace-owner --old 42 --new 43 --owner-category Component
infrastore --store demo.h5 clear --owner-id 42 --owner-category Component
```

### Discover

The step before writing a selector. `stats` says a store holds 5000 series and `list` shows the ones
matching a filter, but writing that filter means already knowing which names, owner types, and owner
ids exist. Each takes the same selector every read command does, so narrowing composes.

| Command       | Purpose                                                 |
| ------------- | ------------------------------------------------------- |
| `names`       | Distinct series names matching the selector.            |
| `owner-types` | Distinct owner types matching the selector.             |
| `owners`      | Distinct owner ids that have a time series.             |
| `exists`      | Whether anything matches; exit `0` for yes, `1` for no. |

```sh
infrastore --store demo.h5 names
infrastore --store demo.h5 names --owner-id 42
infrastore --store demo.h5 owner-types
infrastore --store demo.h5 owners --type SingleTimeSeries --resolution PT1H
infrastore --store demo.h5 exists --name load
```

`owners` projects to owner ids, so it takes only `--owner-category` (default `Component`), `--type`,
and `--resolution`; any other selector flag is refused rather than silently ignored. Use `list` when
you need the full filter.

### Visualize

| Command | Purpose                                            |
| ------- | -------------------------------------------------- |
| `plot`  | Draw a chart to a self-contained SVG or HTML file. |

```sh
infrastore --store demo.h5 plot --name load --out load.svg
infrastore --store demo.h5 plot --name load --kind duration --out ldc.html
infrastore --store demo.h5 plot --name load --kind heatmap --out heat.svg
```

See [Charts](#charts) below for the five `--kind` values and what each is for. For a quick
in-terminal check, `get --plot` draws a sparkline with no file involved.

### Inspect the store

| Command       | Purpose                                                                       |
| ------------- | ----------------------------------------------------------------------------- |
| `stats`       | Association, owner, and distinct-array counts.                                |
| `store-info`  | HDF5 + SQLite paths and sizes, on-disk format version, compression.           |
| `arrays`      | Distinct stored arrays: content hash, HDF5 location, series sharing each.     |
| `summary`     | Grouped static and/or forecast summaries (`--static-only`/`--forecast-only`). |
| `resolutions` | List distinct resolutions and forecast intervals.                             |
| `params`      | Show the store's forecast parameters (`--resolution`/`--interval`).           |

```sh
infrastore --store demo.h5 stats
infrastore --store demo.h5 store-info
infrastore --store demo.h5 arrays --data-hash 2018057b
infrastore --store demo.h5 summary --static-only
infrastore --store demo.h5 resolutions
infrastore --store demo.h5 params --resolution PT1H --interval PT1H
```

### Associations

| Command      | Purpose                                                                     |
| ------------ | --------------------------------------------------------------------------- |
| `attributes` | Component <-> supplemental-attribute associations (`--summary` for counts). |
| `links`      | Directed parent -> child component associations.                            |
| `attach`     | Attach supplemental attributes to components.                               |
| `detach`     | Remove attachments matching the filter.                                     |
| `link`       | Add directed parent -> child component links.                               |
| `unlink`     | Remove links matching the filter.                                           |
| `reassign`   | Move a component's associations from one id to another.                     |

```sh
infrastore --store demo.h5 attributes --component-id 42
infrastore --store demo.h5 attributes --summary
infrastore --store demo.h5 links --parent-type Bus --child-type Generator
infrastore --store demo.h5 attach --component-id 42 --component-type Generator \
    --attribute-id 7 --attribute-type GeographicInfo
infrastore --store demo.h5 attach --from attachments.csv
infrastore --store demo.h5 link --parent-id 42 --parent-type Generator \
    --child-id 7 --child-type Bus
infrastore --store demo.h5 detach --component-id 42 --dry-run
infrastore --store demo.h5 unlink --child-type Bus --force
infrastore --store demo.h5 reassign --old 42 --new 43
```

`attach --from` and `link --from` import a whole table in one all-or-nothing transaction, from a
`component_id,component_type,attribute_id,attribute_type` or
`parent_id,parent_type,child_id,child_type` CSV. The header is mandatory and its names are checked:
the four columns are two interchangeable-looking `(id, type)` pairs, so a file with the pairs
swapped would import cleanly and silently invert every relationship.

`detach` and `unlink` with no filter would empty the whole catalog, so they require `--all` to say
you meant it. `reassign` is the association counterpart of `replace-owner`, which moves time series;
with neither `--attributes` nor `--links` it moves both catalogs, which is what a renumbered
component needs.

### Integrity & maintenance

| Command             | Purpose                                                                          |
| ------------------- | -------------------------------------------------------------------------------- |
| `verify`            | Verify store integrity; nonzero exit if errors are present.                      |
| `check-consistency` | Verify the per-resolution static grid (`--resolution`).                          |
| `compact`           | Rewrite the `.h5` to reclaim space (prompts unless `--force`); print the report. |
| `persist`           | Write the store to a new HDF5 + SQLite artifact (`--dest`).                      |
| `diff`              | Compare this store against another at the catalog level (`--against`).           |

```sh
infrastore --store demo.h5 verify
infrastore --store demo.h5 check-consistency --resolution PT1H
infrastore --store demo.h5 compact --force
infrastore --store demo.h5 persist --dest backup.h5 --dry-run
infrastore --store demo.h5 persist --dest backup.h5 --force
infrastore --store demo.h5 diff --against baseline.h5
```

`persist` is the one write guarded even when the destination is explicit: a save that fails partway
may already have destroyed what was there, so replacing an existing artifact needs `--force` (or the
global `--yes`), and a non-interactive run without one stops rather than proceeding.

`diff` is the regression check for "did this model run change what I expected". Content addressing
makes it cheap — two series hold the same numbers exactly when they carry the same `data_hash` — so
the comparison is a set operation over the two catalogs and neither store's arrays are read. It
exits `1` when the stores differ, so it drops straight into a CI gate; `--all` also lists the
identical series.

### Scaffolding

| Command       | Purpose                                                 |
| ------------- | ------------------------------------------------------- |
| `template`    | Print an example descriptor for a given type to stdout. |
| `completions` | Generate shell completions to stdout (bash/zsh/fish/…). |

```sh
infrastore template SingleTimeSeries > load.json
infrastore completions zsh > ~/.zfunc/_infrastore
```

```text
infrastore --store <PATH> init [--compression <none|deflate[:LEVEL]>] [--no-shuffle] [--catalog <attached|in-memory>]
infrastore --store <PATH> add --descriptor <FILE.json|-> [--csv <FILE.csv>] [--dry-run] [--replace] [--batch-size N] [-q|--quiet] [--compression <SPEC>] [--no-shuffle] [--catalog <MODE>]
infrastore --store <PATH> add --csv <FILE.csv> --owner-id <I> --owner-type <T> --name <N> --type <T> --element-type <E> [DESCRIPTOR FIELDS...]
infrastore --store <PATH> merge --from <PATH.h5> [SELECTOR...] [--replace] [--dry-run]
infrastore --store <PATH> list    [SELECTOR...] [--limit N] [--wide]
infrastore --store <PATH> get     [SELECTOR...] [--time-range START..END] [--limit N | --full] [--tail] [--stride N] [--plot [--plot-width COLS]] [--window N | --issue-time <TS>]
infrastore --store <PATH> grid    [SELECTOR...] [--time-range START..END] [--limit N | --full] [--label <auto|owner|full>]
infrastore --store <PATH> plot    [SELECTOR...] [--out <FILE.svg|FILE.html|->] [--kind <line|duration|heatmap|fan|overlay>] [--time-range START..END] [--title <T>] [--width W] [--height H] [--window N] [--limit N]
infrastore --store <PATH> info    [SELECTOR...] [--no-stats]
infrastore --store <PATH> export  [SELECTOR...] [--dir <DIR>] [--time-range START..END]
infrastore --store <PATH> names       [SELECTOR...]
infrastore --store <PATH> owner-types [SELECTOR...]
infrastore --store <PATH> owners      [--owner-category <C>] [--type <T>] [--resolution <DUR>]
infrastore --store <PATH> exists      [SELECTOR...]
infrastore --store <PATH> diff --against <PATH.h5> [SELECTOR...] [--all]
infrastore --store <PATH> remove  [SELECTOR...] [--all] [--force] [--dry-run]
infrastore --store <PATH> rename  [SELECTOR...] --new-name <NAME> [--dry-run]
infrastore --store <PATH> copy    [SELECTOR...] --dst-owner-id <I> --dst-owner-type <T> [--new-name <NAME>] [--dry-run]
infrastore --store <PATH> replace-owner --old <I> --new <I> --owner-category <C> [--dry-run]
infrastore --store <PATH> clear   [--owner-id <I> --owner-category <C>] [--force] [--dry-run]
infrastore --store <PATH> transform --horizon <DUR> --interval <DUR> [--owner-category <C>] [--resolution <DUR>]
infrastore --store <PATH> persist --dest <PATH.h5> [--force] [--dry-run]
infrastore --store <PATH> compact [--force]
infrastore --store <PATH> stats
infrastore --store <PATH> store-info
infrastore --store <PATH> arrays [SELECTOR...] [--data-hash <HEX>]
infrastore --store <PATH> attributes [--component-id <I>] [--attribute-id <I>] [--component-type <T>] [--attribute-type <T>] [--summary]
infrastore --store <PATH> links [--parent-id <I>] [--child-id <I>] [--parent-type <T>] [--child-type <T>]
infrastore --store <PATH> attach [--component-id <I> --component-type <T> --attribute-id <I> --attribute-type <T> | --from <FILE.csv>] [--dry-run]
infrastore --store <PATH> detach [--component-id <I>] [--attribute-id <I>] [--component-type <T>] [--attribute-type <T>] [--all] [--force] [--dry-run]
infrastore --store <PATH> link   [--parent-id <I> --parent-type <T> --child-id <I> --child-type <T> | --from <FILE.csv>] [--dry-run]
infrastore --store <PATH> unlink [--parent-id <I>] [--child-id <I>] [--parent-type <T>] [--child-type <T>] [--all] [--force] [--dry-run]
infrastore --store <PATH> reassign --old <I> --new <I> [--attributes] [--links] [--dry-run]
infrastore completions <SHELL>
infrastore --store <PATH> summary [--static-only | --forecast-only]
infrastore --store <PATH> verify
infrastore --store <PATH> check-consistency [--resolution <DUR>]
infrastore --store <PATH> resolutions
infrastore --store <PATH> params [--resolution <DUR>] [--interval <DUR>]
infrastore template <SingleTimeSeries|NonSequentialTimeSeries|Deterministic|Probabilistic|Scenarios>
```

`--csv` overrides the `csv` path inside the descriptor, and only works when the descriptor is a
single object (a wide one that expands to many series included). Passing it alongside a descriptor
array holding more than one object fails with `--csv cannot be used with an array descriptor`.

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

`--dry-run` on `remove`, `clear`, `replace-owner`, `rename`, `copy`, `merge`, `persist`, `attach`,
`detach`, `link`, `unlink`, and `reassign` prints what would change and exits without opening the
store for writing.

`add --dry-run` is a validate mode: it resolves every descriptor, reads every CSV in full, and
prints the resolved `(owner, type, name, element type, shape)` table without opening the store at
all. That catches the whole class of "I got the shape wrong" errors before a multi-GB load starts.
Because the store is opened lazily, on the first batch that actually has something to write, a load
that fails validation never leaves an empty store behind.

`add --replace` removes any series that already carries one of the identities being added, inside
the same transaction, which is what makes re-running a load after fixing the data idempotent.
`add --batch-size N` commits every N series instead of the whole load in one transaction, bounding
memory for a very large load at the cost of the load's atomicity; `-q`/`--quiet` silences everything
but errors, and above 20 series the per-series lines are replaced by a progress counter on stderr.

`add --descriptor -` reads the JSON from stdin, so a generator script can pipe descriptors straight
in. Relative `csv` paths in a piped descriptor resolve against the working directory, since there is
no descriptor file for them to sit beside.

`init --compression` (or `add --compression`) sets the HDF5 compression policy for a store the
command creates (`none`, `deflate`, or `deflate:LEVEL` with `--no-shuffle` to disable byte-shuffle;
a bare `deflate` is level 3, and the default when neither is given); passing it for an existing
store is an error, since the persisted policy governs.

`--catalog` decides where the SQLite catalog lives **while the command runs**. `attached` (the
default) commits to `<store>.sqlite` as it goes, so an interrupted load keeps what it had already
written. `in-memory` holds the catalog in RAM instead of journaling every commit — much faster for a
bulk load, and it loses _everything_ if the process dies before the command finishes: arrays still
stream to the `.h5` file, but without a catalog they are unreachable.

Either way the command writes the catalog out before it exits, so the store is complete when it
returns. The modes cannot differ on that: the CLI runs one command per process, so a catalog still
in RAM at exit is not deferred, it is gone — no later `persist` could write _that_ process's
catalog.

### Selectors

`get`, `info`, and `remove` identify exactly one series with these flags; `list` accepts the same
flags as filters. Every flag is optional. Only `--feature` may be repeated; the rest take a single
value:

| Flag                    | Meaning                                                                    |
| ----------------------- | -------------------------------------------------------------------------- |
| `--owner-id <I>`        | Owner identifier (`i64` integer).                                          |
| `--owner-category <C>`  | Restrict to `Component` or `SupplementalAttribute`; omit to match either.  |
| `--name <N>`            | Series name (exact match).                                                 |
| `--name-glob <P>`       | Name pattern (SQLite `GLOB`: case-sensitive `*`/`?`). ANDed with `--name`. |
| `--component-field <F>` | Owning component's field, exact and case-sensitive.                        |
| `--type <T>`            | See the type spellings below.                                              |
| `--resolution <DUR>`    | Resolution as an ISO-8601 duration, e.g. `PT1H`, `PT15M`, `P1M`.           |
| `--feature key=value`   | Feature filter; repeatable. Values are inferred as int/float/bool/string.  |
| `--spelling <S>`        | `zoned` or `zoneless`: which timestamp spelling to keep.                   |

`--spelling` is the constructive half of the time-reference coherence rule. `zoneless` keeps the
wall-clock series; `zoned` keeps the ones that record instants, including those that declare no
reference at all. `grid` and the bulk reads span one timestamp axis, so they refuse a selection
holding both groups — this is how a store containing both is split into one they can read. It is
unrelated to the global `--zoneless`, which says how timestamps arriving on the _input_ side are to
be read.

`--component-field` selects every series that varies that field on its owner — the query the
descriptor exists for. It is descriptive rather than identifying, so it narrows a selector but
rarely resolves one on its own; and a series that declares no `component_field` matches no value, so
it cannot select the ones that left it unset. `owners` rejects it (along with `--owner-id`,
`--name`, `--name-glob`, and `--feature`) rather than silently ignoring it.

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

Both spellings are accepted as input: `--type single` and `--type SingleTimeSeries` are equivalent,
as are `--owner-category component` and `--owner-category Component` (matching is case-insensitive
and ignores underscores). Everything the CLI _emits_ uses the canonical CamelCase name — rendered
rows, `-f json` output, and the descriptors `template` prints — so those all string-match one
another. The lowercase forms are a command-line shorthand, not a second vocabulary.

## Durations and Timestamps

- **Durations** (`resolution`, `horizon`, `interval`): an **ISO-8601 duration**, and nothing else —
  `PT1H`, `PT15M`, `PT30S`, `PT0.5S`, `P1D`, `P7D` for fixed spans, `P1M` / `P3M` / `P1Y` for
  calendar ones. This is also the spelling every command _prints_, so a duration copied out of
  `list`, `info`, or `export -f json` can be pasted straight back into a descriptor.

  The human form the CLI used to accept (`1h`, `15min`, `7d`, and a bare integer meaning
  _milliseconds_) is rejected, with the ISO-8601 translation attached:
  `invalid duration '1h': durations are ISO-8601 — did you mean 'PT1H'?`
- **Timestamps** (`initial_timestamp`, non-sequential timestamp column, `--time-range` bounds,
  `--issue-time`): RFC3339 (e.g. `2024-01-01T00:00:00Z`) or a bare integer of epoch milliseconds. A
  _stored_ timestamp must be a whole number of milliseconds — a finer one is refused by `add` rather
  than truncated, and the epoch-millisecond form cannot express one at all. See
  [timestamp precision](../explanation/data-model.md#timestamp-precision). A timestamp with no
  offset needs [`--assume-timezone` or `--zoneless`](#zoneless-timestamps), and whichever one you
  pass is also recorded as the series' `time_reference`.
- **`--time-range`** is a pair of _timestamps_, not a duration: `START..END` (half-open — `START`
  inclusive, `END` exclusive), where each side is parsed as a timestamp. For example
  `--time-range 2024-01-01T01:00:00Z..2024-01-01T03:00:00Z`. A duration such as `--time-range 1h` is
  rejected with `invalid --time-range '1h' (expected START..END)`. A range bound need not be
  grid-aligned for a static series, and must be a window boundary for a forecast; see
  [reading a time range](rust-api.md#reading-a-time-range) for what each type selects.

## Descriptor Schema

A descriptor JSON file is either a single object (one series) or an array of objects (batch add).
The CSV holds only numbers, preceded by a mandatory header row (plus a leading timestamp column for
`NonSequentialTimeSeries`).

| Key                            | Required for             | Notes                                                            |
| ------------------------------ | ------------------------ | ---------------------------------------------------------------- |
| `owner_id`                     | long layout              | Integer component identifier (`i64`). Rejected when wide.        |
| `owner_type`                   | long layout              | Wide: the default for `owner_map` rows that name none.           |
| `owner_category`               | optional                 | `Component` (default) or `SupplementalAttribute`.                |
| `name`                         | all                      |                                                                  |
| `type`                         | all                      | One of the five writable types; spellings as for `--type`.       |
| `element_type`                 | all                      | `f64`/`f32`/`i64`/…, `tuple(N,f64)`, or a function-data kind.    |
| `csv`                          | unless `--csv` is passed | Path relative to the descriptor; `--csv` overrides it.           |
| `element_shape`                | optional                 | Trailing per-step dims; default scalar (`[]`).                   |
| `units`                        | optional                 | Free-form label.                                                 |
| `quantity_kind`                | optional                 | What the values measure, e.g. `ActivePower` (QUDT name).         |
| `unit_system`                  | optional                 | `natural_units` or `component_base`; unset = unspecified.        |
| `time_reference`               | optional                 | `utc` / `zoneless` / `-07:00` / an IANA name; normally inferred. |
| `component_field`              | optional                 | Owning component's field these values vary over time.            |
| `application_data`             | optional                 | Opaque package-owned payload (e.g. JSON), stored verbatim.       |
| `features`                     | optional                 | JSON object; int/float/bool/string values. See below.            |
| `initial_timestamp`            | all but non-sequential   | Also decides the series' `time_reference` unless declared.       |
| `resolution`                   | all but non-sequential   | ISO-8601 duration, e.g. `PT1H`.                                  |
| `horizon`, `interval`, `count` | forecasts                | The two durations are ISO-8601, e.g. `PT24H`.                    |
| `percentiles`                  | `Probabilistic`          | Strictly increasing list of floats.                              |
| `scenario_count`               | `Scenarios` (optional)   | Inferred from the data length if omitted.                        |
| `layout`                       | optional                 | `long` (default) or `wide`. See below.                           |
| `owner_map`                    | wide layout              | Sidecar CSV path, or an inline `{"column": owner_id}` object.    |
| `owner_id_from`                | wide layout              | `"header"` (the only value) when the headers are owner ids.      |

Unknown keys are rejected. Any key not in the table above — including a typo like `resolutionn` — is
a hard parse error listing the accepted fields, so hand-edited templates fail loudly rather than
silently dropping a setting.

Inside `features`, a name that shadows a time-series or key field (`name`, `resolution`, `owner_id`,
…) is rejected when the series is added — see
[reserved feature names](../explanation/data-model.md#reserved-feature-names).

Every field above also exists as an `add` flag, for a one-off that does not deserve a file:
`--owner-id`, `--owner-type`, `--owner-category`, `--name`, `--type`, `--element-type`, `--units`,
`--quantity-kind`, `--unit-system`, `--component-field`, `--application-data`, `--element-shape`
(repeatable), `--feature` (repeatable), `--initial-timestamp`, `--resolution`, `--horizon`,
`--interval`, `--count`, `--percentile` (repeatable), `--scenario-count`, `--layout`, `--owner-map`,
`--owner-id-from`. The inline form is a shortcut for authoring one descriptor, not a second schema —
both go down the same code path — so `--descriptor` and the inline flags cannot be combined. Keep
the descriptor as the repeatable and batch form.

### Wide layout

The canonical power-systems file is one column per component:

```text
timestamp,gen_001,gen_002,...,gen_500
2024-01-01T00:00:00Z,101.5,88.2,...,44.0
```

In the default `long` layout every value column is part of _one_ series' per-timestep element, so
loading that file would need 500 descriptors and 500 single-column CSVs. `"layout": "wide"` reads it
as 500 separate scalar series instead, sharing this descriptor's `name`, `type`, `resolution`,
`units`, `quantity_kind`, `unit_system`, `component_field`, `application_data`, and `features`, and
differing only by owner:

```json
{
  "csv": "gen_profiles.csv",
  "layout": "wide",
  "type": "SingleTimeSeries",
  "name": "max_active_power",
  "owner_type": "ThermalStandard",
  "element_type": "f64",
  "units": "MW",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H",
  "owner_map": "components.csv"
}
```

The store keys on an `i64` `owner_id` but wide headers are component _names_, so the mapping has to
be an input. There are three ways to supply it:

| Form                                | When                                             |
| ----------------------------------- | ------------------------------------------------ |
| `"owner_map": "components.csv"`     | The batch case: a `column,owner_id[,owner_type]` |
| `"owner_map": {"gen_001": 42, ...}` | A handful of columns, written inline             |
| `"owner_id_from": "header"`         | The headers already are integer owner ids        |

The sidecar CSV's header is mandatory and checked (`column,owner_id` or
`column,owner_id,owner_type`). Where a row names an `owner_type` it wins; otherwise the descriptor's
`owner_type` is the default — and it is required whenever any column lacks one, which is always the
case for the inline object form, since that carries ids only. A column with no mapping is an error
that names the unmapped columns — a 500-column load that stopped at "some column is unmapped" would
leave you diffing two files by hand. Exactly one of `owner_map` and `owner_id_from` may be set, and
either one in a `long` descriptor is an error.

A leading `timestamp` column is **required** for a wide `NonSequentialTimeSeries` (whose timestamps
are explicit rather than a grid). For a wide `SingleTimeSeries` it is optional, and when present it
is checked, not ignored — see [Reading back](#reading-back-and-re-adding). The wide layout covers
the two static types and scalar elements only: a forecast's value block is already three axes deep
before any per-column split, and a multidimensional element would need a second header row to say
which column belongs to which `(owner, element)` pair. Both are rejected rather than guessed at.

`infrastore grid` writes this same shape back out — see below.

## CSV Layout

`infrastore` computes the full array shape from the descriptor and reads the CSV's value cells in
**row-major** order to fill it. The total cell count must equal the product of the shape.

| Type                      | Shape                             | CSV                                                                    |
| ------------------------- | --------------------------------- | ---------------------------------------------------------------------- |
| `SingleTimeSeries`        | `[length, *element_shape]`        | One value column (or `prod(element_shape)` columns), one row per step. |
| `NonSequentialTimeSeries` | `[length, *element_shape]`        | First column is the timestamp, then value columns.                     |
| `Deterministic`           | `[H, count, *E]`                  | Flat row-major values; `H = horizon / resolution`.                     |
| `Probabilistic`           | `[num_percentiles, H, count, *E]` | Flat row-major values.                                                 |
| `Scenarios`               | `[scenario_count, H, count, *E]`  | Flat row-major values.                                                 |

`bool` cells accept `true`/`false`/`1`/`0`. The table above is the **write** layout: the rawest
form, which is what `template` prints and what a hand-authored CSV should look like.

**The header row is mandatory.** It carries no data, but it is the only input to the layout
detection below, and guessing wrong on a forecast transposes its axes without failing. A file whose
first row parses as values of the declared `element_type` is rejected — otherwise the CSV reader
would consume that row as column names and store the series one element short, silently:

```text
$ infrastore --store demo.h5 add --descriptor load.json
Error: the first row of load.csv (1.5) is f64 data, not a header. Every data CSV must start with a
header row — add one (e.g. `value`, or `timestamp,value`), or delete the row if it is a stray value.
```

### Reading back, and re-adding

Every CSV `infrastore` writes carries timestamps, because they are the useful part of the output and
because a piped file otherwise loses the time axis entirely — `initial_timestamp` and `resolution`
live in the catalog, not in the file.

| Type                                          | `get -f csv` / `export -f csv` header                               |
| --------------------------------------------- | ------------------------------------------------------------------- |
| `SingleTimeSeries`, `NonSequentialTimeSeries` | `timestamp,value...`                                                |
| `Deterministic`                               | `issue_time,target_time,value...`                                   |
| `Probabilistic`                               | `issue_time,target_time,value[p10],...` (one column per percentile) |
| `Scenarios`                                   | `issue_time,target_time,value[s0],...` (one column per scenario)    |

`add` reads both layouts. It picks between them from the header row, so a file written by `export`
can be handed straight back to `add` with no column surgery:

- a first column named `timestamp` is read as the time axis. For a `NonSequentialTimeSeries` it _is_
  the data; for a `SingleTimeSeries` it is **validated** against the descriptor's
  `initial_timestamp` + `resolution` grid, row count included, so a file sliced out of an export and
  re-added under the original descriptor fails loudly rather than landing on the wrong instants;
- leading `issue_time` + `target_time` columns mark the timestamped forecast layout, whose rows run
  window-major with the percentiles/scenarios spread across columns — `add` transposes them back
  into the stored `[series, horizon, count, element]` order;
- anything else is the flat write layout above.

The round trip is exact for every type, including forecasts: values come back in the order they went
in. What a CSV cannot carry is the descriptor metadata — owner, name, features, units — so re-adding
still needs a descriptor supplying those. `export -f json` carries all of it, plus `data_hash`.

## Grid: many series, one time axis

`grid` is the read-direction inverse of the wide ingest above, and the CLI surface for the core's
columnar reader. It emits one row per timestamp and one column per series:

```text
$ infrastore --store demo.h5 -f csv grid --name max_active_power --resolution PT1H
timestamp,1,2,3
2024-01-01T00:00:00+00:00,101.5,88.2,44.0
2024-01-01T01:00:00+00:00,102.1,87.4,44.6
```

A reader spans exactly **one timeline**, which is what makes the columns line up row by row without
a presence mask. For `SingleTimeSeries` that means one resolution, so `--resolution` is required;
for `NonSequentialTimeSeries` it means one shared timestamp vector, and a selection spanning two is
an error naming how many were found rather than a padded result.

Columns are named by `--label`:

| Value            | Header                                                           |
| ---------------- | ---------------------------------------------------------------- |
| `auto` (default) | The bare owner id when every column shares one series name, else |
|                  | `name@owner`.                                                    |
| `owner`          | Always the bare owner id.                                        |
| `full`           | Always `name@owner`.                                             |

The bare form is what closes the loop: a `grid` CSV is re-readable by
`add --layout wide --owner-id-from header`, and `grid → add → grid` is a fixed point. Column order
is the reader's own — groups by `(dtype, element_shape)`, keys in build order — so it is stable
across runs and two grid exports can be diffed.

## Charts

`plot` writes one **self-contained** file: no external fonts, scripts, stylesheets, or images, so it
opens in a browser, drops into a report, and survives being emailed. Both light and dark themes are
written into the document, keyed on `prefers-color-scheme`. An `.html` destination wraps the same
SVG in a minimal page; `--out -` writes to stdout, and the default is `chart.svg` in the working
directory. `--width`/`--height` default to 960 × 440 (CSS pixels, fractional values allowed);
`--limit` caps an `overlay` at 8 windows unless told otherwise.

| `--kind`   | What it shows                                                                     |
| ---------- | --------------------------------------------------------------------------------- |
| `line`     | The profile itself, one or more series against time.                              |
| `duration` | The load duration curve: values sorted descending against the percent of time at  |
|            | or above them. Standard in this field, and the fastest read on how peaky a        |
|            | profile is.                                                                       |
| `heatmap`  | Time-of-day against day. The fastest way to spot a timezone or DST error — the    |
|            | bug class this data is most prone to. A correct profile shows vertical banding; a |
|            | shifted one shows a diagonal seam.                                                |
| `fan`      | Percentile bands for a `Probabilistic`, overlaid traces for `Scenarios`, for one  |
|            | window (`--window N`). These types have no other readable rendering.              |
| `overlay`  | A `Deterministic`'s windows drawn over the `SingleTimeSeries` it was transformed  |
|            | from: forecast against actual.                                                    |

```sh
infrastore --store demo.h5 plot --name load --kind line --out load.svg
infrastore --store demo.h5 plot --name load --kind duration --out ldc.svg
infrastore --store demo.h5 plot --name load --kind heatmap --out heat.html
infrastore --store demo.h5 plot --name load_prob --type Probabilistic --kind fan --out fan.svg
infrastore --store demo.h5 plot --name load --type Deterministic --kind overlay --out fc.svg
```

The categorical palette has eight distinguishable colors, so a `line` or `duration` chart refuses a
selector matching more than eight series and points at `grid` instead — cycling colors would produce
a chart whose legend lies. `heatmap` draws one series. `Scenarios` past eight traces are drawn in
one color with the count in the legend, which is the honest reading of a spaghetti plot.

`get --plot` is the no-file version: a Unicode sparkline per element, with the series range printed
beside it. Each column shows its bucket's most extreme sample rather than its average, so a one-hour
spike in a year of hourly data still shows — the thing a sanity-check plot must not hide. The cost
is that a column is not a summary of its bucket; `plot` draws the real curve.

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

`info` reads the array to compute its statistics: `min`, `max`, `mean`, `stddev`, the `p5`/`p25`/
`p50`/`p75`/`p95` percentiles, `first`, `last`, `num_elements`, and a separate `non_finite` count (a
NaN in a load profile is a data bug, and a mean that quietly ignored it would hide the bug rather
than surface it). `--no-stats` skips all of that, leaving a purely catalog-side query that never
touches the HDF5 file.

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

| Code | Meaning                                                                          |
| ---- | -------------------------------------------------------------------------------- |
| `0`  | Success.                                                                         |
| `1`  | Runtime error. The message goes to stderr, in the `--format` that was asked for. |
| `2`  | Usage error from argument parsing (unknown flag, missing `--descriptor`, …).     |

A runtime error is `Error: <message>` under `table` and `csv`, and
`{"status": "error", "message": "<message>"}` under `json` and `jsonl` — pretty for `json`, one
compact line for `jsonl`. stdout is left empty either way.

Exit `2` is the exception: clap renders those itself, before there is a parsed `--format` to honor,
so an argument error is always prose no matter what `-f` says.

Three commands also use `1` as an _answer_ rather than a failure, so they drop into a shell
conditional or a CI gate: `verify` when the integrity report lists any error, `diff` when the two
stores differ, and `exists` when nothing matches. All three still print their result to stdout, and
a genuine failure is distinguishable by the `Error:` line on stderr.
