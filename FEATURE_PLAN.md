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

### 2.1 Where the code lives

A new workspace crate, **`infrastore-parquet`**, depending on `infrastore-core` and on the `arrow`
and `parquet` crates with default features off (enable only the `snap` and `zstd` codecs). The CLI
depends on it behind a `parquet` cargo feature, **off by default**, so the default `infrastore`
binary and every other crate stay free of the Arrow dependency tree. A separate crate rather than a
core feature keeps core's feature surface flat and makes the optional dependency visible in the
workspace graph.

Check before adding: the pinned `arrow`/`parquet` versions must compile on the declared MSRV (Rust
1.94). arrow-rs moves its MSRV quickly; pin a version that fits rather than raising `rust-version`.

### 2.2 One schema, two producers

Python already has `to_arrow()` on the three static types, behind the `arrow` extra, and a user can
write Parquet today with `to_arrow()` plus `pyarrow.parquet.write_table`. **The CLI export must
produce the same schema.** That is the governing principle: a Parquet file has one shape whether
Python or the CLI wrote it, and the import reads that one shape. Concretely the export mirrors what
`python/tests/test_arrow.py` already pins:

- `timestamp`: Arrow `timestamp(ms, tz)`, the zone taken from the series' `time_reference` the way
  `to_arrow()` already does (`Utc` and `Zone` become an Arrow zone, `FixedOffset` an offset string,
  `Zoneless` no zone).
- `value`: the type follows the `element_type` and `element_shape`, see §2.3.
- Schema metadata: the row's descriptors as UTF-8 key/value pairs, exactly the keys `to_arrow()`
  writes today (`name`, `units`, `quantity_kind`, `unit_system`, `component_field`,
  `application_data`, `element_type`, ...), plus the ones import needs and `to_arrow()` may not yet
  write: `id`, `owner_id`, `owner_type`, `owner_category`, `time_series_type`, `time_reference`,
  `element_shape`, `features`. Add any missing key to `to_arrow()` in the same change so the two
  producers stay identical. `time_reference` must be spelled explicitly because an Arrow timestamp
  without a zone cannot distinguish `Zoneless` from unspecified.

One file per series, which is what `export --dir` already does for CSV and what makes the footer a
faithful copy of one catalog row. A combined multi-series file (an `id` column plus a footer array
of rows) is a possible later option, not part of this plan.

### 2.3 Element values

The `value` column's Arrow type is decided by the element type alone; the time series type only adds
key columns. The tuple and dense cases are the same bytes and the same Arrow type, told apart by the
`element_type` metadata, exactly as the element-type reference describes:

| `element_type` and shape                                                      | Arrow type of `value`                                                  |
| ----------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| scalar dtype, shape `[]`                                                      | primitive (`float64`, `int32`, `bool`, ...)                            |
| `tuple(N,T)`                                                                  | `FixedSizeList<T>[N]`                                                  |
| scalar dtype with dense shape `[N]`                                           | `FixedSizeList<T>[N]`                                                  |
| scalar dtype with shape `[M,N]`                                               | `FixedSizeList<FixedSizeList<T>[N]>[M]` (nested, as `to_arrow()` does) |
| `linear_function`, `quadratic_function`, `piecewise_linear`, `piecewise_step` | `FixedSizeList<float64>[w]` holding the stored packing                 |

**Decision: composites keep their stored packing in v1.** This revises the earlier suggestion of
decoding them into `Struct` and `List` columns. `to_arrow()` already pins the packed form with a
test, the packing is the documented cross-language wire form held to
`conformance/element_type_vectors.json`, and every binding has a decoder for it. Changing
`to_arrow()` would break the one-schema principle for a benefit that mostly reaches foreign readers
of function-data series, which are rare. A `decoded` option (`--decode` on the CLI, `decoded=True`
on `to_arrow()`) that emits `Struct{proportional, constant}`, `List<Struct{x, y}>` and friends is
the natural follow-up, and it is out of scope here.

Columns are written as **required** (non-nullable). The store has no nulls; NaN is a value.

### 2.4 Export

`infrastore export ... -f parquet --dir <DIR>` writes `<id>.parquet` per matched series, reusing
`export`'s selection and `--time-range` path (`read_by_ids_range`). Phase 1 covers the three static
types, matching `to_arrow()`'s coverage. Phase 2 adds dense forecasts as a long table with
`issue_time` and `target_time` columns, plus `percentile` or `scenario` for `Probabilistic` and
`Scenarios`; `Deterministic` gets the same long shape rather than `to_arrow_windows()`' one table
per window, and the plan for that phase should say whether `to_arrow_windows()` gains a long form to
keep the two producers aligned.

`-f parquet` on stdout is refused: Parquet needs a seekable sink for its footer.

`PersistentTimeSeries` exports like `NonSequentialTimeSeries` (breakpoint column named `timestamp`)
with `time_series_type` in the metadata telling them apart, since the two share storage and differ
only in read semantics.

### 2.5 Import

`infrastore add --parquet <FILE>` becomes a third `add` layout beside the timestamped and wide CSV
ones. The rules:

- **Schema metadata is the descriptor.** When the footer carries the keys from §2.2 the file is
  self-describing and no `--descriptor` is needed; the descriptor or inline flags may still override
  a key. Since `add` never accepts an id, the `id` key is ignored on import and reported at
  `--dry-run`. A file exported by `export` therefore round-trips through `add` with no flags.
- **Foreign files infer.** With no metadata, the element type is inferred from the Arrow type:
  primitive to its dtype; `FixedSizeList<T>[N]` to dtype `T` with `element_shape [N]` (dense, not
  tuple, because the bytes cannot say and dense is the weaker claim); nested fixed-size lists to a
  multi-dim shape. `--element-type tuple(N,f64)` asserts the tuple reading, and as everywhere else
  in the project a contradicting assertion is an error, not an override. `Struct` and `List` value
  columns are refused in v1 (they are the decoded form §2.3 defers).
- **Timestamps** of unit seconds or milliseconds are accepted; microseconds or nanoseconds only when
  every value is a whole millisecond, otherwise refused with `InvalidParameter`, the same
  millisecond rule the write path enforces. The Arrow zone becomes the `time_reference` unless the
  `time_reference` metadata key names it, which wins. A regular series' timestamps must sit on a
  grid, reusing the check the timestamped CSV layout already performs.
- **Nulls are refused**, not coerced to NaN.
- **Round-trip caveat to document:** values round-trip exactly (an improvement over CSV, where
  floats pass through decimal text), but a composite series re-encodes at the minimum padding width,
  so one stored wider comes back with a different `data_hash`. Its id survives only through the
  rows-only OpenAPI import; a plain `add` assigns a fresh one.

### 2.6 Python

`SingleTimeSeries.from_arrow(table, **overrides)` and the same on the two irregular types, as the
Python inverse of `to_arrow()`, pure Python on top of the existing constructors and applying the
§2.5 inference rules. The rules live in one place in the docs so the CLI and Python implementations
cannot drift; a pytest round-trips a CLI-written file through `from_arrow` and a `to_arrow()` file
through `add`.

### 2.7 Phases

1. `infrastore-parquet` crate, static-type export, `to_arrow()` metadata parity, CLI `-f parquet`.
2. Import of self-describing and foreign files, `--dry-run`, round-trip tests against CSV export.
3. Python `from_arrow`.
4. Dense forecasts, export then import.
5. Docs: CLI guide and reference, Python guide, `bindings.md` matrix row, a DuckDB recipe showing
   Parquet plus the attached SQLite catalog (the reason DuckDB is not embedded).

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

### 7.10 A dense forecast's grid is required, not inferred (§2.4)

§2.4 specifies the long table but not what the import may assume. Resolution, horizon, interval,
window count and the percentile list could be reverse-engineered from a complete set of rows, but a
merely self-consistent set would give a plausible wrong answer: a one-window forecast is
indistinguishable from a static series, and overlapping windows make the interval ambiguous.
**Decision:** the footer's forecast parameters are required, and a long table without them is
refused naming the missing key. Rows are then placed by their coordinates rather than their order,
so a file a query engine rewrote still reads correctly.

### 7.11 `to_arrow_windows()` does not gain a long form (§2.4)

§2.4 asks this phase to settle it. **Decision: no.** It returns a dict of per-window tables, which
is an in-memory analysis shape and could never be one Parquet file — so it is not a competing
spelling of the file format and "one schema, two producers" has nothing to reconcile. The long table
has one producer, the CLI, and one consumer, the CLI. A Python `to_arrow_long()` /
`from_arrow_long()` pair is a reasonable follow-up; §2.6 scopes Python's Arrow inverse to the three
static types, and that is what phase 3 delivered.

### 7.12 An unspecified `time_reference` comes back as `utc` (§2.2)

Arrow's timestamp type has a zone or it has none, and _unspecified_ has no third spelling.
`to_arrow()` has always mapped it to a UTC-zoned column, and this export follows — so the import
reads `utc` back. The instants are unchanged; only the label moves from "not stated" to "UTC".
**Decision:** keep the existing mapping and document the asymmetry rather than inventing a
`zoneless`-shaped column for "unspecified", which would collide with the real `zoneless` and be
worse.
