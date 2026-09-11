# CLI Developer Guide

`infrastore` loads time series from CSV files and inspects a store, talking directly to the on-disk
`.h5` + `.h5.sqlite` pair — no gRPC server, and unlike the server it is **not** read-only. This
guide walks the whole workflow; for every flag, the descriptor schema, and the CSV layouts, see the
[CLI Reference](../reference/cli.md). For a 60-second round trip, see the
[CLI Quick Start](../getting-started/quick-start-cli.md).

It is also the fastest way to see what a consumer package actually wrote: `infrastore list` and
`infrastore info` read any artifact your package produces, and `infrastore diff` compares two by
hash without reading arrays.

## Install

Grab a prebuilt archive from the
[Releases page](https://github.com/NatLabRockies/infrastore/releases) — the executables inside are
statically linked against HDF5, so there is nothing else to install:

```sh
VERSION=v0.13.1    # pick a release from the Releases page
curl -fsSLO https://github.com/NatLabRockies/infrastore/releases/download/$VERSION/infrastore-aarch64-apple-darwin.tar.gz
tar xzf infrastore-aarch64-apple-darwin.tar.gz
```

Or install from crates.io, which builds HDF5 from source and so needs `cmake` and a C compiler:

```sh
cargo install infrastore-cli       # installs the `infrastore` binary
```

See [Installation](../getting-started/installation.md#the-infrastore-cli) for the per-platform
archive list and checksum verification.

Working in a checkout instead:

```sh
cargo build -p infrastore-cli      # debug build at target/debug/infrastore
cargo build -p infrastore-cli --release
```

The examples below assume `infrastore` is on your `PATH` (or use `./target/debug/infrastore`).

## Describe the Data

Numeric values live in a CSV; everything that does not fit a flat grid (owner, name, type,
element_type, resolution, initial timestamp, units, features) lives in a **descriptor JSON**. Print
a starting point for any type with `template`:

```sh
infrastore template SingleTimeSeries > load.json   # print an example descriptor to edit
```

Edit it to point at your data and metadata:

```json
{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "Component",
  "name": "load",
  "type": "SingleTimeSeries",
  "element_type": "f64",
  "units": "MW",
  "csv": "load.csv",
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "PT1H",
  "features": {
    "model_year": 2030
  }
}
```

```text
# load.csv
value
100.0
101.5
103.0
104.2
102.8
101.0
```

**Every data CSV needs a header row.** It is not decoration: `add` reads it to tell a hand-written
value-only file from one `export` wrote (see
[Reading back, and re-adding](../reference/cli.md#reading-back-and-re-adding)). A file whose first
row is data is rejected rather than quietly losing that row to the header.

The descriptor rejects unknown keys, so a typo (`resolutionn`) is a hard error rather than a
silently ignored setting.

**Timestamps must name an instant.** A `timestamp` column (or `initial_timestamp`) written as
`2024-01-01 00:00:00` — no offset, the way most spreadsheets and databases export — is rejected,
because it names no instant. Rather than rewrite the file, say what zone it was written in with the
global `--assume-timezone` (`UTC`, or a fixed offset like `-07:00`; named zones such as
`America/Denver` are deliberately not accepted, because a DST fold would make some rows ambiguous):

```sh
infrastore --store demo.h5 --assume-timezone UTC add --descriptor load.json
```

A timestamp that carries its own offset is never overridden. See
[Zoneless timestamps](../reference/cli.md#zoneless-timestamps).

## Add It to a Store

```sh
infrastore --store demo.h5 add --descriptor load.json --dry-run   # check first
infrastore --store demo.h5 add --descriptor load.json
```

The store (`demo.h5` and its `demo.h5.sqlite` catalog) is created on first `add`, or explicitly with
`init` when you want to pin a compression policy up front:

```sh
infrastore --store demo.h5 init --compression deflate:6                 # default is deflate:3
infrastore --store demo.h5 init --compression none --catalog in-memory   # see below
```

A descriptor may also be a JSON array of objects to add many series in one transaction. `--csv`
overrides the descriptor's `csv` path, but only when the descriptor is a single object: with an
array of two or more it errors (`--csv cannot be used with an array descriptor`).

`--dry-run` is worth running first on anything large. It resolves every descriptor and reads every
CSV in full, then prints the resolved `(owner, type, name, element type, shape)` table without
opening the store — which catches the "I got the shape wrong" class of mistake before a multi-GB
load starts. `--replace` makes a re-run after fixing the data idempotent, and `--descriptor -` reads
the JSON from stdin so a generator script can pipe descriptors straight in:

```sh
infrastore --store demo.h5 add --descriptor batch.json --replace
generate.py | infrastore --store demo.h5 add --descriptor - --quiet
```

For a load too large to hold in one transaction, `--batch-size N` commits every `N` series (at the
cost of the load's atomicity), and `--catalog in-memory` skips the per-commit journaling of the
SQLite catalog while the command runs — the CLI writes it out at the end of the command either way:

```sh
infrastore --store demo.h5 add --descriptor batch.json --batch-size 500 --catalog in-memory
```

Every command carries worked examples in its own help — `infrastore add --help` — and
`infrastore --help` is the grouped index.

For a one-off, the descriptor fields are also `add` flags:

```sh
infrastore --store demo.h5 add --csv load.csv --owner-id 42 --owner-type Generator \
  --name load --type SingleTimeSeries --element-type f64 --units MW \
  --resolution PT1H --initial-timestamp 2024-01-01T00:00:00Z
```

### One file, many components

The canonical power-systems CSV is one column per component, which is the opposite shape from the
descriptor's one-object-one-series default:

```text
# gen_profiles.csv
timestamp,gen_001,gen_002,gen_003
2024-01-01T00:00:00Z,101.5,88.2,44.0
2024-01-01T01:00:00Z,102.1,87.4,44.6
```

`"layout": "wide"` loads that as one scalar series per column. The store keys on an integer
`owner_id` while the headers are component _names_, so the mapping is an input — a
`column,owner_id[,owner_type]` sidecar CSV, an inline object, or `"owner_id_from": "header"` when
the headers already are ids:

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

`infrastore grid` writes that same shape back out, so the two are an inverse pair — see below.

## Read It Back

`infrastore` follows an output convention: a global `-f/--format` with `table` (default), `json`,
`jsonl`, and `csv`. The read commands render their results in it; the write commands (`add`,
`remove`, `transform`, …) report their outcome in it — prose under `table`, a one-object status
document such as `{"added": 3, "store": "demo.h5"}` under `json`/`jsonl` — so a scripted mutation
pipes into `jq` the same way a query does. Only `template` ignores it. `jsonl` is `json`
line-delimited — one compact object per line, which streams into `jq` where a single pretty array
cannot. Under `-f json` an error is a `{"status": "error", "message": …}` document on stderr.

Before you can write a selector you need to know what values exist, which is what the discovery
commands are for:

```sh
infrastore --store demo.h5 names                                      # distinct series names
infrastore --store demo.h5 owner-types                                # distinct owner types
infrastore --store demo.h5 owners --type SingleTimeSeries             # distinct owner ids
infrastore --store demo.h5 exists --name load                         # exit 0 = yes, 1 = no
```

```sh
infrastore --store demo.h5 list                                       # what's in the store
infrastore --store demo.h5 list --name-glob 'load_*'                  # name pattern (SQLite GLOB)
infrastore --store demo.h5 list --limit 20 --wide                     # bounded, all columns
infrastore --store demo.h5 get  --owner-id 42 --name load             # pretty table
infrastore --store demo.h5 get  --name load --tail --limit 24         # the last day
infrastore --store demo.h5 get  --name load --plot                    # a terminal sparkline
infrastore --store demo.h5 -f csv  get  --owner-id 42 --name load     # timestamped CSV
infrastore --store demo.h5 -f jsonl list                              # one JSON object per line
infrastore --store demo.h5 -f json info --owner-id 42 --name load     # metadata + hash + stats
infrastore --store demo.h5 -f csv  export --dir out/                  # one file per series
```

To see many series side by side against one time axis — the read-direction inverse of the wide
ingest above — use `grid`:

```sh
infrastore --store demo.h5 -f csv grid --name max_active_power --resolution PT1H
```

Every column in a grid shares one timeline, which is what makes the rows line up without a presence
mask; that is why `SingleTimeSeries` needs `--resolution`. When every column shares one series name
the headers are bare owner ids, which is exactly the wide form `add` reads back.

When the matched series do **not** share one — a year of load beside a shorter schedule — the build
fails, naming the series that diverges. `--window-start` sweeps a span you name instead, with each
column reading at an offset of its own:

```sh
infrastore --store demo.h5 -f csv grid --resolution PT1H --window-start 2024-01-01T02:00:00Z
```

Add `--window-length N` to pin the extent; without it the sweep runs as far as every matched series
reaches. A series that does not cover the span is an error naming it, never a column quietly left
out of the table.

Sometimes the odd series out should not be a column at all. Then select one grid instead — with
`--resolution`, the `--initial-timestamp` and `--length` selectors name a whole grid, and every
series not on it drops out:

```sh
infrastore --store demo.h5 -f csv grid --resolution PT1H \
    --initial-timestamp 2024-01-01T02:00:00Z --length 6
```

They are ordinary selectors, so `list`, `remove`, and the rest take them too — which is how you find
the grids a store holds (`list --length 24`) and retire a stray cohort without naming its owners.

`export` is the bulk read-direction inverse of `add`: every series the selector matches is written
to its own CSV or JSON file under `--dir` (or to stdout when exactly one matches), optionally sliced
with `--time-range`; `-f parquet` instead writes
[one file pair per partition](#hand-it-to-something-else-parquet). Setting `INFRASTORE_STORE` in the
environment stands in for `--store`, every destructive command except `compact` accepts `--dry-run`
to preview its effect, and the global `-y`/`--yes` answers every confirmation prompt so a script
does not have to know which commands ask:

```sh
export INFRASTORE_STORE=demo.h5
infrastore remove --name-glob 'scratch_*' --dry-run      # what would go
infrastore -y remove --name-glob 'scratch_*'             # no prompt
infrastore -f csv export --name load --time-range 2024-01-01T00:00:00Z..2024-01-08T00:00:00Z
```

`info` reports metadata, the array's content hash and where it lives in the HDF5 file, and stats
over the values: `min`/`max`/`mean`/`stddev`, the `p5`–`p95` percentiles, `first`/`last`, and a
separate `non_finite` count — or `true_count`/`false_count` when the dtype is `bool`. The stats are
the only part that reads the array — `--no-stats` skips it for a purely catalog-side query.

`list` shows every field that is part of a series' identity, features included, so two series that
differ only by a feature never render as the same row. Its `Hash` column is the first 12 characters
of the array's content hash: rows with equal hashes share one array on disk.

`get`/`info`/`remove` select a single series with `--owner-id`, `--owner-category`, `--name`,
`--name-glob`, `--component-field`, `--type`, `--resolution`, and repeated `--feature key=value`
(`--feature` is the only repeatable one); if more than one series matches, `infrastore` lists the
candidates so you can narrow the query. The owner is the `(owner_id, owner_category)` pair, so a
component and a supplemental attribute may share a numeric id — add `--owner-category` (`Component`
/ `SupplementalAttribute`) to disambiguate. Large series truncate in `table` output — pass
`--limit N`, `--full`, or `--tail` to read from the end. `--stride N` keeps every Nth row and,
unlike the display bounds, applies to a `-f csv` pipe too: it selects data rather than shortening a
view, and a silently short pipe is a bug in whatever consumes it.

`--time-range START..END` on `get` takes two _timestamps_ (RFC3339 or epoch-ms), not a duration:

```sh
infrastore --store demo.h5 get --owner-id 42 --name load \
  --time-range 2024-01-01T01:00:00Z..2024-01-01T03:00:00Z
```

Selectors accept either spelling: `--type single` and `--type SingleTimeSeries` mean the same thing,
as do `--owner-category component` and `--owner-category Component`. What the CLI _prints_ — in
`list`/`get`/`info` output and in what `template` writes — is always the canonical CamelCase name,
so descriptors, rendered rows, and `-f json` output all string-match each other. The lowercase forms
are a typing shortcut on the command line, not a second vocabulary.

## Chart It

```sh
infrastore --store demo.h5 get --name load --plot                        # sparkline, no file
infrastore --store demo.h5 plot --name load --out load.svg               # the profile
infrastore --store demo.h5 plot --name load --kind duration --out ldc.svg
infrastore --store demo.h5 plot --name load --kind heatmap --out heat.html
infrastore --store demo.h5 plot --name load_prob --type Probabilistic --kind fan --window 0 --out fan.svg
infrastore --store demo.h5 plot --name load --type Deterministic --kind overlay --out forecast.svg
```

`plot` writes one self-contained file — no external fonts, scripts, or images, and both light and
dark themes inside it — so it opens in a browser and drops into a report. The five `--kind` values
are `line`, `duration` (the load duration curve), `heatmap` (time-of-day against day, which is how
you catch a timezone or DST shift), `fan` (percentile bands or scenario traces for one forecast
window), and `overlay` (a `Deterministic`'s windows over the actuals it came from).

## Find the Bytes on Disk

Arrays are content-addressed, so identical values are stored once and shared. `arrays` shows what
collapsed onto what, and where each array actually lives:

```sh
infrastore --store demo.h5 store-info    # both file paths, format version, compression
infrastore --store demo.h5 arrays        # one row per distinct array + the series sharing it
infrastore --store demo.h5 arrays --data-hash 2018057b   # narrow to one (any prefix, any case)
```

`info` resolves a single series the same way, reporting `data_hash`, `hdf5_dataset`, and
`hdf5_column`. You need all three to open the data with an outside tool: a packed array is one
_column_ of a dataset shared with other same-shaped arrays, and a packed dataset that fills up
spills into suffixed siblings, so neither the column nor the dataset name can be worked out from
metadata alone.

Opening the catalog directly, use the `time_series_readable` view — `sqlite3` prints the raw `BLOB`
hashes as garbage bytes, and in `.mode box` it mangles the table borders:

```sh
sqlite3 demo.h5.sqlite 'SELECT name, data_hash FROM time_series_readable;'
```

## Hand It to Something Else: Parquet

CSV is the interchange format `add` and `export` default to, and it has one real cost: a float goes
out as decimal text and comes back as whatever parsing that text gives. Parquet does not have that
problem, and every analysis tool worth the name reads it:

```sh
infrastore --store demo.h5 -f parquet export --name-glob 'load_*' --dir parquet/
infrastore --store other.h5 add --parquet parquet/
```

What comes out is a handful of **partitions**, not a file per series -- a store with thousands of
series would otherwise become thousands of files. Each partition is a **pair**:

```text
parquet/
  SingleTimeSeries.f64.utc.values.parquet            every distinct array once, one row per value
  SingleTimeSeries.f64.utc.series.parquet            one catalog row per series
  SingleTimeSeries.f64.America_Denver.values.parquet
  SingleTimeSeries.f64.America_Denver.series.parquet
  Deterministic.f64.utc.values.parquet
  Deterministic.f64.utc.series.parquet
```

One pair per `(type, value type, time reference)` triple, because those three cannot vary inside one
table without nullable or ill-typed columns -- a forecast has an `issue_time` and a static series
does not, and a table has one Arrow type per column. The payoff is that **every column is
required**, which is worth more to whoever queries the files than the files it costs.

The split is the other half of it. The store is content-addressed, so a thousand components sharing
one profile hold **one** array; writing the catalog row beside every value would write that profile
a thousand times, and Parquet's compression does not find repeats across pages. So the values file
holds each array once, keyed by the pair `(data_hash, time_axis)`, and the series file carries that
same pair beside each catalog row. They join on it.

`add --parquet` takes a file, a whole directory, or a partition stem, and commits one transaction
per partition, so a partition that fails leaves the ones already committed alone. A pair `export`
wrote re-adds with no other flag; a foreign file -- one from a dataframe, or a values file whose
partner was not copied -- carries less, and is told what it is missing:

```sh
infrastore --store demo.h5 add --parquet from_pandas.parquet \
    --owner-id 42 --owner-type Generator --name load
```

Three things to know. `-f parquet` requires `--dir`, because Parquet's footer sits at the end of the
file and a writer has to seek back to it -- a pipe cannot. The `data_hash` half of the key is a
checksum on the way back in: if you edited values in DuckDB, recompute it or pass `--no-checksum`.
And a series with no values **fails** the export, naming every one -- an empty series would be a
catalog row whose key matches no values rows, which is also what a truncated export looks like.

The full layout -- both column sets, the array key, the partition rules, the filenames, the footer,
the merge join, and what a foreign file has to supply -- is in
[Parquet Layout](../reference/parquet-format.md).

Parquet is on by default, in the released binaries and in `cargo install infrastore-cli` alike. It
_is_ a cargo feature, so `--no-default-features --features vendored` builds a binary without the
Arrow dependency tree; that binary still accepts the flags and tells you which feature to rebuild
with, rather than reporting `parquet` as an unknown format.

### Querying it with DuckDB

The reason to write Parquet rather than to embed a query engine here. Every question starts with the
same join, on the pair that keys both halves:

```sql
SELECT s.name, s.owner_id, s.units, max(v.value) AS peak
FROM 'parquet/SingleTimeSeries.f64.utc.values.parquet' v
JOIN 'parquet/SingleTimeSeries.f64.utc.series.parquet' s USING (data_hash, time_axis)
GROUP BY s.name, s.owner_id, s.units;
```

Write it once as a view and nothing after has to think about it:

```sql
CREATE VIEW load AS
SELECT s.*, v.timestamp, v.value
FROM 'parquet/SingleTimeSeries.f64.America_Denver.values.parquet' v
JOIN 'parquet/SingleTimeSeries.f64.America_Denver.series.parquet' s USING (data_hash, time_axis);

-- One component's day, in its own spelling -- the timestamp column is zoned.
SELECT timestamp, value FROM load
WHERE owner_id = 42 AND name = 'load'
ORDER BY timestamp;
```

That view is the denormalized table the format deliberately does not write. Materializing it costs
exactly what the split saves; keeping it as a view costs nothing.

A forecast joins the same way, and its window is a column of the values half:

```sql
SELECT v.issue_time, count(*) AS steps, max(v.value) AS peak
FROM 'parquet/Deterministic.f64.utc.values.parquet' v
JOIN 'parquet/Deterministic.f64.utc.series.parquet' s USING (data_hash, time_axis)
WHERE s.name = 'load_det'
GROUP BY v.issue_time
ORDER BY v.issue_time;
```

Note what a glob does _not_ mix: `'parquet/*.values.parquet'` only works across partitions whose
columns agree, which is to say within one `time_series_type`. Reading a static partition and a
forecast one together needs the columns named, since only the latter has `issue_time`.

The catalog is still there when you want what the files leave out -- the `data_hash` in its binary
form, the feature sets, the association tables:

```sql
INSTALL sqlite; LOAD sqlite;
ATTACH 'demo.h5.sqlite' AS catalog (TYPE sqlite);

-- Two joins: the array key pairs the halves, then `id` reaches the catalog.
SELECT s.name, s.owner_id, max(v.value) AS peak, c.timestamps_hash
FROM 'parquet/SingleTimeSeries.f64.utc.values.parquet' v
JOIN 'parquet/SingleTimeSeries.f64.utc.series.parquet' s USING (data_hash, time_axis)
JOIN catalog.time_series_readable AS c ON c.id = s.id
GROUP BY s.name, s.owner_id, c.timestamps_hash;
```

`time_series_readable` is the catalog's hand-inspection view -- it hex-encodes the two content
hashes and decodes the integer type codes, so the rows read as text (see
[Reading the SQLite catalog by hand](../reference/cli.md#reading-the-sqlite-catalog-by-hand)). The
`id` column on the **series** half is what reaches it; it is provenance only, and `add` assigns
fresh ids rather than reusing it.

## Stamp Provenance on the Artifact

A store built by a model run should say so. `store-attr` records free-form key/value provenance
about the whole artifact -- who built it, from what source system, under which of your own schema
versions:

```sh
infrastore --store demo.h5 store-attr set creator sienna-build
infrastore --store demo.h5 store-attr set source_system WECC-2032-ADS
infrastore --store demo.h5 store-attr list
infrastore --store demo.h5 store-attr get creator     # bare value on stdout
infrastore --store demo.h5 store-attr remove creator
```

The store never interprets a value, so structure rides in the text -- store JSON if you need it. The
attributes live in the catalog, which means they travel with `persist`, survive `compact`, and show
up under `store_attributes` in `infrastore -f json store-info`.

Three things to know. `set` replaces rather than appending: an artifact records one creator, not a
history of them. `get` exits 1 when the key is unset, so a script can branch on it, while `remove`
reports `removed: false` and exits 0. And keys beginning with `infrastore.` are reserved.

`merge` brings a source's attributes across without overwriting: a key the destination lacks is
copied, a key both sides agree on is a no-op, and a disagreement is reported and left as the
destination has it. `diff` gives them a section of their own and gates on them -- an artifact whose
recorded source system changed is not the artifact you expected, even when every series is
identical.

Note the name. `attributes` (below) is a different command about a different thing.

## Associations

Two association catalogs live alongside the time series, readable and writable here:

```sh
infrastore --store demo.h5 attributes                 # component <-> supplemental attribute
infrastore --store demo.h5 attributes --summary       # counts by (component type, attribute type)
infrastore --store demo.h5 links --parent-id 42       # directed parent -> child edges
infrastore --store demo.h5 attach --from attachments.csv
infrastore --store demo.h5 link --parent-id 42 --parent-type Generator \
  --child-id 7 --child-type Bus
infrastore --store demo.h5 reassign --old 42 --new 43 # both catalogs follow a renumbered component
```

The store holds only the _relationship_ — the components and attributes themselves live in the
consumer's object graph — so the flags are bare ids and type names. `attach --from` and
`link --from` import a whole table in one all-or-nothing transaction; their header is checked,
because the four columns are two interchangeable-looking `(id, type)` pairs and a swapped file would
silently invert every relationship. `detach` and `unlink` are the inverses, and take `--dry-run`.

## Forecasts

All six writable types work (`SingleTimeSeries`, `NonSequentialTimeSeries`, `PersistentTimeSeries`,
`Deterministic`, `Probabilistic`, `Scenarios`). `infrastore template Deterministic` prints a
descriptor to edit, but it is plain JSON and says nothing about the data layout, so here is the
rule:

Forecast CSVs are a flat, **row-major** stream of values with no structure of their own. The count
must equal the product of the type's shape:

| Type            | Shape                             |
| --------------- | --------------------------------- |
| `Deterministic` | `[H, count, *element_shape]`      |
| `Probabilistic` | `[num_percentiles, H, count, *E]` |
| `Scenarios`     | `[scenario_count, H, count, *E]`  |

`H = horizon / resolution` — with the template's `"horizon": "PT24H"`, `"resolution": "PT1H"`, and
`"count": 7`, a scalar `Deterministic` needs exactly `24 * 7 = 168` values, plus the header row. Use
`-f json` to read the flat values back at full fidelity. `get -f csv` and `export -f csv` on a
forecast emit **timestamped analysis rows** instead — one row per `(window, step)` with
`issue_time`/`target_time` columns and one value column per percentile or scenario. `add` recognizes
that header too and transposes the rows back, so an exported forecast re-adds exactly (see
[Reading back, and re-adding](../reference/cli.md#reading-back-and-re-adding)).

`DeterministicSingleTimeSeries` is not added from CSV — store a `SingleTimeSeries`, then derive it.
`transform` takes **no selector**: it rewrites _every_ `SingleTimeSeries` in the store. `--horizon`
must fit inside each one (`horizon / resolution` steps must not exceed its `length`), so with the
6-row hourly `load` above, a 24-hour horizon fails and a 3-hour one works:

```sh
infrastore --store demo.h5 transform --horizon PT3H --interval PT1H
```

The derived series keeps the source's owner, name, and resolution, so `load` now matches two entries
and a bare selector becomes ambiguous. Disambiguate with `--type`:

```sh
infrastore --store demo.h5 get --owner-id 42 --name load --type single
infrastore --store demo.h5 get --owner-id 42 --name load --type deterministic_single
```

A forecast's table view is the same structured one `-f csv` writes — `issue_time`, `target_time`,
and one column per percentile or scenario — and `--window N` (or `--issue-time <TS>`) narrows it to
a single window instead of dumping all of them:

```sh
infrastore --store demo.h5 get --name load --type deterministic_single --window 0
```

## Compare and Move Stores

```sh
infrastore --store run.h5 diff --against baseline.h5      # exits 1 when they differ
infrastore --store demo.h5 merge --from other.h5 --name-glob 'load_*'
infrastore --store demo.h5 persist --dest backup.h5 --force
```

`diff` compares catalog identities and content hashes without reading either store's arrays, which
makes it cheap enough for a CI gate: two series hold the same numbers exactly when they carry the
same hash. `merge` moves arrays as bytes, so nothing is lost to a CSV round trip. `persist` is the
one write that refuses an existing destination without `--force`: a save that fails partway may
already have destroyed what was there.

## Maintain It

```sh
infrastore --store demo.h5 verify                 # re-hash every array; exit 1 on a mismatch
infrastore --store demo.h5 check-consistency      # every SingleTimeSeries of a resolution on one grid
infrastore --store demo.h5 stats                  # association, owner, and distinct-array counts
infrastore --store demo.h5 remove --owner-id 42 --name load --dry-run
infrastore --store demo.h5 compact --force        # rewrite the .h5 so deletions actually shrink it
```

Deleting frees a column or unlinks a dataset, but HDF5 cannot give the space back in place, so the
file only shrinks when `compact` rewrites it from the live set — nothing else may have the store
open while it runs, and it is the one destructive command with no `--dry-run`. `verify` and `diff`
use exit status `1` as an answer, not a failure; `2` is a usage error
([Exit Status](../reference/cli.md#exit-status)).

Shell completion for bash, zsh, fish, elvish, and PowerShell comes from the binary itself:

```sh
infrastore completions zsh > "${fpath[1]}/_infrastore"
```

## Notes

- The CLI writes locally; there is no remote/gRPC mode yet (store access is isolated so one can be
  added later).
- Output is colored (green table headers) only when stdout is a terminal; it is plain when
  piped/redirected or when `NO_COLOR` is set, so `-f json`/`-f csv` stay clean for other tools.
- `--log-level` (or `RUST_LOG`) controls logging; the default is quiet (`warn`).
- The `.h5` and `.h5.sqlite` files are one artifact — move, copy, and delete them together.
