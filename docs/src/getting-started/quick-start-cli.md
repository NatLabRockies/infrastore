# Quick Start (CLI)

The `infrastore` binary reads and writes an on-disk store directly — no server, no binding, no
Python or Julia environment. This is the shortest path from a CSV to a store you can inspect.

Get the binary from the [Releases page](https://github.com/NatLabRockies/infrastore/releases) (the
executables are statically linked, so there is nothing else to install) or with
`cargo install infrastore-cli`. See [Installation](./installation.md#the-infrastore-cli) for the
per-platform archives.

## A Minimal Round-Trip

Two files: the values, and a descriptor saying what they mean.

```text
# load.csv
value
100.0
101.5
103.0
104.2
```

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
  "resolution": "PT1H"
}
```

Save that as `load.json`, then add and read it back:

```sh
infrastore --store demo.h5 add --descriptor load.json
infrastore --store demo.h5 list
infrastore --store demo.h5 get --owner-id 42 --name load
```

```text
╭────┬───────┬────────────┬───────────┬──────────────────┬──────┬──────────┬──────────────┬────────────┬──────────┬────────┬───────┬──────────────╮
│ ID │ Owner │ Owner Type │ Category  │ Type             │ Name │ Features │ Element Type │ Resolution │ Interval │ Length │ Units │ Hash         │
├────┼───────┼────────────┼───────────┼──────────────────┼──────┼──────────┼──────────────┼────────────┼──────────┼────────┼───────┼──────────────┤
│ 1  │ 42    │ Generator  │ Component │ SingleTimeSeries │ load │ -        │ f64          │ PT1H       │ -        │ 4      │ MW    │ 09ec58683de3 │
╰────┴───────┴────────────┴───────────┴──────────────────┴──────┴──────────┴──────────────┴────────────┴──────────┴────────┴───────┴──────────────╯

╭───────────────────────────┬───────╮
│ timestamp                 │ value │
├───────────────────────────┼───────┤
│ 2024-01-01T00:00:00+00:00 │ 100   │
│ 2024-01-01T01:00:00+00:00 │ 101.5 │
│ 2024-01-01T02:00:00+00:00 │ 103   │
│ 2024-01-01T03:00:00+00:00 │ 104.2 │
╰───────────────────────────┴───────╯
```

## What Just Happened

- **`demo.h5` and `demo.h5.sqlite` were both created.** They are one artifact: the arrays are in the
  HDF5 file, the catalog row in the SQLite one. Move, copy, and delete them together.
- **The values came from the CSV; everything else came from the descriptor.** A flat grid of numbers
  fits a CSV; an owner, a resolution, and a feature map do not.
- **The header row is required.** `add` reads it to tell a hand-written value-only file from one
  `infrastore export` wrote, so a file whose first row is data is rejected rather than silently
  losing that row.
- **The store assigned `id` 1.** That id is how every later read and removal addresses the series —
  see [Association IDs](../explanation/data-model.md#association-ids).
- **Timestamps must name an instant.** `2024-01-01T00:00:00Z` does; a bare `2024-01-01 00:00:00`
  does not, and is rejected. Pass `--assume-timezone UTC` to say what a zoneless file meant.

Print a starting descriptor for any of the five writable types with `infrastore template`:

```sh
infrastore template NonSequentialTimeSeries > outages.json
```

## Look Around

```sh
infrastore --store demo.h5 names                    # distinct series names
infrastore --store demo.h5 get --name load --plot   # a terminal sparkline
infrastore --store demo.h5 store-info               # format version, compression, catalog state
infrastore --store demo.h5 -f json list             # every read command honors -f
```

## Next Steps

- The whole workflow — wide CSVs, forecasts, charts, associations, `diff` and `merge` — is in the
  [CLI Developer Guide](../guides/cli.md).
- Every flag and the descriptor schema: [CLI Reference](../reference/cli.md).
- Doing this from a program instead: [Python](./quick-start-python.md) ·
  [Julia](./quick-start-julia.md).
