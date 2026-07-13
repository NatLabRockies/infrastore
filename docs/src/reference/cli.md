# CLI Reference

`time-series-store-cli` builds the `tss` binary, which reads and writes a store directly on disk
(NetCDF + SQLite). For a task-oriented walkthrough, see [Use the `tss` CLI](../how-to/use-cli.md).

## Synopsis

```text
tss [--store <PATH.nc>] [-f <FORMAT>] [--log-level <FILTER>] <COMMAND>
```

### Global options

| Option           | Description                                                             |
| ---------------- | ----------------------------------------------------------------------- |
| `--store <PATH>` | Path to the NetCDF store file. The `<PATH>.sqlite` catalog is implicit. |
| `-f`, `--format` | Output format: `table` (default), `json`, or `csv`.                     |
| `--log-level`    | Tracing filter; also read from `RUST_LOG`. Defaults to `warn`.          |

`--store` is required by every command except `template`.

`-f`/`--format` only affects the read commands (`list`, `get`, `info`). It is accepted anywhere
because it is global, but `add`, `remove`, `transform`, and `template` ignore it and always print
plain text (`template` always prints a JSON descriptor).

## Commands

| Command     | Purpose                                                                |
| ----------- | ---------------------------------------------------------------------- |
| `add`       | Add one or more series from a descriptor JSON + CSV.                   |
| `list`      | List stored series matching the selector filters.                      |
| `get`       | Read and display a single series' values.                              |
| `info`      | Show metadata plus numeric stats for a single series.                  |
| `remove`    | Delete a single series (prompts unless `--force` or non-interactive).  |
| `transform` | Derive `DeterministicSingleTimeSeries` from stored `SingleTimeSeries`. |
| `template`  | Print an example descriptor for a given type to stdout.                |

```text
tss --store <PATH> add --descriptor <FILE.json> [--csv <FILE.csv>]
tss --store <PATH> list    [SELECTOR...]
tss --store <PATH> get     [SELECTOR...] [--time-range START..END] [--limit N | --full]
tss --store <PATH> info    [SELECTOR...]
tss --store <PATH> remove  [SELECTOR...] [--force]
tss --store <PATH> transform --horizon <DUR> --interval <DUR>
tss template <single|non_sequential|deterministic|probabilistic|scenarios>
```

`--csv` overrides the `csv` path inside the descriptor, and only works for a descriptor that
describes a single series. Passing it alongside a descriptor array holding more than one object
fails with `--csv cannot be used with an array descriptor`.

`transform` takes no selector: it rewrites **every** `SingleTimeSeries` in the store, deriving a
`DeterministicSingleTimeSeries` from each. `--horizon` must not exceed the shortest stored series
(`horizon / resolution` steps must fit within its `length`), or the command fails.

### Selectors

`get`, `info`, and `remove` identify exactly one series with these flags; `list` accepts the same
flags as filters. Every flag is optional. Only `--feature` may be repeated; the rest take a single
value:

| Flag                   | Meaning                                                                    |
| ---------------------- | -------------------------------------------------------------------------- |
| `--owner-id <I>`       | Owner identifier (`i64` integer).                                          |
| `--owner-category <C>` | Restrict to `component` or `supplemental_attribute`; omit to match either. |
| `--name <N>`           | Series name.                                                               |
| `--type <T>`           | See the type spellings below.                                              |
| `--resolution <DUR>`   | Resolution, e.g. `1h`, `15min`, or ISO-8601 like `PT1H`, `P1M`.            |
| `--feature key=value`  | Feature filter; repeatable. Values are inferred as int/float/bool/string.  |

If a selector matches more than one series, `tss` errors and lists the candidates so the query can
be narrowed. The owner identity is the pair `(owner_id, owner_category)`, so a component and a
supplemental attribute may share a numeric `owner_id`; add `--owner-category` to disambiguate when
both exist.

### Type Spellings

`--type` (and the descriptor's `type` key) accepts six values. Matching is case-insensitive and
ignores underscores, so each type has a short form and a full form:

| Type                            | Accepted spellings                                      |
| ------------------------------- | ------------------------------------------------------- |
| `SingleTimeSeries`              | `single`, `SingleTimeSeries`                            |
| `NonSequentialTimeSeries`       | `non_sequential`, `NonSequentialTimeSeries`             |
| `Deterministic`                 | `deterministic`                                         |
| `DeterministicSingleTimeSeries` | `deterministic_single`, `DeterministicSingleTimeSeries` |
| `Probabilistic`                 | `probabilistic`                                         |
| `Scenarios`                     | `scenarios`                                             |

`deterministic_single` is not writable from a descriptor (use `transform`), but it _is_ selectable,
and it is often required: `transform` derives a series that shares `(owner_id, name, resolution)`
with its source `SingleTimeSeries`, so after a transform a query like
`tss get --owner-id 42 --name load` matches two series and errors. `--type single` or
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

| Key                            | Required for                | Notes                                                  |
| ------------------------------ | --------------------------- | ------------------------------------------------------ |
| `owner_id`                     | all                         | Integer component identifier (`i64`).                  |
| `owner_type`                   | all                         |                                                        |
| `owner_category`               | optional                    | `component` (default) or `supplemental_attribute`.     |
| `name`                         | all                         |                                                        |
| `type`                         | all                         | One of the five writable types.                        |
| `dtype`                        | all                         | `f64`, `f32`, `i64`, `i32`, `u64`, `bool`.             |
| `csv`                          | unless `--csv` is passed    | Path relative to the descriptor; `--csv` overrides it. |
| `has_header`                   | optional                    | Skip the first CSV row. Default `true`.                |
| `element_shape`                | optional                    | Trailing per-step dims; default scalar (`[]`).         |
| `units`                        | optional                    | Free-form label.                                       |
| `features`                     | optional                    | JSON object; int/float/bool/string values.             |
| `initial_timestamp`            | all except `non_sequential` |                                                        |
| `resolution`                   | all except `non_sequential` |                                                        |
| `horizon`, `interval`, `count` | forecasts                   |                                                        |
| `percentiles`                  | `probabilistic`             | Strictly increasing list of floats.                    |
| `scenario_count`               | `scenarios` (optional)      | Inferred from the data length if omitted.              |

Unknown keys are rejected. Any key not in the table above — including a typo like `resolutionn` — is
a hard parse error listing the accepted fields, so hand-edited templates fail loudly rather than
silently dropping a setting.

## CSV Layout

`tss` computes the full array shape from the descriptor and reads the CSV's value cells in
**row-major** order to fill it. The total cell count must equal the product of the shape.

| Type             | Shape                             | CSV                                                                    |
| ---------------- | --------------------------------- | ---------------------------------------------------------------------- |
| `single`         | `[length, *element_shape]`        | One value column (or `prod(element_shape)` columns), one row per step. |
| `non_sequential` | `[length, *element_shape]`        | First column is the timestamp, then value columns.                     |
| `deterministic`  | `[H, count, *E]`                  | Flat row-major values; `H = horizon / resolution`.                     |
| `probabilistic`  | `[num_percentiles, H, count, *E]` | Flat row-major values.                                                 |
| `scenarios`      | `[scenario_count, H, count, *E]`  | Flat row-major values.                                                 |

`bool` cells accept `true`/`false`/`1`/`0`. `get -f csv` re-emits the same layout `add` consumes, so
values round-trip. That output always starts with a header row (`value`, or `timestamp,value...` for
`non_sequential`), which is what the `has_header: true` default — and every `template` — expects.

## Exit Status

| Code | Meaning                                                                      |
| ---- | ---------------------------------------------------------------------------- |
| `0`  | Success.                                                                     |
| `1`  | Runtime error. The message is printed to stderr, prefixed with `Error:`.     |
| `2`  | Usage error from argument parsing (unknown flag, missing `--descriptor`, …). |
