# Use the `tss` CLI

`tss` loads time series from CSV files and inspects a store, talking directly to the on-disk `.nc` +
`.nc.sqlite` pair (no gRPC server required). For the full command and sidecar reference, see
[CLI Reference](../reference/cli.md).

## 1. Build the Binary

```sh
cargo build -p time-series-store-cli      # debug build at target/debug/tss
# or a release build:
cargo build -p time-series-store-cli --release
```

The examples below assume `tss` is on your `PATH` (or use `./target/debug/tss`).

## 2. Describe the Data

Numeric values live in a CSV; everything that does not fit a flat grid (owner, name, type, dtype,
resolution, initial timestamp, units, features) lives in a **sidecar TOML**. Print a starting point
for any type with `template`:

```sh
tss template single > load.toml
```

Edit it to point at your data and metadata:

```toml
# load.toml
owner_uuid = "42"
owner_type = "Generator"
owner_category = "component"
name = "load"
type = "single"
dtype = "f64"
units = "MW"
csv = "load.csv"               # relative to this sidecar; override with --csv
has_header = true
initial_timestamp = "2024-01-01T00:00:00Z"
resolution = "1h"

[features]
model_year = 2030
```

```text
# load.csv
value
100.0
101.5
103.0
```

## 3. Add It to a Store

```sh
tss --store demo.nc add --sidecar load.toml
```

The store (`demo.nc` and its `demo.nc.sqlite` sidecar) is created on first `add`. A sidecar may also
hold a `[[series]]` array of tables to add many series in one transaction.

## 4. Read It Back

`tss` follows the same output convention as the sibling `torc` CLI: a global `-f/--format` with
`table` (default), `json`, and `csv`.

```sh
tss --store demo.nc list                                       # what's in the store
tss --store demo.nc get  --owner-uuid 42 --name load           # pretty table
tss --store demo.nc -f csv  get  --owner-uuid 42 --name load   # round-trippable CSV
tss --store demo.nc -f json info --owner-uuid 42 --name load   # metadata + min/max/mean
```

`get`/`info`/`remove` select a single series with `--owner-uuid`, `--name`, `--type`,
`--resolution`, and repeated `--feature key=value`; if more than one series matches, `tss` lists the
candidates so you can narrow the query. Large series truncate in `table` output — pass `--limit N`
or `--full`.

## 5. Forecasts

All five writable types work (`single`, `non_sequential`, `deterministic`, `probabilistic`,
`scenarios`). Forecast values are laid out as a flat, row-major stream whose count must equal the
product of the type's shape (e.g. `[H, count, *element_shape]` for `deterministic`, where
`H = horizon / resolution`). `tss template deterministic` documents the layout. Use `-f csv` or
`-f json` to read them back at full fidelity.

`DeterministicSingleTimeSeries` is not added from CSV — store a `SingleTimeSeries`, then derive it:

```sh
tss --store demo.nc transform --horizon 24h --interval 1h
```

## Notes

- The CLI writes locally; there is no remote/gRPC mode yet (store access is isolated so one can be
  added later).
- Output is colored (green table headers, like `torc`) only when stdout is a terminal; it is plain
  when piped/redirected or when `NO_COLOR` is set, so `-f json`/`-f csv` stay clean for other tools.
- `--log-level` (or `RUST_LOG`) controls logging; the default is quiet (`warn`).
- The `.nc` and `.nc.sqlite` files are one artifact — move, copy, and delete them together.
