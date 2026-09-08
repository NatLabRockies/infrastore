# Plan: Parquet export/import and store-level attributes

Two features, agreed on 2026-09-07 after a survey of the current surface. Each work item carries the
context needed to implement it without re-deriving the discussion. Resampling on read was considered
and dropped; the reasoning is recorded in §4 so it is not re-litigated.

## 1. Ground rules

- **Core is the source of truth.** Both features land in the Rust core first, then in the C ABI
  (regenerating `crates/infrastore-ffi/include/infrastore.h`, never hand-edited), Julia, Python, the
  CLI, and the read-only gRPC server where the feature has a read half. A feature is done when every
  binding has it, per `CLAUDE.md`.
- **Never** bump `DATA_FORMAT_VERSION` for either feature. Neither touches the HDF5 layout. Store
  attributes are a purely additive catalog table, so `CATALOG_SCHEMA_REVISION` stays put too.
- **Dependency policy.** Arrow and Parquet are new dependencies and `deny.toml` treats every new
  dependency as a policy decision. They stay out of the default build (§2.1) and any new transitive
  license goes through review before it joins the allowlist.
- **Quality gates after every phase**, all passing before the phase's commit:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-features
  dprint check
  cargo deny check --config deny.toml
  ```

  Python phases additionally run `maturin develop` and `pytest python/tests`; Julia phases build
  `infrastore-ffi` and run the package tests, as documented in `CLAUDE.md`.

## 2. Parquet export and import

**Revised 2026-09-07, and again 2026-09-08.** The first version of this section exported one Parquet
file per series (commits `3750a82` through `cce9d45`); it does not scale past a few hundred series.
The second version replaced it with partitioned long tables, one row per value with every catalog
column beside it (commits `450ad84` through `de7cdff`). That one **duplicates arrays**: the store is
content-addressed, so a thousand components sharing one profile hold one array, but a long table
writes its rows a thousand times, and Parquet's compression does not find repeats across pages. This
revision **normalizes** the layout the way the store itself is normalized: per partition, a
**values** file holding each distinct array once and a **series** file holding one catalog row per
series, joined on the array's identity. The store-attributes work (§3) and the crate structure
(§2.1) are unchanged. The rework lands as new commits on top of the existing ones; history is not
rewritten.

### 2.1 Where the code lives

Unchanged: the `infrastore-parquet` crate, depending on `infrastore-core` and on `arrow` and
`parquet` with default features off. The CLI enables it through its `parquet` feature, which is **on
by default** (decided 2026-09-07): the shipped `infrastore` binary carries Parquet, while
`infrastore-core`, `infrastore-py`, and `infrastore-ffi` never depend on the Arrow tree. The
bindings export to Parquet themselves through `to_arrow()` if they want to.

### 2.2 Partitioning: two files per (type, value type, time reference)

Three things cannot vary within one Parquet table without nullable or ill-typed columns: the set of
key columns (a forecast has an `issue_time`, a static series does not), the Arrow type of the
`value` column, and the zone of the `timestamp` column. So the export **partitions the selection by
the triple**

```text
(time_series_type, value type, time_reference)
```

and writes **two files per distinct triple**:

- `<type>.<value-slug>.<reference-slug>.values.parquet` — every distinct array in the partition,
  once, one row per value.
- `<type>.<value-slug>.<reference-slug>.series.parquet` — one row per series, the whole catalog row,
  naming the array it reads.

Within both files every column is **required**; there are no nullable columns anywhere in the
format. The three partition keys are written as ordinary columns in the series file (constant within
the file, so they dictionary-encode to nothing) and recorded in both files' footers so a reader
knows the partition before scanning a row. The two files of a partition share a stem, which is how
the import pairs them; a stem with one half missing is an error.

The **value type** is the element type with its shape, except that composite kinds
(`piecewise_linear`, `piecewise_step`) partition by kind alone: their stored width `w` varies per
series, so within a file every composite row is **re-padded to the widest series in that file**. The
layout allows any width the kind can produce and the leading count `n` keeps rows self-describing,
so this is a legal re-encoding, with the `data_hash` caveat in §2.6.

Slugs must be filesystem-safe on all three CI platforms (no `(`, `,`, `/`, `:`, `+`); the slug
function is one-way and tested, collisions get a suffix, and the footer carries the exact partition
key, so the name is a convenience rather than the truth.

A `DeterministicSingleTimeSeries` is handled the way the CSV export already handles it; the import
refuses a partition whose type names it, pointing at `transform_single_time_series`, since the type
is derived rather than added.

### 2.3 The array key: `data_hash` plus `time_axis`

`data_hash` alone does not identify a group of value rows. It covers the array bytes and not the
time axis: the same 8760-value profile anchored on two different years is one stored array with two
different timestamp columns, and for the irregular types the project's own docs are explicit that
two series with identical values on different axes share one array and only the catalog's
`timestamps_hash` tells them apart. So value rows are keyed by the **pair**

```text
(data_hash, time_axis)
```

where `time_axis` is one string spelling the catalog fields that determine the timestamps for that
type. Within a file the spelling is uniform, since a partition is one type:

| Type                                              | `time_axis`                                                                                                                   |
| ------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `SingleTimeSeries`                                | the ISO 8601 repeating interval `R<length>/<initial_timestamp>/<resolution>`, e.g. `R8760/2024-01-01T00:00:00Z/PT1H`          |
| `NonSequentialTimeSeries`, `PersistentTimeSeries` | the catalog's `timestamps_hash`, hex                                                                                          |
| dense forecasts                                   | `R<count>/<initial_timestamp>/<interval>/<horizon>/<resolution>`, since the horizon's own step decides the `target_time` rows |

The timestamp inside a `time_axis` is spelled as the instant in UTC regardless of the partition's
`time_reference`; the reference is a partition key, so nothing is lost. Both columns are
dictionary-encoded and cost one entry per distinct array per row group.

For composite kinds the `data_hash` written is the **canonical** (minimum-width) hash rather than
the catalog's, as Finding 7.14 decided, so that a file whose rows were re-padded still verifies.

### 2.4 Columns

**Values file**, one row per value, rows **contiguous per array key** and sorted by
`(data_hash, time_axis)` and then by the time columns:

| Column       | Type                  | Files           | Notes                                                                                        |
| ------------ | --------------------- | --------------- | -------------------------------------------------------------------------------------------- |
| `data_hash`  | `utf8` (hex)          | all             | Half of the array key; §2.3.                                                                 |
| `time_axis`  | `utf8`                | all             | The other half; §2.3.                                                                        |
| `timestamp`  | `timestamp(ms, zone)` | all             | The target time for a forecast, the breakpoint for a `PersistentTimeSeries`. Zone from §2.5. |
| `issue_time` | `timestamp(ms, zone)` | forecasts       | Same zone as `timestamp`.                                                                    |
| `percentile` | `float64`             | `Probabilistic` | One row per (issue, target, percentile).                                                     |
| `scenario`   | `int64`               | `Scenarios`     | Zero-based scenario index.                                                                   |
| `value`      | per §2.6              | all             |                                                                                              |

**Series file**, one row per series, sorted by `(data_hash, time_axis)` and then by `id`, so the
import can walk both files together:

| Column                                                                         | Type                             | Files                         | Notes                                                                          |
| ------------------------------------------------------------------------------ | -------------------------------- | ----------------------------- | ------------------------------------------------------------------------------ |
| `data_hash`, `time_axis`                                                       | `utf8`                           | all                           | The array this series reads; joins to the values file.                         |
| `id`                                                                           | `int64`                          | all                           | Provenance only; §2.7.                                                         |
| `owner_id`                                                                     | `int64`                          | all                           |                                                                                |
| `owner_type`                                                                   | `utf8`                           | all                           |                                                                                |
| `owner_category`                                                               | `utf8`                           | all                           |                                                                                |
| `time_series_type`                                                             | `utf8`                           | all                           | Constant per file.                                                             |
| `name`                                                                         | `utf8`                           | all                           |                                                                                |
| `initial_timestamp`                                                            | `timestamp(ms, zone)`            | `SingleTimeSeries`, forecasts | Also inside `time_axis`; here as a real column so a reader need not parse one. |
| `length`                                                                       | `int64`                          | `SingleTimeSeries`            | Likewise.                                                                      |
| `resolution`                                                                   | `utf8` (`Period` storage string) | `SingleTimeSeries`, forecasts | Absent from the two irregular types' files.                                    |
| `interval`, `horizon`                                                          | `utf8`                           | forecasts                     |                                                                                |
| `count`                                                                        | `int64`                          | forecasts                     | Window count.                                                                  |
| `features`                                                                     | `utf8` (JSON object)             | all                           | `{}` when empty.                                                               |
| `element_type`                                                                 | `utf8`                           | all                           | Constant per file except composite width.                                      |
| `element_shape`                                                                | `utf8` (JSON array)              | all                           |                                                                                |
| `time_reference`                                                               | `utf8`                           | all                           | Constant per file; the literal `unspecified` when the series records none.     |
| `units`, `quantity_kind`, `unit_system`, `component_field`, `application_data` | `utf8`                           | all                           | Empty string when the row has none; §2.8.                                      |

`percentiles` is not a column: it is implied by the values rows. Every string column is
dictionary-encoded. Compression is zstd.

**Row groups** in the values file target roughly one million rows and are cut at an array-key
boundary whenever the coming group would carry the buffer past the target, so row-group statistics
on `data_hash` let a reader skip whole groups; only an array larger than the target on its own is
split. The series file is small and needs no policy. Both footers record
`rows_contiguous_by_key=true`.

### 2.5 Time spelling

`time_reference` is a partition key, so one partition has one spelling and the timestamp columns'
zone states it exactly: `Utc` is zone `UTC`; `FixedOffset` is the offset string; `Zone` is the IANA
name; `Zoneless` is a naive `timestamp(ms)`. A series that records **no** reference is written
UTC-zoned with the `time_reference` column and footer saying `unspecified`, the decision recorded in
Finding 7.12, and reads back as none. Because the spelling is a partition key, the export never
faces a mixed selection and never refuses one; `--spelling` remains a way to select fewer files.

### 2.6 Element values

Unchanged: the `value` column's Arrow type follows the element type alone.

| `element_type` and shape            | Arrow type of `value`                                                     |
| ----------------------------------- | ------------------------------------------------------------------------- |
| scalar dtype, shape `[]`            | primitive                                                                 |
| `tuple(N,T)`                        | `FixedSizeList<T>[N]`                                                     |
| scalar dtype with dense shape `[N]` | `FixedSizeList<T>[N]`                                                     |
| scalar dtype with shape `[M,N]`     | nested `FixedSizeList`                                                    |
| composite kinds                     | `FixedSizeList<float64>[w]`, stored packing, `w` the file's widest series |

Composites keep their stored packing (a `--decode` option remains a follow-up). Casting to a common
dtype is ruled out: the dtype round trip is a project promise.

### 2.7 Import: a merge join

The import opens a partition's two files together. Both are sorted by the array key, so it walks
them as a **merge join**: read the next values group into one array, then file every series row
carrying that key, then move on. Peak memory is one values group plus one row group of each file,
never a partition. The store's write path already recognizes an array it holds, so the second
through thousandth adds of one array are catalog inserts only.

Every series row must find its values group and every values group must be claimed by at least one
series row; either dangling side is an error naming the key. A key that reappears after another
key's rows, in either file, is refused with a message saying to sort by `data_hash`, `time_axis`; a
file re-sorted in a query engine must meet the same rule, which the docs state.

`add` never accepts an id, so the `id` column is ignored and reported at `--dry-run`. Identity on
import is the row's own `KeyIdentity` columns, as for any add; a duplicate is whatever the store
says it is.

`data_hash` is a **checksum** as well as a key: the import re-encodes the group, hashes it, and
refuses a mismatch naming the key. A user who edits values in DuckDB must recompute the hash or pass
`--no-checksum` (the existing `skip_checksum` option), in which case the key is still the join key
and only its meaning as a content hash is waived. For composite kinds the comparison is of the
decoded points, per §2.3.

**Foreign files.** A values file with no series file beside it is a foreign file: anything with a
`timestamp` and a `value` column. It is read as one series per distinct `(data_hash, time_axis)` if
those columns exist, or as exactly one series if they do not, and the inline flags supply what a
series row would have carried: `--name`, `--owner-id`, `--owner-type` are required, the element type
is inferred from the Arrow type (dense reading by default, `--element-type tuple(N,f64)` asserts),
`time_series_type` absent reads as `SingleTimeSeries` when the timestamps walk a grid and
`NonSequentialTimeSeries` otherwise (`PersistentTimeSeries` is never inferred), and `time_reference`
absent comes from the Arrow zone.

### 2.8 Absent descriptors

The five free-form descriptors (`units`, `quantity_kind`, `unit_system`, `component_field`,
`application_data`) are optional in the catalog. To keep every column required they are written as
the **empty string when absent**, and the import maps an empty string back to absent. The one
consequence, to document: a stored empty string round-trips as absent. `unit_system`'s unset state
means unspecified, not natural units, and the empty string preserves that.

### 2.9 Empty series

A series with no values has a catalog row and an array of length zero. It could be represented (a
series row whose values group has no rows) but it is not: **the export fails** if any selected
series is empty, naming every one of them, and writes nothing — the destination is left exactly as
it was found, which the stale-directory check (§2.10) makes easy, since it is empty. The import
likewise refuses a series row whose key matches no values group (§2.7). The remedy on export is to
narrow the selection past the empty series; the decision is recorded here so Finding 7.9's history
reads correctly: first refused on import for lack of an anchor, then warned about on export, now
refused on export.

### 2.10 CLI

- `infrastore export ... -f parquet --dir <DIR>` writes the partition file pairs into `<DIR>`.
  `-f parquet` to stdout stays refused. `--time-range` applies as it does for CSV. A `--dir` that
  already holds `.parquet` files is refused before the selection is resolved, so a selection that
  matches nothing cannot leave an earlier export standing.
- `infrastore add --parquet <PATH>` takes a **file, a directory, or a partition stem**; a directory
  imports every partition in it, each in its own transaction, all-or-nothing per partition. A values
  file named directly with no series file beside it is a foreign file (§2.7). `--dry-run` reports
  the partitions, distinct arrays, series counts, ignored ids, and any empty-string descriptors,
  streaming as the load does.
- Inline flags override a column for every series in the partition. `--descriptor` alongside
  `--parquet` stays refused (the series file carries the descriptors).

### 2.11 Python and Julia

`to_arrow()` and `from_arrow()` stay **per-series and unchanged**; they are in-memory conveniences,
not a file format. The relationship is documented in one sentence: a series file's columns are
`to_arrow()`'s metadata keys turned into columns, and the values file is its two columns keyed by
the array. No new Python or Julia code is part of this revision.

### 2.12 Phases

1. Values/series export for the three static types: the `time_axis` spelling per type with a test
   for each, the two column sets, sorting and row-group policy, footers, the empty-series refusal.
2. Import as a merge join: pairing by stem, streaming both files, the dangling-key errors, the
   contiguity refusal on both sides, the checksum, `--dry-run` reporting distinct arrays. Round trip
   of every static type, every element type in §2.6 including re-padded composites, every
   `time_reference` including unspecified, and a thousand series sharing one array, asserting the
   values file has that array once and the store holds it once.
3. Forecasts, export then import, all three dense kinds.
4. Foreign values files, with and without the key columns.
5. Remove the single-table writer and reader and any test or doc that describes them; amend Findings
   7.9, 7.10, 7.14, 7.16, 7.19, 7.20 with dated notes saying what this revision changes, without
   deleting their text.
6. Docs: the Parquet layout reference (both column sets, the array key, `time_axis` per type,
   pairing, the merge join, the empty-series rule), CLI guide and reference, `bindings.md`, and the
   DuckDB recipe as a `JOIN ... USING (data_hash, time_axis)` plus a view that flattens it.

## 3. Store-level attributes

### 3.1 What it is

A key/value table on the catalog for provenance a consumer wants stamped on the whole artifact:
creator, description, source system, the consumer's own schema version. The store never interprets a
value, in the same spirit as `application_data`.

### 3.2 Catalog

```sql
CREATE TABLE IF NOT EXISTS store_attributes (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
```

A new table, so the idempotent DDL creates it on any existing store and **no
`CATALOG_SCHEMA_REVISION` bump is needed**. Values are TEXT; a caller wanting structure stores JSON.
Keys with the prefix `infrastore.` are reserved and refused on write, leaving room for the store to
stamp its own facts later without colliding with a consumer's keys.

### 3.3 Core API

```rust
fn set_store_attribute(&mut self, key: &str, value: &str) -> Result<()>;
fn get_store_attribute(&self, key: &str) -> Result<Option<String>>;
fn list_store_attributes(&self) -> Result<BTreeMap<String, String>>;
fn remove_store_attribute(&mut self, key: &str) -> Result<bool>;
```

Writes take part in the ambient transaction like every other write and are refused on a read-only
store. Empty keys are `InvalidParameter`. `get` returns `None` for an absent key, since a consumer
asking is asking a question, mirroring `get_metadata_by_id`.

### 3.4 Interaction with existing operations

- `persist_to`, `persist_catalog`, `compact`, `open_copy`: preserved, because the catalog carries
  them. `open_without_catalog` mints an empty table.
- `merge`: the destination's attributes win; a source key the destination lacks is copied; a
  conflicting value is reported in the merge summary, not overwritten.
- `diff`: attributes are compared and reported in their own section, since they describe the
  artifact rather than a row.
- OpenAPI export/import: not carried. The vendored schema has no place for them.
- Parquet export (§2): out of scope for v1; the per-series footer describes a row, not the store.

### 3.5 Bindings

- **C ABI:** four exports mirroring §3.3, string results returned through the existing string
  out-parameter and deallocator pattern, `list` as a JSON object string to avoid a new
  array-of-pairs type. Full `# Safety` docs, header regenerated.
- **Julia:** `set_store_attribute!`, `get_store_attribute`, `list_store_attributes` returning a
  `Dict{String,String}`, `remove_store_attribute!`.
- **Python:** `Store.set_store_attribute`, `get_store_attribute`, `list_store_attributes` returning
  a `dict`, `remove_store_attribute`. Stubs and the drift guard updated.
- **CLI:** a `store-attr` command group with `get`, `set`, `list`, `remove` subcommands, honoring
  `-f`. The name avoids the existing `attributes` command, which lists supplemental attribute
  associations. `store-info` gains an `attributes` object.
- **gRPC:** read half only, `ListStoreAttributes` and `GetStoreAttribute`, named for the `Store`
  methods with `<Rpc>Req`/`<Rpc>Resp` messages per the existing convention.

### 3.6 Naming hazard

"Attribute" already means a supplemental attribute in this project and "metadata" means a
`TimeSeriesMetadata` row. Every new identifier carries the `store_` prefix so neither reading is
possible, and the docs introduce the term as **store attributes** once, in `data-model.md`.

### 3.7 Phases

1. Catalog table, core API, core tests including read-only refusal, reserved prefix, `merge`/`diff`
   behavior, and survival through `persist_to`/`compact`/`open_copy`.
2. C ABI plus Julia, then Python, each with tests.
3. CLI `store-attr` and `store-info`, gRPC read RPCs.
4. Docs: data model, per-binding references, CLI guide.

## 4. Considered and dropped: resampling on read

A `ReadWindow` option aggregating a `SingleTimeSeries` to a coarser period was proposed and
rejected. A Python user already has `to_arrow()` and from there pandas or polars resampling; polars
aggregates in vectorized Rust, so a core implementation would not be faster in any case that fits in
memory, which is every realistic series (a year of five-minute data is under a megabyte). The one
scenario where the core could win, aggregating arrays too large to materialize chunk by chunk from
HDF5, is not one this project's users have. Nor would the core be more correct: it deliberately does
not resolve zones, so it cannot do calendar-aware aggregation any better than a dataframe library.
When an aggregated series is wanted repeatedly, the store's answer is to aggregate once and write
the result back as its own row, which is what a persistence layer is for.

## 5. Deferred

Discussed, not planned here, in rough priority order: a Tables.jl interface for Julia (the analog of
`to_arrow()`); a bulk `extract --filter --time-range` into a new store; an advisory writer lock
beside the artifact; SQLite import via a query; the CLI `--endpoint` mode for which
`src/store_access.rs` is reserved; a TUI browser as a separate binary crate.

## 6. Handoff

- **Branch:** `feat/parquet-and-store-attributes`, based on `main`, in the worktree
  `/Users/dthom/repos/infrastore-parquet`. Work only there.
- **Order:** §3 (store attributes) first, then §2 (Parquet). Store attributes are smaller, touch
  every binding, and exercise the full cross-binding checklist before the larger feature starts.
- **Commits:** one commit per phase listed in §2.7 and §3.7, each passing every gate in §1. Commit
  messages explain the why, as the existing history does. Do not push and do not open a pull
  request; the branch is reviewed locally first.
- **Scope discipline:** touch nothing outside the two features. If a phase reveals a defect or a
  question the plan does not settle, record it in a `## 7. Findings` section at the end of this file
  with the decision taken, and keep going under the stated assumption rather than stopping.
- **Environment:** `maturin` is at `/Users/dthom/repos/infrastore/.venv/bin/maturin` and that venv
  has pyarrow, pytest, and numpy; `maturin develop` from the worktree installs into it. `julia` is
  on `PATH` via juliaup. `dprint` and `cargo-deny` are installed. The worktree has its own
  `target/`, so the first build compiles the vendored HDF5 (a few minutes).
- **Done means:** every phase committed, every gate green, `bindings.md`'s matrix and the
  per-binding references updated, and a final summary listing each commit, any findings, and
  anything left out with the reason.

## 7. Findings

Questions the plan did not settle, and the decision taken. Recorded as they came up, so the order is
the implementation order.

### 7.1 `is_empty` must probe `store_attributes` (§3, phase 1)

`MetadataStore::is_empty` documents itself as covering **every** persistent content table, and warns
that a table it misses is data a consumer drops with no error — infrasys skips writing an artifact
the store reports empty. Store attributes are the consumer's own text, put there by an explicit call
and recoverable from nowhere else, so a store holding nothing but provenance is not empty.
**Decision: probe it.** The only behavior change is for a store that has attributes and nothing
else, which no existing test or consumer can already have.

### 7.2 `merge` and `diff` are CLI commands, not core ones (§3.4, §3.7 phase 1)

§3.4 assigns `merge` and `diff` behavior to store attributes, and §3.7 puts it in phase 1 alongside
the core work. There is no `Store::merge` or `Store::diff`: both are `infrastore-cli` commands built
out of `list_metadata` + `read_by_ids` + `add_time_series_bulk`. **Decision:** the core phase covers
`persist_to` / `persist_catalog` / `compact` / `open_copy` / `open_without_catalog` survival, and
the merge/diff behavior lands with the rest of the CLI work in phase 3.

The behavior itself is as §3.4 specifies. `merge` is additive with the destination winning, reported
through `store_attributes_copied` and `store_attribute_conflicts` in the JSON status document and in
the prose form; merging twice therefore changes nothing the second time. `diff` gives them a section
of their own, and — a point §3.4 does not reach — counts a difference toward the **nonzero exit**.
The gate asks whether this is the artifact you expected, and one whose recorded source system
changed is not; reporting the difference while exiting 0 would make the section decorative.

### 7.3 `store-info` reports `store_attributes`, not `attributes` (§3.5)

§3.5 says "`store-info` gains an `attributes` object". §3.6 says every new identifier carries the
`store_` prefix so the supplemental-attribute reading is not available. In this CLI `attributes` is
already a command that lists component <-> supplemental-attribute associations, so the bare key is
exactly the collision §3.6 forbids. **Decision: `store_attributes`**, following §3.6 over §3.5's
wording.

### 7.4 CLI documentation lands with the CLI phase, not the docs phase (§3.7)

`crates/infrastore-cli/src/main.rs`'s
`every_command_is_shown_in_the_docs_and_every_doc_example_parses` test fails the build for a command
with no example in `docs/src/reference/cli.md`, `docs/src/guides/cli.md`, or the quick start — and
it runs every documented example through the real clap parser. **Decision:** the CLI reference and
guide entries for `store-attr` are part of phase 3, because phase 3 cannot pass its own gates
without them. The remaining documentation (data model, per-binding references, the `bindings.md`
matrix, the README) stays in phase 4.

One incidental constraint: the test splits examples on whitespace with no quote handling, so a
documented example cannot contain a quoted argument. The guide's examples use unquoted values.

### 7.5 `CLAUDE.md` updated alongside the docs (§6 scope discipline)

§6's "done means" lists `bindings.md` and the per-binding references but not `CLAUDE.md`, whose
project overview enumerates the same surface in prose. **Decision:** update it too. Leaving the file
that every future agent reads first describing a surface that has moved is a defect, and the edit is
one paragraph inside the two features' own subject matter.

### 7.6 The row-level footer keys have only one producer (§2.2)

§2.2 asks for `id`, `owner_id`, `owner_type`, `owner_category` and `features` in the footer, and
says to "add any missing key to `to_arrow()` in the same change so the two producers stay
identical." `to_arrow()` cannot produce four of those five: it is a method on a value object, and a
`SingleTimeSeries` built in Python is not filed anywhere — it has no owner, no catalog id, and no
feature map. **Decision:** the two producers agree on every key that describes the _values_, and the
CLI writes the row-level ones on top. The consequence is stated in the docs: a file written by
`export -f parquet` re-adds with no flags, while a `to_arrow()` file needs `--owner-id` and
`--owner-type`, exactly as a CSV does.

`element_shape` was genuinely missing from both and was added to both. It is written even when
empty, unlike the descriptors: an absent descriptor means "not declared", where an empty shape is a
fact about the data.

`features` is written as plain JSON scalars (`{"model_year":2030}`), not `FeatureValue`'s externally
tagged serde form (`{"model_year":{"Int":2030}}`). The plain form is what the C ABI's
`features_json` and the CLI's `--features` already use, and a foreign reader of this footer should
see the value rather than the discriminant carrying it.

**Superseded 2026-09-07 by the §2 revision.** There is no footer of descriptors any more: every one
of those keys is a **column** of the long table, and the row-level ones (`id`, `owner_id`,
`owner_type`, `owner_category`, `features`) are ordinary columns like the rest. The asymmetry the
finding recorded is gone along with the principle that created it — §2.9 withdraws "one schema, two
producers", so `to_arrow()` is no longer a second producer of the same artifact and has nothing to
stay identical to.

Two decisions survive the move and are now properties of columns rather than of footer keys.
`features` is still written as plain JSON scalars (`{"model_year":2030}`) rather than
`FeatureValue`'s externally tagged serde form, and for a stronger reason than before: this column is
read in DuckDB by hand. And `element_shape` is still written even when empty, as a footer entry
describing the partition rather than a row.

### 7.7 `Format::Parquet` exists in every build (§2.1)

Gating the clap variant on the `parquet` cargo feature would make `--help`, the shell completions,
and the documented examples differ between builds — and
`every_command_is_shown_in_the_docs_and_every_doc_example_parses` runs the documented examples
through the real parser, so a default-feature `cargo test` would fail on a documented `-f parquet`
example. **Decision:** the variant is unconditional and the export path fails with the flag that
turns the feature on. A binary without Parquet then says "rebuild with `--features parquet`" instead
of reporting `parquet` as an unknown format, which is the better error anyway.

`-f` is global, so the variant is refused for every command but `export`, once, in `run` — otherwise
each command's `match` would fall through to its `_` arm and quietly print a table.

**Amended 2026-09-07, after review: the `parquet` feature is now on by default.** Nothing above
changes — the clap variant was already unconditional, and it has to stay that way, because the
feature is still switchable and `--help`, the completions, and the documented examples must read the
same in a `--no-default-features` build. What changed is which build most people get: the shipped
binary and `cargo install infrastore-cli` both carry Parquet now, and the "rebuild with
`--features parquet`" refusal survives for the lean build rather than being the common case.

The reason for the original default was that Arrow is a large dependency tree, and that reason still
holds — for the _libraries_. `infrastore-core`, `infrastore-py`, and `infrastore-ffi` must never
link Arrow: a Python wheel or a Julia cdylib carrying it would be several times its current size for
a format both host languages already read, and both bindings can already produce the file through
`to_arrow()`. The check is `cargo tree --edges normal -p <crate> | grep -E 'arrow v|parquet v'`,
which must find nothing for those three. (Grep the versioned name, not the bare word: this
worktree's directory is called `infrastore-parquet`, so a path match is not evidence.)

No CI or release change was needed. Both `cargo build` steps in `.github/workflows/release.yml` use
`--all-features`, so the released `infrastore` binary already carried Parquet; nothing in the
workflows builds the CLI with `--no-default-features` or an explicit feature list.

**Still current after the §2 revision (2026-09-07).** Nothing here depends on the file layout: the
clap variant is unconditional, `-f parquet` is refused for every command but `export`, and a build
without the feature names the feature to rebuild with. The revision changes what `export` then
writes, not whether the flag parses.

### 7.8 `tiny-keccak` is CC0-1.0, allowed as a scoped exception (§1)

Arrow reaches it through `arrow-array -> ahash -> const-random -> const-random-macro`, a build-time
proc macro. CC0-1.0 is a public-domain dedication, strictly more permissive than everything in the
allowlist. **Decision:** allow it as a `[[licenses.exceptions]]` entry naming the crate, not by
adding CC0-1.0 to `allow`. CC0's fallback license grant explicitly does not grant patent rights,
which is why several organizations treat it as a review item rather than as a plain permissive
license — so a future CC0 dependency should get its own look rather than inheriting this one's.

### 7.9 Two refusals the plan did not anticipate (§2.5)

An **empty `SingleTimeSeries`** cannot be imported: the timestamp column _is_ its anchor, and an
empty table has nowhere to put one. Refused with a message saying so, rather than defaulted to an
epoch nobody chose. The two irregular types need no anchor and import fine. (Adding
`initial_timestamp` to the footer would fix it, but that key would have to land in both producers
and is not what §2.2 asks for.)

A **type that cannot be inferred**: with no `time_series_type` in the footer, a grid reads as
`SingleTimeSeries` and anything else as `NonSequentialTimeSeries`. `PersistentTimeSeries` is never
inferred — it is structurally identical to the irregular type and differs only in what the values
mean between rows, so guessing it would be guessing that. `--type` names it.

**Half superseded 2026-09-07 by the §2 revision.**

The **empty-series** refusal is gone, and §2.8 says so outright: a long table has one row per value,
so a series with no values contributes no rows and is simply not in the output. The export warns,
naming it, and the question of anchoring an empty `SingleTimeSeries` on import never arises.

The **`PersistentTimeSeries` is never inferred** half stands unchanged, and §2.8 restates it. It is
structurally identical to `NonSequentialTimeSeries` and differs only in what the values mean between
rows, so guessing it would be guessing that; `--type` names it. The type is a partition key now, so
a file this project wrote always states it.

**Reversed again 2026-09-07 by the normalized layout.** The empty-series rule has now been all three
things in turn: refused on import for want of an anchor, warned about on export, and — as of §2.9 —
**refused on export**, naming every empty series and writing nothing. A partition is a values file
and a series file that must describe the same arrays, so an empty series is a series row whose key
matches no values group, which is exactly what a truncated export looks like. There is no way to
represent one that a reader could tell apart from damage, so the export refuses rather than writing
something ambiguous. The remedy is to narrow the selection past it.

The `PersistentTimeSeries` half is untouched and still current.

### 7.10 A dense forecast's grid is required, not inferred (§2.4)

§2.4 specifies the long table but not what the import may assume. Resolution, horizon, interval,
window count and the percentile list could be reverse-engineered from a complete set of rows, but a
merely self-consistent set would give a plausible wrong answer: a one-window forecast is
indistinguishable from a static series, and overlapping windows make the interval ambiguous.
**Decision:** the footer's forecast parameters are required, and a long table without them is
refused naming the missing key. Rows are then placed by their coordinates rather than their order,
so a file a query engine rewrote still reads correctly.

**Still current after the §2 revision (2026-09-07), with the source moved.** The grid is still
required rather than reverse-engineered, for the reason the finding gives: a merely self-consistent
set of rows would give a plausible wrong answer. What changed is where it comes from — `resolution`,
`interval` and `horizon` are **columns** now, not footer keys, so a query that selects a subset of a
forecast's rows carries the grid with them.

The revision also tightened one thing the finding claimed. Placing rows by their coordinates is only
order-independent if the coordinates themselves are, and the first cut of the long-table import took
the window anchor from the first issue time it saw and the percentile labels in first-appearance
order — both of which depend on file order. Both are now derived from the values (minimum issue
time, sorted percentiles), which is what the claim was always meant to say.

**Still current under the normalized layout (2026-09-07), with the source moved once more.** The
grid columns now live in the **series file**, one row per series, rather than repeating beside every
value — which is the whole point of normalizing, and does not change the finding: they are still
required, and a partition whose series file lacks `horizon` is still refused naming it. The
coordinates in the values file are still what places each row, and are still derived
order-independently.

One thing the move improves: a query that selects a subset of a forecast's values rows no longer
carries the grid with it, because the grid was never in those rows. It carries the array key, and
the grid is one join away.

### 7.11 `to_arrow_windows()` does not gain a long form (§2.4)

§2.4 asks this phase to settle it. **Decision: no.** It returns a dict of per-window tables, which
is an in-memory analysis shape and could never be one Parquet file — so it is not a competing
spelling of the file format and "one schema, two producers" has nothing to reconcile. The long table
has one producer, the CLI, and one consumer, the CLI. A Python `to_arrow_long()` /
`from_arrow_long()` pair is a reasonable follow-up; §2.6 scopes Python's Arrow inverse to the three
static types, and that is what phase 3 delivered.

**Reframed 2026-09-07 by the §2 revision.** The decision stands, and its reason is now the project's
stated position rather than one phase's judgement: §2.9 withdraws "one schema, two producers"
outright and says `to_arrow()` and `from_arrow()` are per-series in-memory conveniences rather than
a file format. So there is no principle left for `to_arrow_windows()` to violate and no long form
for it to grow. A forecast Parquet file comes from `export -f parquet` and goes back through
`add --parquet`.

### 7.12 An unspecified `time_reference` comes back as `utc` (§2.2)

Arrow's timestamp type has a zone or it has none, and _unspecified_ has no third spelling.
`to_arrow()` has always mapped it to a UTC-zoned column, and this export follows — so the import
reads `utc` back. The instants are unchanged; only the label moves from "not stated" to "UTC".
**Decision:** keep the existing mapping and document the asymmetry rather than inventing a
`zoneless`-shaped column for "unspecified", which would collide with the real `zoneless` and be
worse.

**Fixed 2026-09-07, after review.** Documenting the asymmetry was the wrong call: a round trip that
turns "the series declared nothing" into "the series declared UTC" invents a claim, and a consumer
that reads `time_reference` to decide whether it may trust a bound would be misled by it.

The premise stands — Arrow has no third spelling and the column stays UTC-zoned — so the fix is in
the **footer**, which has no such limit. `time_reference` is now written for every series and holds
the literal `unspecified` when the series records none, in both producers; both consumers decode
that literal back to `None`, and it beats the column's own zone. A file with no `time_reference` key
at all — a foreign one — keeps the existing Arrow-zone inference, so nothing about reading someone
else's Parquet changed.

The literal is defined once per crate (`infrastore_parquet::schema::UNSPECIFIED_REFERENCE` and a
private `UNSPECIFIED_REFERENCE` in `infrastore-py`) and pinned to the same string by a test on each
side, because the two are compiled separately and a silent divergence would look exactly like the
bug this replaces.

`TimeReference::parse` was deliberately **not** taught the literal. Unspecified is `None`, not a
fourth variant, and the CLI's `--time-reference` must go on meaning what it meant. Note what that
implies and what it does not: the core validates a zone name's shape and never resolves it, so
`TimeReference::parse("unspecified")` already returned `Zone("unspecified")` and still does — the
footer decoder intercepts the literal itself rather than delegating, which is what leaves the core
untouched. One collision follows and is accepted rather than engineered around: a series whose
reference is literally `Zone("unspecified")` writes the same footer value and comes back as
unspecified. It is not an IANA zone, so nothing could ever resolve such a reference, and every
alternative encoding is collidable the same way.

**Carried forward 2026-09-07 into the §2 revision.** The fix survives the rework unchanged in
substance, and the rework strengthens it: `time_reference` is a **partition key**, so a series that
declares no spelling gets a file of its own rather than sharing one with a series that declared UTC,
and the `unspecified` literal appears in both the column and the footer. §2.4 states it as part of
the format. The literal is still not a `TimeReference` and `TimeReference::parse` is still
untouched, so the collision noted above — a series whose reference is literally
`Zone("unspecified")` — is still accepted rather than engineered around, for the same reason.

### 7.13 Two partitions can want one filename (§2.2, decided 2026-09-07)

§2.2 says the slug function is one-way and the footer carries the exact key, which leaves open what
happens when two partitions slug alike — and they can: the zones `a/b` and `a_b` both flatten to
`a_b`. Writing both would mean the second silently overwrote the first. **Decision:** collisions get
a numeric suffix, assigned in the partition keys' own sort order so a re-run of the same export
produces the same names. `disambiguate` is the function and it is tested from both input orders.

Ordering the keys needed a total order over `TimeSeriesType` and `TimeReference`, neither of which
is `Ord` and neither of which should become one for this crate's convenience — neither has a
meaningful order of its own. `PartitionKey` implements `Ord` over a rendered form instead.

### 7.14 A composite series' `data_hash` is not the catalog's (§2.6, decided 2026-09-07)

§2.6 says the import "compares the hash of the **decoded** points for composite kinds rather than of
the packed bytes", which is only coherent if the export writes that hash — there is nothing else for
the import to compare a decoded-points hash against. **Decision:** the `data_hash` column holds
`hash(canonical(array))`, where the canonical form is the array itself for every kind but the
composite ones and its minimum-width re-encoding for those. `encode(decode(x))` drops the padding,
because `encode` derives the width from the widest timestep.

The consequence to know, and the format reference says it: for a composite series this column is
**not** the `data_hash` the catalog holds, and `id` is the way back to that. Every other kind's is
identical to the catalog's.

**Now load-bearing rather than incidental (2026-09-07).** Under the normalized layout the canonical
hash is not merely what the checksum compares — it is half the **array key**, so two composite
series whose curves are the same points at different paddings land in one values group and are
written once. The finding's consequence stands unchanged and matters more: for a composite series
the `data_hash` column is not the catalog's, and `id` is the way back to that.

### 7.15 A missing owner is refused, not defaulted (§2.8, decided 2026-09-07)

§2.8 says "a missing `name` or `owner_id` is an error". Before that was implemented a foreign file
with neither filed itself silently as `''` under owner 0 — owner 0 is a real owner, not a sentinel,
so the series landed somewhere nobody named. **Decision:** all three of `name`, `owner_id` and
`owner_type` are refused when neither the file nor a flag supplies them, each naming the flag that
would. `owner_type` is the addition to §2.8's list, on the same reasoning: a series owned by `""` is
not something any consumer means, and `AddRequest` has nowhere to put "unknown".

This is why the import's `owner_id` is `Option<i64>` internally rather than defaulting to 0: "the
file has no such column" and "the file says owner 0" have to stay distinguishable.

### 7.16 Placing rows by coordinate requires order-independent coordinates (§2.6, found 2026-09-07)

§2.6 asks the forecast import to place rows by their coordinates so that a file a query engine
rewrote still reads. The first implementation did not deliver that: the window grid's anchor was the
**first issue time seen** and the percentile labels were in **first-appearance order**, so a
reversed file produced a wrong answer rather than an error. **Decision:** both are derived from the
values — the minimum issue time, and sorted percentiles. Sorting the percentiles loses nothing,
because the core already refuses a `Probabilistic` whose percentiles are not strictly increasing, so
there is no stored order to preserve.

**Still current, and now asserted with the checksum live (2026-09-07).** The order-independence test
used to read a reversed file with `data_hash` projected away. Under the normalized layout dropping
that column turns the file into a foreign one, so the test edits the exported values file in place
and reads the pair — which checks the stronger claim the finding was always making: a reversed file
rebuilds to the same _bytes_, not merely to something that parses.

### 7.17 A forecast's per-step shape is not the catalog's `element_shape` (§2.2, found 2026-09-07)

The catalog stores `TypedArray::element_shape` — everything after the leading axis — which is the
per-step shape only for a static series. A `Deterministic`'s cube is `[H, count, *E]`, so the
catalog records `[count, *E]`, and keying the partition on it would have produced a `value` column
claiming the window count is part of one timestep. **Decision:** `per_step_shape` counts from the
type's own `leading_dims`, and the same correction applies to the composite width a partition
settles on.

### 7.18 `-f parquet` names the payload, not the status report (§2.8, decided 2026-09-07)

`export` prints a status document saying what it wrote. `-f` selects both the payload format and the
report's, which cannot both be Parquet. **Decision:** the report renders as a table unless
`-f json`/`-f jsonl` was asked for, so `-f parquet export` prints a readable summary and
`-f json ... export` still gives a script one object — with `partitions`, `rows` and `empty` in it,
since the partition layout is what a caller most wants to know afterwards.

### 7.19 What the first code review found (fixed 2026-09-08)

The PR's automated review found eight defects in the long-table implementation, all confirmed
against the code and fixed in one commit, each with a test:

- `PartitionKey`'s `Ord` was derived from the filename slug while `Eq` compared the exact reference,
  so two zones that slug alike (`a/b`, `a_b`) compared `Equal` and the export's `BTreeMap` pooled
  them into one file with one timestamp zone. The order now keys the reference by variant and
  storage string; only the filename is sanitized.
- A collision suffix (`_2`) was not itself reserved, so a key whose natural slug was that name
  overwrote the moved file. Every name handed out is tracked, and a candidate increments until free.
- A null in a text column read as the empty string, and a null in an integer column as whatever the
  buffer held, despite the format's no-nulls rule; in an identity column that filed rows under a
  different series. Both are refused, naming the column.
- The documented foreign-file fallback for `time_reference` (the Arrow zone when neither column nor
  footer says) returned `None`, relabeling a zoned foreign file as unspecified. The grouper keeps
  the timestamp column's zone and derives `utc`, `zoneless`, or the named zone from it.
- Exporting into a directory that already held `.parquet` files left the stale partitions for the
  next `add --parquet <dir>` to import. The export refuses such a directory; §2.8 and the CLI
  reference say so.
- `bindings.md` still described "one schema, two producers", and a CLI reference example named a
  per-series file the partitioned export never writes.

**Six of the eight survive the normalized layout unchanged (2026-09-07)**: the `PartitionKey` order,
the collision-suffix reservation, the no-nulls refusal, the Arrow-zone fallback, and the
stale-directory check are all still in force and still tested. The null refusal moved with its
columns — a null `units` or `owner_id` is now a **series file** defect, since those columns are no
longer beside every value — and the eighth, the documentation fix, is superseded by the layout
rewrite the docs phase carries.

### 7.20 What the second review found (fixed 2026-09-08)

Two posted comments and six the reviewer had missed the first time, all confirmed and fixed with
tests:

- The CLI parsed **every** Parquet file before writing any, so a malformed later partition failed
  the whole load with nothing committed, contrary to the documented one-transaction-per-file rule,
  and every partition's decoded data was held at once. The reader now has a streaming form,
  `read_file_with`, that hands each series to a sink the moment its rows end; the CLI opens one
  transaction per file, adds each series as it arrives, and commits or rolls back per file. The
  collecting `read_file` remains for tests, and the dry run streams too, keeping one summary line
  per series rather than its values.
- `export -f parquet` returned early on an empty selection before the writer's stale-directory check
  ran, so "exported 0" could leave an earlier export's partitions in place. The check runs before
  the selection is resolved; `check_destination` is public for that.
- Row groups were cut only after the next series had been appended, so a 900k-row series followed by
  a 200k-row one produced a group ending 100k rows into the second. The buffer is now cut at the
  boundary whenever the coming series would carry it past the target; only a series larger than the
  target on its own is split.
- Three comments (the crate manifest, its crate-level doc, `deny.toml`) still said the CLI feature
  was off by default, and a proto field comment promised an order a protobuf map cannot carry.

**All four survive the normalized layout (2026-09-07), two of them enlarged.** Streaming is now a
merge join over two files rather than a group-by over one, and `read_partition_with` replaces
`read_file_with` with the same sink contract; one transaction per **partition** replaces one per
file. The row-group policy is unchanged and applies to the values file, where boundaries are array
keys rather than series — which is what makes a row group's `data_hash` statistics worth having. The
stale-directory check and the comment fixes are untouched.

### 7.21 `time_axis` comes from the values, not from the catalog row (§2.3, decided 2026-09-07)

The first cut of `time_axis_of` read the catalog row: `length`/`initial_timestamp`/`resolution` for
a `SingleTimeSeries`, `row.timestamps` for the irregular pair. Both are wrong sources.

`list_metadata` leaves `TimeSeriesMetadata::timestamps` unpopulated — materializing every irregular
axis to list a catalog would be absurd — so `export -f parquet` of a `NonSequentialTimeSeries`
failed with "its catalog row carries no timestamps". And `export --time-range` hands back a _slice_,
whose anchor and length are its own and not the stored series'; an axis read off the catalog would
have claimed a grid the values file does not hold, while `data_hash` beside it is already recomputed
from the sliced array.

So `time_axis_of` takes the `TimeSeriesData` being exported, and the series file's
`initial_timestamp`, `length` and `count` columns come from the same place (`table::grid_of`). The
two sources agree for a whole-series export, which is every existing test; where they disagree the
values are the ones the file holds. The spelling is unchanged, so an irregular series' axis is still
exactly the catalog's `timestamps_hash` in hex.

### 7.22 `--no-checksum` is a real CLI flag (§2.7, §2.10, decided 2026-09-07)

§2.7 tells a user who edited values in DuckDB to "recompute the hash or pass `--no-checksum` (the
existing `skip_checksum` option)", but §2.10 never lists the flag and the option was previously
library-only, hard-wired to `false` — the old remedy was to drop the `data_hash` column. That remedy
is gone: dropping the key column from a values file makes it a **foreign** file, which then needs
`--name`, `--owner-id` and `--owner-type` and loses the series file entirely. `add --no-checksum`
therefore exists, and waives only the comparison; the pair is still the join key.

### 7.23 Foreign values files land with the merge join, not one phase later (§2.12 phases 2 and 4)

Phase 2 replaces the single-table reader outright, so the old reader's foreign-file path had to go
with it rather than be kept alive for a phase. Reading a lone values file as one group per distinct
`(data_hash, time_axis)`, or as exactly one group when those columns are absent, is two lines of the
same `ValuesReader`, so it is here. Phase 4's remaining work is the tests §2.12 names — foreign
files with and without the key columns — plus whatever they turn up.

### 7.24 Forecast round trips stay `#[ignore]`d for one more phase (§2.12 phases 2 and 3)

Phase 2 is scoped to the static types, so the nine forecast tests in `long_round_trip.rs` and the
CLI forecast test keep an `#[ignore]` whose reason now names phase 3. They still compile against the
new reader, so the attribute is the only thing phase 3 removes. (Phase 5 renamed the file to
`partition_round_trip.rs`, and `long_export.rs` to `partition_export.rs`, since "long table" is not
what either describes any more.)

### 7.25 A partition's halves must agree before they are joined (§2.7, decided 2026-09-08)

The merge join pairs by **file name**, and a name is easy to arrange by accident: copy one half of
one export next to the other half of another and the stems match while the contents do not. If the
two happen to share array keys the join succeeds and the checksum passes — the values really do hash
to what the values file says — while every catalog field comes from the wrong export. Pairing a UTC
values file with an `unspecified` series file changes a series' `time_reference` silently, and the
values file has nothing in it that could object.

**Decision:** compare the footers before streaming. Each half carries the role it plays and the
whole `PartitionKey` — `time_series_type`, `element_type`, `element_shape`, `time_reference` — so a
disagreement in any of them is refused naming the field and both files. `SeriesReader` keeps its
footer for this rather than dropping it after the version check. Two **unmarked** files named as a
pair are left alone, since a pair of foreign files is not something this format can have opinions
about; one marked and one not is refused, because our export writes the marker on both.

### 7.26 Every column of a marked file is checked before a row is read (§2.4, found 2026-09-08)

"Every column is required" is the payoff the partitioning buys, and the reader was not keeping it:
`text_column` returns `None` for an absent column, `cell` turns that into the empty string, and
`build` reads the empty string as absent. So a series file with `features` projected away imported
an empty feature set — and `features` is part of a series' `KeyIdentity`, so every row landed under
an identity nobody named, without a word.

**Decision:** a file carrying the format marker is held to the complete per-type column set for its
role, checked by name against the Arrow schema before any row is read, and a missing column is
refused naming it. `table::required_columns` is the single list, and a unit test asserts it equals
what `PartitionSchema::new` emits for every type and both roles, so the two cannot drift. Files
**without** the marker keep every fallback: a foreign file is allowed to carry two columns and let
the inline options say the rest, which is the whole point of §2.7's foreign-file rules.

### 7.27 The grid flags are refused with `--parquet`, not dropped (§2.10, decided 2026-09-08)

§2.10 says "inline flags override a column for every series in the partition", and `--parquet`
honoured six of them and silently ignored the rest. Three groups, three answers:

- **Overrides.** `--units`, `--quantity-kind`, `--unit-system`, `--component-field` and
  `--application-data` now reach the reader through `ImportOptions` and replace the column for every
  series, as §2.10 already promised. An empty string clears the descriptor, since the empty string
  is how this format writes "absent" (§2.8).
- **Assertions.** `--element-shape` and `--resolution` join `--element-type` and `--type`: they must
  equal what the file implies, a contradiction is an error rather than a silent replacement, and on
  a foreign file — which says nothing to contradict — they are what names the reading. A
  `--resolution` on a foreign grid file is then checked against the grid it _generates_, not against
  successive differences.
- **Refused.** `--initial-timestamp`, `--interval`, `--horizon`, `--count`, `--percentile`,
  `--scenario-count`, `--layout`, and (added on the same reasoning, since they are `--layout`'s
  companions) `--owner-map` and `--owner-id-from`. The values imply the grid — a `SingleTimeSeries`'
  anchor is its first timestamp, a forecast's windows are its `issue_time` column — so a flag naming
  one is either redundant or a contradiction nothing should have to adjudicate, and `--layout`
  describes a CSV's columns, which a Parquet file does not have.

The consequence to record: **a foreign forecast is not supported.** A values file with an
`issue_time` column and no series file beside it has the coordinates but not the grid, and §2.7
gives no way to supply one — the refused flags are exactly what would have to describe it. Adding it
would mean accepting `--interval`, `--horizon`, `--count` and `--percentile`/`--scenario-count` for
a foreign file only, building the lane labels from the flags rather than the rows, and deciding what
a partial cube means; none of that is asked for, and a forecast that came from an infrastore export
has its series file. The remedy today is to keep both halves.

### 7.28 A lean build must refuse a Parquet export before the selection (§2.10, fixed 2026-09-08)

`check_parquet_destination` runs before the selector is resolved, so that "exported 0" cannot leave
an earlier export standing (Finding 7.20). In a build without the `parquet` feature it returned
`Ok`, and the empty-selection path then returned before ever reaching `write_parquet` — so
`-f parquet export --dir` with a selector matching nothing _succeeded_ on a binary that cannot write
Parquet at all, reporting a zero. It now returns the same refusal, and the message itself is one
string in `commands::without_parquet` rather than two copies that had already drifted apart in
wording. A `#[cfg(not(feature = "parquet"))]` test in `cli_workflows.rs` covers both the empty and
the matching selection, and `add --parquet` saying the same thing.
