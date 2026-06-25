# IS.jl → Rust migration backlog

Remaining candidates for migrating time-series logic out of InfrastructureSystems.jl
(`../sienna/IS3.jl`) and down into the `time-series-store` Rust core (exposed via the FFI +
`TimeSeriesStore.jl` binding).

**Status of prior work:** The catalog/query migration (key-identity enum, `list_keys`, the
ListFilter widening, and the Tier-2 aggregate SQL pushdowns) is **done and committed** — 9 Rust
commits on branch `key-identity-enum`, and the IS glue rewrite is committed as `799a99c6` on IS
branch `feat/rust-time-series`. This file tracks what's left after that.

---

## ⚠️ The gating issue: the core has no calendar-aware period type

The core models `resolution`, `horizon`, and `interval` as `chrono::Duration` everywhere
(`crates/time-series-store-core/src/types/key.rs:25,55,56`, `types/time_series.rs`,
`types/metadata.rs:105–108`) — a **fixed nanosecond span**.

IS uses Julia's `Dates.Period`, which distinguishes:

- **regular** periods (`Hour`, `Day`) — fixed span, round-trips through `Duration` fine
- **irregular** periods (`Month`, `Quarter`, `Year`) — calendar arithmetic, **no fixed span**

This is the gap that caused the earlier regression (`Month(12)` over-matching a ms-canonicalized
forecast), which forced the IS rewrite to keep all resolution/interval comparisons Julia-side. A
large block of "pure math, easy Rust win" timestamp functions **cannot move down until the core can
represent an irregular period.** They look migratable in isolation but they are exactly the calendar
logic Rust can't currently express.

**Foundational decision for the next phase:** does the core get a calendar-aware period type (e.g. a
`Resolution` enum like `{ Fixed(Duration), Months(i64), Years(i64) }`)?

- **Yes →** unblocks the window/index arithmetic tier below; IS glue stops re-deriving it.
- **No →** that logic stays in Julia permanently and we should stop eyeing it.

This is the prerequisite for **Item 1** below.

---

## Item 1 — Core calendar-aware period / `Resolution` type _(FOUNDATION DONE)_

**Status (2026-06-25):** the foundational `Period` type is **implemented and shipped** across the
whole Rust stack + every binding. `crates/time-series-store-core/src/types/period.rs` defines
`Period { Fixed(Duration), Months(i32) }` (Quarter→3, Year→12; canonical) with
`add_to`/`steps_between`/`floor_steps`/`ceil_steps`/`divide_into`/`to_iso8601`/`from_iso8601`,
`From<Duration>`, `Display`, and `PartialEq<Duration>`. `resolution`/`horizon`/`interval` are now
`Period` on `SingleTimeSeries`/`Deterministic`/`Probabilistic`/`Scenarios`/`TimeSeriesMetadata`/
`KeyIdentity`/`ForecastTimeSeriesKey`/`ListFilter`/`ForecastParameters`/readers, and all the
divisibility/index/window arithmetic (`compute_h`, DST synthesis, `resolve_windows`,
`reader::index_on_grid`, time-range slicing) is calendar-aware. On disk it is an ISO-8601 string
(NetCDF dataset-name segment + SQLite `resolution`/`horizon`/`interval` TEXT columns);
`DATA_FORMAT_VERSION` bumped 0.6.0 → **0.7.0** (clean break). Bindings: proto/gRPC string fields,
FFI `*const c_char` in / owned `char**` out (+ `ts_string_free`, header regenerated), Julia
`_period_to_iso`/`_iso_to_period`, Python `timedelta|str` in / ISO `str` out, CLI
`parse_period`/`format_period`. Forecast rule enforced: `resolution.kind == horizon.kind` (so `H` is
constant); `interval` independent; DST derivation needs `interval.kind == resolution.kind`.
**Verified:** full Rust workspace (clippy/fmt/tests) green, pytest 44 passed, Julia suite green,
dprint clean. NOTE: `cargo deny` shows pre-existing pyo3-0.28 advisories (RUSTSEC-2026-0176/0177),
unrelated to this change.

**Remaining for Item 1 (follow-up, not blocking):** migrate the additive Julia helper functions in
the table below into new core fns + FFI + Julia wrappers (they now have `Period` to build on), and
delete the corresponding Julia code. `get_resolutions` ordering is now lexical-by-ISO (mixed period
kinds have no numeric total order) — callers must not assume numeric sort.

Introduce a period type in the core that can represent irregular periods, then migrate the
timestamp/window-index math that depends on it.

**Why:** unblocks an entire tier of pure-computation functions currently stuck in Julia and stops
the glue from re-deriving window math.

**Touches `DATA_FORMAT_VERSION`** — irregular resolutions change how resolution is encoded on disk
(`crates/time-series-store-core/src/version.rs`; see file-format docs). This is the main risk.

**Functions unblocked once the type exists** (all currently Julia-side, pure computation):

| Function                    | File                                  | Lines     | Computes                                                                |
| --------------------------- | ------------------------------------- | --------- | ----------------------------------------------------------------------- |
| `compute_periods_between`   | `time_series_utils.jl`                | 128–182   | periods between two timestamps (regular + Month/Quarter/Year overloads) |
| `compute_time_array_index`  | `time_series_utils.jl`                | 82–91     | DateTime → 1-based array index                                          |
| `get_initial_times`         | `time_series_utils.jl`                | 42–54     | sequence of forecast-window start timestamps                            |
| `get_total_period`          | `time_series_utils.jl`                | 200–211   | last timestamp from count/interval/horizon/resolution                   |
| `check_resolution`          | `time_series_utils.jl`                | 19–36     | validate consecutive timestamps differ by exactly `resolution`          |
| `is_irregular_period`       | `time_series_utils.jl`                | 278–281   | classify Month/Quarter/Year                                             |
| `get_forecast_window_count` | `time_series_interface.jl`            | 1064–1088 | # forecast windows fitting a data range                                 |
| `_forecast_window_range`    | `rust_time_series_store.jl`           | 700–728   | validate + compute `(start_idx, n)` window range                        |
| `get_window` index math     | `deterministic_single_time_series.jl` | 112–137   | window start/end indices                                                |

**Also relevant:** ISO-8601 period (de)serialization `to_iso_8601` / `from_iso_8601`
(`time_series_utils.jl:217–276`) — Rust currently uses a different human format (`parse.rs`,
`"1h"`); no ISO-8601 period support exists in Rust.

---

## Item 2 — Timestamp readers: delete the Julia cache _(highest value; do FIRST)_

**Goal:** Remove `time_series_cache.jl` (523 lines) entirely. Replace it with a stateful Rust
_reader_ that, for a given timestamp, returns the value for every component/attribute at once. This
matches how simulations consume the library — a `for` loop over all timestamps — and is why
SingleTimeSeries is stored in the compacted format.

### Storage facts this design rests on (verified in core)

- **Compacted/packed format** groups SingleTimeSeries by
  `(dtype, element_shape, length,
  resolution_ms)` — the `DatasetGroupKey`
  (`storage/netcdf.rs:240`). `initial_timestamp` is **not** in the key. Dataset dimension order is
  `[time, column, *element_shape]` (`packed_extents`, `netcdf.rs:536`), so **one timestamp across
  all columns of a packed dataset is a single NetCDF hyperslab** `data[idx, :, …]`. This is the
  access pattern the reader exploits.
- **Dense forecasts are NOT packed.** Deterministic/Probabilistic/Scenarios are stored standalone in
  native shape (`store.rs:315`). A "window at `t`" is a per-forecast slice of its own array — no
  columnar benefit.
- Per-column hash lives in the `{dataset}_h` companion var; the SQLite catalog maps hash → key
  (owner_id, owner_category, type, name, features). The reader does this join once, at build time.

### Locked design decisions (user, 2026-06-25)

1. **One resolution per reader.**
2. **Static and forecast are separate calls / separate readers.** A forecast reader targets **one
   forecast type** (no mixing — the workflow never mixes types in a single reader).
3. **Return is columnar batches** grouped by `(dtype, element_shape)` for static.
4. **No presence mask.** A single-resolution reader is over series sharing one grid
   (`initial_timestamp` + `length` = the sim horizon), so every column is present at every valid
   `t`. Build-time **validates** all participating series share the grid and **errors** on
   divergence (the store structurally allows divergence; the sim workflow never produces it).
5. **Stateful reader handle** over the FFI (opaque handle, like existing handles).
6. **Buffer reuse** — `read(t)` overwrites reader-owned buffers; Julia reinterprets/copies before
   the next call. Zero per-step allocation across the (e.g.) 8760-iteration loop.
7. **Off-grid `t` → hard error** (no clamp/round). Consistent with #4.
8. **NonSequentialTimeSeries is excluded** (irregular, no resolution — doesn't fit a
   fixed-resolution loop).

### `StaticReader` (SingleTimeSeries)

**Build** `make_static_reader(filter{resolution, …}) -> StaticReader`:

- catalog join once; validate all series share `initial_timestamp` + `length` (error otherwise);
- partition columns into **groups by `(dtype, element_shape)`**;
- return ONCE: master grid (`initial_timestamp`, `resolution`, `length`) and, per group,
  `(dtype, element_shape, ordered Vec<key>)`; internally retain each group's
  `(dataset, column-positions)`.

**Read** `reader.read(t) -> &[buffer]` (reader-owned, overwritten each call), one buffer per group
in build-time order:

- `idx = (t − initial_timestamp) / resolution`; error unless integral and in `[0, length)`;
- per physical dataset, one hyperslab `[idx, :, …]`, gather the group's columns into a
  `[n_cols, *element_shape]` typed buffer.
- No catalog access, no key allocation — just bytes. Column _j_ of group _g_ ↔ the _j_-th key in
  group _g_'s build-time key list.

### `ForecastReader` (one forecast type)

**Build** resolves per-forecast `(key, hash, interval, horizon, count, shape)` for the chosen type +
resolution. **Read** `read(t)`: `w_idx = (t − initial)/interval` per forecast; return a per-key
window list `(key_index, window_buffer, window_shape)` where shape is `[horizon, *step]` /
`[horizon, n_percentiles, …]` / `[horizon, n_scenarios, …]`. Standalone arrays → one slice read
each; not columnar.

### Surface to build

- **Core:** `StaticReader` / `ForecastReader` types on `Store` (build = catalog join + layout plan;
  read = hyperslab gather into owned buffers). New hyperslab-at-index read on the NetCDF backend.
- **FFI:** opaque `TsStaticReaderHandle` / `TsForecastReaderHandle`; build fn returns handle +
  serialized layout (keys/dtypes/shapes/group order) once; `read(handle, t)` fills owned buffers and
  returns pointers + lengths (probe-then-fetch or direct pointer into reader-owned memory, valid
  until the next `read`); free fn. cbindgen regen.
- **Julia (`TimeSeriesStore.jl`):** wrap the handles; expose iterate-by-timestamp returning columnar
  typed matrices (static) / window arrays (forecast).
- **IS (`IS3.jl`):** delete `time_series_cache.jl`; rewire `time_series_interface.jl` consumers and
  the sim loops to the new reader. (Sequential-access validation in the old cache becomes moot — the
  reader is random-access by `t` with a hard off-grid error.)

**No on-disk format change** — reads only; `DATA_FORMAT_VERSION` untouched.

### Sketch status (2026-06-25, uncommitted working tree)

Core `StaticReader` first draft landed and validated (clippy/fmt clean, 3 unit tests + full core lib
suite green):

- `crates/time-series-store-core/src/reader.rs` — `StaticReader` / `StaticGroup` (passive plan +
  reusable per-group buffers), pure `build_groups` (grid validation + `(dtype,element_shape)`
  grouping + deterministic column order), `index_at` (off-grid = hard error). Unit tests cover
  columnar read, buffer reuse, off-grid errors, divergent-grid rejection.
- `crates/time-series-store-core/src/store.rs` — `Store::build_static_reader(filter)` (requires a
  resolution; SingleTimeSeries-only) + `Store::static_read(&mut reader, at)` (fills buffers via the
  backend, mask-free).
- `crates/time-series-store-core/src/storage.rs` — new
  `StorageBackend::read_index_into(hashes,
  index, &mut out)` **default** method (per-hash
  `get_slice`, works for every backend incl. MemoryBackend).
- `crates/time-series-store-core/src/lib.rs` — `pub mod reader;` + re-exports.

Branch: `static-reader`. Commits: `240a2b4` (StaticReader), `9a85d4b` (NetCDF override), `98252de`
(bounded read), `efb575f` (ForecastReader).

**Still TODO for this item:**

1. ~~**NetCdfBackend override of `read_index_into`**~~ — DONE (`9a85d4b`, bounded `98252de`). One
   hyperslab per packed dataset (`[idx, 0..=max_col, …]`) + gather; groups input hashes by
   `Location::Packed{dataset, col}`; standalone hashes fall back to a single-step read. On-disk test
   cross-checks the override vs the in-memory default byte-for-byte across every grid index, incl. a
   multi-dim element shape and non-contiguous high columns.
2. ~~**ForecastReader**~~ — DONE (`efb575f`, DST in `5a80950`). One forecast type; per-key window
   list; standalone arrays read one window per hyperslab (count axis fixed); uniform-timeline
   validation at build; shared `index_on_grid`. On-disk vs in-memory cross-check + build/off-grid
   error tests. A `Deterministic` reader is **abstract**: it also includes
   `DeterministicSingleTimeSeries`, read into byte-identical `[H, *E]` windows (DST window k = the
   contiguous slice `[k·interval_steps .. +H]` of the packed underlying STS, via `get_slice`).
3. ~~**FFI**~~ — DONE (`a500dc3`). Opaque `TsStaticReaderHandle` / `TsForecastReaderHandle`. Static:
   `ts_store_build_static_reader`, `_grid`, `_num_groups`, `_group_info` (dtype + columns + element
   shape, probe-then-fetch), `_group_key` (owned `KeyIdentity` handle), `_read`, `_group_values`
   (pointer into reader-owned memory, valid until next read/free), `_free`. Forecast: analogous
   `_timeline` / `_num_entries` / `_entry_info` / `_entry_key` / `_read` / `_entry_values` /
   `_free`; build pins the forecast type (Deterministic abstract over DST). cbindgen header
   regenerated; Rust roundtrip tests for both readers.
4. ~~**Julia wrapper**~~ — DONE (`99555f9`). `TimeSeriesStore.jl` `StaticReader` / `ForecastReader`
   types with finalizers: `build_static_reader(store; resolution, …)` → `static_groups` (dtype /
   element_shape / column keys) → `static_read!(reader, t)` → `static_values(reader, gi)`
   (column-major `(num_columns, element_shape…)`); `static_grid`. Forecast analog:
   `build_forecast_reader(store, type; resolution, …)` (a Julia forecast type, `Deterministic`
   abstract over DST) → `forecast_read!` → `forecast_values`; `forecast_timeline` /
   `forecast_entries`. Values copied + reshaped row-major→column-major (stay valid across reads).
   Julia testsets cover both readers (multidtype/multidim, off-grid, DST≡Deterministic); full suite
   green against the release dylib.
5. **IS cache deletion** — remaining: delete `time_series_cache.jl` (523 lines) in IS3.jl and rewire
   consumers (`time_series_interface.jl`) to the new reader API.
6. Decide: should `build_static_reader` / `build_forecast_reader` over an empty match be an error
   (current) or an empty reader?

### Old cache anatomy (for the IS deletion)

- `ForecastCache` / `StaticTimeSeriesCache` (`time_series_cache.jl:207–300`, `345–416`) — FIFO
  windows; `_update!` (308–331, 421–440) is the only fetch point.
- `_get_row_size` (453–466) — array-footprint measurement (becomes irrelevant).
- The cache has no direct Rust calls today; it sits entirely above the FFI boundary.

---

## Item 3 — Parsing & normalization consolidation _(lower priority; ingest-path)_

Consolidate the time-series ingest parsing, much of which is **already duplicated** in the Rust CLI.

**Why lower:** it's the ingest path, not the query path, and the Rust CLI already has a parallel
implementation (`descriptor.rs::load`, `parse.rs`). Less leverage than Items 1–2.

**Candidates:**

| Function                         | File                        | Lines   | Notes                                                                                |
| -------------------------------- | --------------------------- | ------- | ------------------------------------------------------------------------------------ |
| `read_time_series_file_metadata` | `time_series_parser.jl`     | 68–135  | JSON/CSV descriptor parse — **dup** of CLI `descriptor.rs::load` (61–92)             |
| `handle_normalization_factor`    | `time_series_parser.jl`     | 148–185 | **Julia-only**: MAX-scaling / divisor normalization at ingest; Rust does not do this |
| `check_params_compatibility`     | `time_series_parameters.jl` | 40–69   | forecast count/timestamp/horizon validation; Rust enforces at construction instead   |

**Existing Rust CLI parsing to reconcile with:** `descriptor.rs::load` / `to_add_request`,
`parse.rs::{parse_duration, parse_timestamp, parse_dtype, parse_ts_type, horizon_steps}`. Note
format mismatches: Julia descriptors use **seconds** + **ISO-8601** periods; Rust CLI uses human
strings (`"1h"`) + RFC3339/epoch-ms timestamps.

---

## Poor candidates (leave in Julia)

Logic that returns Julia `TimeSeries.TimeArray` / dispatches on Julia types / navigates the system —
not worth migrating:

- `make_time_array`, `get_time_series_array`, `get_time_series_timestamps`, `get_time_series_values`
  (`time_series_interface.jl`) — return Julia `TimeArray`.
- `iterate_windows` / `iterate_windows_common` — thin Julia generators over `get_window`.
- `make_timestamps` — returns a Julia `StepRange{DateTime}`.
- `_check_transform_single_time_series` (`system_data.jl:707+`) — system traversal + conflict
  detection; the core transform itself is already in Rust (`TSS.transform_single_time_series!`).
- `TimeSeriesParsedInfo`, the parsing cache / assignment tracking (`time_series_parser.jl:220–312`)
  — Julia UUID/component plumbing.

---

## Suggested order

1. **Item 2 (windowed reads)** — self-contained, no format change, deletes the most Julia code.
2. **Item 1 (period type)** — foundational design decision; touches `DATA_FORMAT_VERSION`; unblocks
   the window/index arithmetic tier.
3. **Item 3 (parsing/normalization)** — opportunistic cleanup of ingest-path duplication.
