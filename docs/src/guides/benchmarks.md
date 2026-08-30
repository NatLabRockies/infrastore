# Benchmarks

`infrastore-bench` is a standalone binary (`infrastore-bench`) for measuring two critical
performance dimensions: **bulk ingestion** and **simulation-loop reads**.

## Build

```sh
# Debug build (fast to compile, slower to run)
cargo build -p infrastore-bench

# Release build (recommended for any real measurement)
cargo build --release -p infrastore-bench
```

The binary is placed at `target/release/infrastore-bench`.

## Subcommands

| Subcommand | What it measures                                                             |
| ---------- | ---------------------------------------------------------------------------- |
| `add`      | `add_time_series_bulk` throughput — HDF5 packing and SQLite transaction cost |
| `read`     | Per-timestep simulation I/O — reading all N components at each step t        |
| `all`      | Runs `add` then `read` back-to-back                                          |

## Common flags

| Flag          | Default  | Description                                                        |
| ------------- | -------- | ------------------------------------------------------------------ |
| `--count N`   | 1 000    | Number of components (time series)                                 |
| `--length L`  | 168      | Timesteps per `SingleTimeSeries`; window count for `Deterministic` |
| `--in-memory` | off      | Use an in-memory store — eliminates disk I/O to isolate CPU cost   |
| `--path DIR`  | temp dir | Directory for on-disk store files                                  |

The `read` and `all` subcommands also accept `--steps T` (default: `--length`) to control how many
simulation timesteps are benchmarked.

## Add benchmark

```sh
# 10 000 components, 1 week hourly, in-memory
infrastore-bench add --count 10000 --length 168 --in-memory

# 100 000 components, same length, on-disk (tests real I/O + OS page cache)
infrastore-bench add --count 100000 --length 168
```

Reports for both `SingleTimeSeries` and `Deterministic`:

- **Build requests** — time to construct `AddRequest` objects in memory (array allocation only;
  excluded from the add throughput measurement).
- **`add_time_series_bulk`** — wall time, items/s, and data MB/s.

`SingleTimeSeries` arrays are column-packed into HDF5 datasets of up to 1 000 columns each and
wrapped in a single SQLite transaction; the add benchmark stresses both. `Deterministic` arrays are
standalone HDF5 variables; their add cost is dominated by per-variable write overhead and the same
single-transaction SQLite commit.

## Read benchmark

```sh
# 1 000 components, 168 timesteps, in-memory (pure CPU / metadata cost)
infrastore-bench read --count 1000 --length 168 --in-memory

# Same, but on-disk — store is written, dropped, then reopened read-only
infrastore-bench read --count 1000 --length 168

# Benchmark only the first 24 simulation steps of a 168-step series
infrastore-bench read --count 10000 --length 168 --steps 24
```

The read benchmark simulates the access pattern of an energy-simulation step loop:

```
for t in 0..T:
    for each component key:
        store.read_by_ids_range(ids, (t₀ + t·Δt, t₀ + (t+1)·Δt))
```

For on-disk stores the binary flushes, drops, and reopens the store read-only before the timed loop.
This rebuilds the HDF5 in-memory index and starts the HDF5 chunk cache cold, reflecting real
simulation startup conditions. The OS page cache may still be warm.

Reports per-step **min / median / p95 / max** and total **component-reads/s**.

> **Deterministic note.** The current `read_by_id` implementation reads the full `[H × count]` array
> from storage and then slices to the requested window in memory. For large `count` (e.g. 168
> windows) each single-step read transfers `H × count × 8` bytes, which is `24 × 168 × 8 = 32 KB`
> per component. The benchmark output flags this explicitly so the overhead is visible.

## Interpreting results

The two metrics to watch:

- **`add` throughput** drops sharply when the number of items exceeds a SQLite transaction or HDF5
  dataset threshold. If adding 100 000 items is notably slower per-item than adding 10 000, the
  bottleneck is likely the SQLite commit or HDF5 file growth, not array construction.

- **Per-step time** for the read benchmark scales linearly with `--count`. If the cost per step is
  much higher for on-disk than in-memory, the bottleneck is HDF5 chunk reads or SQLite metadata
  queries rather than Rust overhead. The packed `SingleTimeSeries` layout chunks `(1, cols)` — one
  timestamp across every column — so reading one timestamp across many series is a single chunk
  read, while reading one full series touches every chunk band (the slow direction, expected for
  exploration/plotting rather than hot loops).

## Tracing spans for deeper diagnosis

When the benchmark numbers show a problem but don't tell you which layer is slow, enable the
built-in tracing spans with `--log-level`:

```sh
# Show all debug-level spans from the store core only (least noise)
infrastore-bench --log-level infrastore_core=debug add --count 100 --in-memory

# Show everything — useful when the bottleneck might be in infrastore-bench itself
infrastore-bench --log-level debug add --count 100
```

`RUST_LOG` is also accepted as a fallback when `--log-level` is not provided.

The key spans emitted by `infrastore-core`:

| Span                      | Layer        | Key fields                                       |
| ------------------------- | ------------ | ------------------------------------------------ |
| `add_time_series_bulk`    | `Store`      | `count` — number of items in the bulk request    |
| `read_by_id`              | `Store`      | `id`, `window`                                   |
| `copy_time_series`        | `Store`      | `src_id` — the source association                |
| `remove_by_ids`           | `Store`      | `count` — number of ids in the request           |
| `read_by_ids`             | `Store`      | `count` — number of ids read in one pass         |
| `list_metadata_by_ids`    | `Store`      | `count` — number of ids requested                |
| `put_array`               | HDF5 backend | `bytes`, `packed`                                |
| `put_packed`              | HDF5 backend | `bytes`                                          |
| `put_packed_block`        | HDF5 backend | `n` — series written in one batch-sized block    |
| `put_standalone`          | HDF5 backend | `bytes`                                          |
| `get_array` / `get_slice` | HDF5 backend | `start`, `end` (slice only)                      |
| `read_arrays`             | HDF5 backend | `n` — arrays fetched in one decompress-once pass |
| `read_index_into`         | HDF5 backend | `n`, `index` — one timestep across `n` series    |
| `read_window_into`        | HDF5 backend | `count_axis`, `window_index`                     |
| `read_locked`             | HDF5 backend | —                                                |
| `rebuild_index`           | HDF5 backend | — (runs once on `Store::open`)                   |

Spans nest: a single `add_time_series_bulk` call groups packed series by shape and emits one
`put_packed_block` span per group (filling whole chunks), plus a `put_array` → `put_standalone` span
per standalone item; a single `add_time_series` call instead emits one `put_array` → `put_packed`
span. This makes it straightforward to see whether time is spent in metadata insertion, HDF5 I/O, or
the `debug_span` overhead itself.

The `infrastore` CLI supports the same `--log-level` flag for diagnosing a live store.
