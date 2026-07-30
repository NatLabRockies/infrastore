# Use the `infrastore` CLI

`infrastore` loads time series from CSV files and inspects a store, talking directly to the on-disk
`.h5` + `.h5.sqlite` pair (no gRPC server required). For the full command and descriptor reference,
see [CLI Reference](../reference/cli.md).

## 1. Build the Binary

```sh
cargo build -p infrastore-cli      # debug build at target/debug/infrastore
# or a release build:
cargo build -p infrastore-cli --release
```

The examples below assume `infrastore` is on your `PATH` (or use `./target/debug/infrastore`).

## 2. Describe the Data

Numeric values live in a CSV; everything that does not fit a flat grid (owner, name, type, dtype,
resolution, initial timestamp, units, features) lives in a **descriptor JSON**. Print a starting
point for any type with `template`:

```sh
infrastore template single > load.json       # print an example descriptor to edit
```

Edit it to point at your data and metadata:

```json
{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "component",
  "name": "load",
  "type": "single",
  "dtype": "f64",
  "units": "MW",
  "csv": "load.csv",
  "has_header": true,
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "1h",
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

The descriptor rejects unknown keys, so a typo (`resolutionn`) is a hard error rather than a
silently ignored setting.

## 3. Add It to a Store

```sh
infrastore --store demo.h5 add --descriptor load.json
```

The store (`demo.h5` and its `demo.h5.sqlite` catalog) is created on first `add`. A descriptor may
also be a JSON array of objects to add many series in one transaction. `--csv` overrides the
descriptor's `csv` path, but only for a single-series descriptor: with an array of two or more
objects it errors (`--csv cannot be used with an array descriptor`).

## 4. Read It Back

`infrastore` follows an output convention: a global `-f/--format` with `table` (default), `json`,
and `csv`. Only the read commands (`list`, `get`, `info`) honor it; `add`, `remove`, `transform`,
and `template` accept the flag but ignore it and print plain text.

```sh
infrastore --store demo.h5 list                                       # what's in the store
infrastore --store demo.h5 list --name-glob 'load_*'                  # name pattern (SQLite GLOB)
infrastore --store demo.h5 get  --owner-id 42 --name load             # pretty table
infrastore --store demo.h5 -f csv  get  --owner-id 42 --name load     # round-trippable CSV
infrastore --store demo.h5 -f json info --owner-id 42 --name load     # metadata + stats
infrastore --store demo.h5 -f csv  export --dir out/                  # one file per series
```

`export` is the bulk read-direction inverse of `add`: every series the selector matches is written
to its own CSV or JSON file under `--dir` (or to stdout when exactly one matches). Setting
`INFRASTORE_STORE` in the environment stands in for `--store`, and destructive commands (`remove`,
`clear`, `replace-owner`, `rename`, `copy`) accept `--dry-run` to preview their effect.

`info` reports metadata plus stats over the values: `min`/`max`/`mean` for numeric dtypes, or
`true_count`/`false_count` when `dtype` is `bool`, and always `num_elements`.

`get`/`info`/`remove` select a single series with `--owner-id`, `--owner-category`, `--name`,
`--type`, `--resolution`, and repeated `--feature key=value` (`--feature` is the only repeatable
one); if more than one series matches, `infrastore` lists the candidates so you can narrow the
query. The owner is the `(owner_id, owner_category)` pair, so a component and a supplemental
attribute may share a numeric id — add `--owner-category` (`component` / `supplemental_attribute`)
to disambiguate. Large series truncate in `table` output — pass `--limit N` or `--full`.

`--time-range START..END` on `get` takes two _timestamps_ (RFC3339 or epoch-ms), not a duration:

```sh
infrastore --store demo.h5 get --owner-id 42 --name load \
  --time-range 2024-01-01T01:00:00Z..2024-01-01T03:00:00Z
```

Beware that inputs and outputs are spelled differently. You type the short, lowercase forms
(`--type single`, `--owner-category component`), but `list`/`get`/`info` print the canonical
CamelCase names (`SingleTimeSeries`, `Component`). Both spellings are accepted as input, so
`-f json list` output can be fed back into a selector unchanged; just don't expect the rendered
value to string-match what you typed.

## 5. Forecasts

All five writable types work (`single`, `non_sequential`, `deterministic`, `probabilistic`,
`scenarios`). `infrastore template deterministic` prints a descriptor to edit, but it is plain JSON
and says nothing about the data layout, so here is the rule:

Forecast CSVs are a flat, **row-major** stream of values with no structure of their own. The count
must equal the product of the type's shape:

| Type            | Shape                             |
| --------------- | --------------------------------- |
| `deterministic` | `[H, count, *element_shape]`      |
| `probabilistic` | `[num_percentiles, H, count, *E]` |
| `scenarios`     | `[scenario_count, H, count, *E]`  |

`H = horizon / resolution` — with the template's `"horizon": "24h"`, `"resolution": "1h"`, and
`"count": 7`, a scalar `deterministic` needs exactly `24 * 7 = 168` values (plus the header row that
`has_header: true` skips). Use `-f json` to read the flat values back at full fidelity. `get -f csv`
on a forecast emits **timestamped analysis rows** instead — one row per `(window, step)` with
`issue_time`/`target_time` columns and one value column per percentile or scenario — so it is not
re-ingestible by `add` (static series' `get -f csv` still round-trips).

`DeterministicSingleTimeSeries` is not added from CSV — store a `SingleTimeSeries`, then derive it.
`transform` takes **no selector**: it rewrites _every_ `SingleTimeSeries` in the store. `--horizon`
must fit inside each one (`horizon / resolution` steps must not exceed its `length`), so with the
6-row hourly `load` above, a 24-hour horizon fails and a 3-hour one works:

```sh
infrastore --store demo.h5 transform --horizon 3h --interval 1h
```

The derived series keeps the source's owner, name, and resolution, so `load` now matches two entries
and a bare selector becomes ambiguous. Disambiguate with `--type`:

```sh
infrastore --store demo.h5 get --owner-id 42 --name load --type single
infrastore --store demo.h5 get --owner-id 42 --name load --type deterministic_single
```

## Notes

- The CLI writes locally; there is no remote/gRPC mode yet (store access is isolated so one can be
  added later).
- Output is colored (green table headers) only when stdout is a terminal; it is plain when
  piped/redirected or when `NO_COLOR` is set, so `-f json`/`-f csv` stay clean for other tools.
- `--log-level` (or `RUST_LOG`) controls logging; the default is quiet (`warn`).
- The `.h5` and `.h5.sqlite` files are one artifact — move, copy, and delete them together.
