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

### Selectors

`get`, `info`, and `remove` identify one series with these repeatable/optional flags; `list` accepts
the same flags as filters:

| Flag                  | Meaning                                                                    |
| --------------------- | -------------------------------------------------------------------------- |
| `--owner-uuid <U>`    | Owner UUID.                                                                |
| `--name <N>`          | Series name.                                                               |
| `--type <T>`          | `single`, `non_sequential`, `deterministic`, `probabilistic`, `scenarios`. |
| `--resolution <DUR>`  | Resolution, e.g. `1h`, `15min`.                                            |
| `--feature key=value` | Feature filter; repeatable. Values are inferred as int/float/bool/string.  |

If a selector matches more than one series, `tss` errors and lists the candidates so the query can
be narrowed.

## Durations and Timestamps

- **Durations** (`resolution`, `horizon`, `interval`, `--time-range`): an integer plus a unit —
  `ms`, `s`, `min`, `h`, `d` (e.g. `500ms`, `15min`, `24h`, `7d`). A bare integer is milliseconds.
- **Timestamps** (`initial_timestamp`, non-sequential timestamp column, `--time-range`): RFC3339
  (e.g. `2024-01-01T00:00:00Z`) or a bare integer of epoch milliseconds.

## Descriptor Schema

A descriptor JSON file is either a single object (one series) or an array of objects (batch add).
The CSV holds only numbers (plus a leading timestamp column for `non_sequential`).

| Key                            | Required for                | Notes                                               |
| ------------------------------ | --------------------------- | --------------------------------------------------- |
| `owner_uuid`                   | all                         |                                                     |
| `owner_type`                   | all                         |                                                     |
| `owner_category`               | optional                    | `component` (default) or `supplemental_attribute`.  |
| `name`                         | all                         |                                                     |
| `type`                         | all                         | One of the five writable types.                     |
| `dtype`                        | all                         | `f64`, `f32`, `i64`, `i32`, `u64`, `bool`.          |
| `csv`                          | all                         | Path relative to the descriptor; `--csv` overrides. |
| `has_header`                   | optional                    | Skip the first CSV row. Default `true`.             |
| `element_shape`                | optional                    | Trailing per-step dims; default scalar (`[]`).      |
| `units`                        | optional                    | Free-form label.                                    |
| `scaling_factor_multiplier`    | optional                    | Opaque label, stored verbatim.                      |
| `features`                     | optional                    | JSON object; int/float/bool/string values.          |
| `initial_timestamp`            | all except `non_sequential` |                                                     |
| `resolution`                   | all except `non_sequential` |                                                     |
| `horizon`, `interval`, `count` | forecasts                   |                                                     |
| `percentiles`                  | `probabilistic`             | Strictly increasing list of floats.                 |
| `scenario_count`               | `scenarios` (optional)      | Inferred from the data length if omitted.           |

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
values round-trip.

## Exit Status

`0` on success; `1` on any error (the message is printed to stderr, prefixed with `Error:`).
