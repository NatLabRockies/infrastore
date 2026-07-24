# Plan: test-coverage buildout — corner cases, failure paths, binding parity

Execute the medium-term **test** work from `TEST_COVERAGE_ASSESSMENT.md` (§2 is the evaluated gap
list; §3 the agreed split). This plan is self-contained: each work item carries the context needed
to implement it without re-deriving the assessment. Scope is tests plus the small enabling code
changes listed in Phase 0 — nothing else changes.

## 1. Ground rules

- **Pin, don't fix.** Many items below test behavior that is currently _undefined_ (empty arrays,
  hostile names, torn artifacts). The job is to pin what the code does today with a test and a
  comment saying so. If a test reveals a genuine defect, do **not** change core behavior: record it
  in the Findings log (§9), write the test to assert the current (wrong) behavior with a
  `// FINDING:` comment referencing the log entry, and move on. The only authorized non-test code
  changes are in Phase 0 and the explicitly flagged dev-dependency additions.
- **Never** change: `DATA_FORMAT_VERSION`, the proto surface, public API signatures, on-disk layout,
  error variants, or FFI exports. Never hand-edit `crates/castore-ffi/include/castore.h`.
- **Quality gates after every phase** (all must pass before the phase's commit):

  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-features
  dprint check
  cargo deny check --config deny.toml
  ```

  Python phases additionally: `maturin develop --manifest-path crates/castore-py/Cargo.toml` then
  `pytest python/tests`. Julia phases: `cargo build -p castore-ffi --release`, export
  `CASTORE_LIB=$PWD/target/release/libcastore_ffi.dylib` (`.so` on Linux), then
  `julia --project=julia/Castore.jl julia/Castore.jl/test/runtests.jl`.
- One commit per phase, message prefixed `test:` (Phase 0: `refactor:`). Do not bypass a failing
  pre-commit hook.
- **Cross-platform**: CI runs Linux/macOS/Windows. Gate permission-bit tests with `#[cfg(unix)]`
  (precedent: `read_only_open_works_on_write_protected_files`), use `tempfile`, never hard-code path
  separators.
- **Julia style**: never vertically align `::`/`=` into columns (Blue style, plain `field::Type`).
- **Python style**: optional arguments are keyword-only; if any test file touches the public surface
  list, `python/tests/test_stubs.py` must stay green (it diffs `castore.pyi` member sets).

## 2. Phase 0 — harness fixes (authorized code changes)

- **0.1 De-duplicate the `slice_count_axis` test.** `crates/castore-core/tests/forecasts.rs` lines
  ~25–60 contain a private `mod slice_axis` that _re-implements_ `store.rs`'s
  `pub(crate)
  slice_count_axis` so the five `slice_count_axis_*` tests can call it — they test a
  copy, not the shipping code. Move those five tests into the existing `#[cfg(test)]` module in
  `crates/castore-core/src/store.rs` (precedent: `resolve_windows_tests` there), delete
  `mod slice_axis` and the test-file copies. No public export.
- **0.2 Shared backend harness.** `fn for_each_backend` is duplicated verbatim in
  `tests/indexing.rs:24` and `tests/forecasts.rs:66` (runs a populate/verify pair against both the
  in-memory and NetCDF backends). Extract to `crates/castore-core/tests/common/mod.rs` and include
  with `mod common;` from each integration-test file that uses it. Phase 1 threads more tests
  through it.
- **0.3 Dev-dependencies** (authorized, needed by later phases): add `rusqlite` to `castore-core`
  dev-deps if not already visible to tests (used to corrupt catalogs in 1.7 — precedent:
  `tests/associations.rs` already opens the sqlite file raw to drop tables), and to `castore-cli`
  dev-deps (used in 3.6 to build a corrupt store for the `verify` failing-exit test).

## 3. Phase 1 — core corner cases (`castore-core`)

New integration-test file `tests/edge_values.rs` for 1.1–1.4; other items extend the named files.

- **1.1 Non-finite floats.** Through `for_each_backend` and a persist/reopen cycle: store and read
  back arrays containing NaN, +Inf, −Inf, −0.0 (f64 and f32; static + one Deterministic). Assert
  byte-exact round trip (compare via `to_le_bytes`, not `==`, because NaN). Dedup determinism: two
  arrays identical except different NaN _bit patterns_ must content-address to one stored array
  (`num_distinct_arrays == 1`) — `hash.rs` canonicalizes NaN, this pins it end-to-end. Also pin that
  NetCDF fill values do not collide: a stored value equal to the NetCDF default f64 fill must
  survive reopen.
- **1.2 Empty and minimal arrays.** Pin current behavior (accept-or-error, with a comment) for:
  `SingleTimeSeries` with `length == 0`; single-element series persisted and reopened (memory-only
  today); `Deterministic` with `count == 1` and with `horizon_count == 1`; `Probabilistic` with one
  percentile; `Scenarios` with `scenario_count == 1`. Window-select the count-1 forecast at its only
  window.
- **1.3 Extreme integers.** `i64::MIN`/`MAX`, `u64::MAX`, `i32::MIN` through store add/get and a
  disk round trip (unit tests in `array.rs` cover the type layer; nothing covers the NetCDF trip).
- **1.4 Hostile strings.** Names/owner_type/units/ext containing: non-ASCII (`"负荷_ø"`), spaces and
  quotes, GLOB metacharacters as _literals_ (`"wind[1]"`, `"a*b"`, `"100%_load"`), a 10 kB name, and
  the empty-string name — pin each. Then query back: exact-name filter must match the metacharacter
  names literally; `name_glob` with those characters follows SQLite GLOB semantics — pin what a
  caller must escape. `ext` with invalid-JSON content and a 1 MB payload stored verbatim.
- **1.5 Calendar (`Months`) forecasts.** Nothing today uses `Period::Months` as a forecast
  horizon/interval (only one static-resolution test exists:
  `monthly_calendar_resolution_round_trips_on_disk_and_reader` in `tests/round_trip.rs`). Add, in
  `tests/forecasts.rs`: a `Deterministic` with `resolution = P1M`, `interval = P1M`, `horizon = P3M`
  — round trip, window selection at a calendar boundary, off-grid start rejected;
  `transform_single_time_series` over a monthly `SingleTimeSeries`; a `ForecastReader` sweep on the
  months grid. Also pin zero and negative periods rejected through the store's add path
  (`validate_positive_periods` has never fired in a test).
- **1.6 Forecast constructor validation.** `src/types/time_series.rs` has **no** test module. Add
  one covering: `Deterministic::new` with <2 dims and with shape mismatch; non-divisible
  horizon/resolution (`compute_h`); `Probabilistic::new` with empty, non-increasing, and
  wrong-length percentiles; `Scenarios::new` with scenario_count mismatch; `NonSequentialTimeSeries`
  single-point (length 1) construction.
- **1.7 Failure-side persistence** (extend `tests/netcdf_roundtrip.rs`):
  - `verify_integrity` has never produced a failing report anywhere. Create a store on disk, flip
    one association's `data_hash` via raw `rusqlite`
    (`UPDATE time_series_associations SET
    data_hash = ...`), reopen, assert the report lists the
    error. Same recipe drives the CLI test in 3.6.
  - Torn artifacts: delete the `.sqlite` half and open (pin); zero-byte `.nc` (pin); open a path
    that exists but is a directory.
  - Version: backdate precedent exists (`opening_a_store_from_an_older_format_is_rejected` writes
    attribute `"0.9.0"`); add the **newer**-version case (e.g. `"99.0.0"`) and the missing-attribute
    case.
- **1.8 Backend parity.** These currently run memory-only; thread each through
  `common::for_each_backend`: glob filtering (`tests/filtering.rs`), `rename_time_series`
  (collision + shared-array cases), `remove_by_filter` / `remove_time_series_bulk` rollback,
  discovery (`get_intervals`, `list_names`, `list_owner_types`), `copy_time_series`. Keep series
  tiny (length 3–4) so the disk variants stay fast.
- **1.9 Missing error/API paths** (extend `tests/api_additions.rs`): direct
  `get_time_series(missing_key)` → `NotFound`; wrong-type read (a `KeyIdentity` whose
  `time_series_type` mismatches the stored row); `bulk_read(&[])`, `bulk_read_range(&[])`,
  `remove_time_series_bulk(&[])`; `replace_owner` (zero tests today — move, collision, missing
  owner); `get_array_by_hash` miss; `read_only()` / `get_path()` accessors on all three store
  states.
- **1.10 Association edges** (extend `tests/associations.rs`): supplemental self-pair
  (`component_id == attribute_id`) — pin; `replace_*_component_id` where `old == new` and where the
  old id has no rows; a type-list filter with ~1,200 entries (pins the rendered `IN (…)` list
  against SQLite's bind-variable ceiling); counts/summary cross-checked against brute-force
  enumeration on a fan-in _and_ fan-out graph (one attribute on 50 components, one component with 50
  attributes).
- **1.11 Index plan guards.** The five indexes added to `metadata/schema.rs` (idx_ts_type, idx_name,
  idx_owner_type, idx_category_owner, idx_interval) were verified empirically; a schema change must
  not silently reintroduce full scans. Add a `#[cfg(test)]` module in `src/metadata.rs`: open an
  in-memory rusqlite connection, apply `schema::DDL`, and for each hot shape run
  `EXPLAIN QUERY PLAN`, asserting the expected index name appears: `name = ?` → `idx_name`;
  `name GLOB 'wind_*'` → `idx_name`; `time_series_type = ?` count → `idx_ts_type`;
  `owner_category = ?` distinct-owner → `idx_category_owner`; `DISTINCT interval` → `idx_interval`;
  `owner_type = ?` → `idx_owner_type`; and (regression side) `data_hash = ?` → `idx_hash`.

## 4. Phase 2 — binding parity

- **2.1 Python** (`python/tests/`):
  - Dtype matrix: `bool` and `f32` static round trips are missing from `test_dtype_round_trip` (f32
    exists only inside one forecast test); add both, plus one non-f64 dtype on each forecast type,
    and a multidimensional element shape on a static series (today only `element_shape ==
    []` is
    asserted; Julia tests `(4,2,3)`).
  - NaN/Inf round trip via numpy; forecast persisted-and-reopened (today only statics persist in
    Python tests).
  - Untested `Store` methods, all present in `castore.pyi` — cover each at least once:
    `has_time_series`, `remove_time_series_bulk`, `counts_by_type`, `list_owner_ids`,
    `static_summary`, `forecast_summary`, `check_static_consistency` (including the divergent-grid
    error → this is how to finally _raise_ `IntegrityError`), `resolve_forecast_key`,
    `copy_time_series`, `compact`, `count_array_references`, `num_distinct_arrays`,
    `get_array_by_hash`, `list_array_groups`, `time_series_counts_detailed`.
  - Replace both bare `pytest.raises(Exception)` (off-grid static read; post-close use) with the
    concrete exception types.
  - `IncompatibleFormatError`: build a store, backdate its `data_format_version` NetCDF attribute to
    `"0.9.0"` by reopening the file with raw bytes? Not feasible from Python without a netcdf
    library — instead do it via a tiny Rust-side fixture? **Skip**; pin it in Rust (1.7) and log a
    Finding that Python cannot construct this case natively.
- **2.2 Julia** (`julia/Castore.jl/test/runtests.jl`):
  - Stored round trips for `UInt64` and `Int32` (today only constructor-inference covers Int32); a
    `Float32` forecast.
  - Error paths: operate on a store after `close!` (pin — Python tests this, Julia does not);
    double-`close!`; `open_store` on a nonexistent path (assert the mapped exception type, not just
    "throws").
  - The three forecast `time_range` error cases Python has (misaligned start, end < start, start
    past last window) — mirror them.
  - `replace_owner!` and `clear!` (both exported, both untested; Python covers the equivalents —
    mirror the "both-or-neither" argument validation).
  - NaN round trip; embedded-NUL name: Julia's `Cstring` conversion throws `ArgumentError` before
    reaching the FFI — pin that at the wrapper level.
- **2.3 FFI direct tests** (`crates/castore-ffi`, extend the existing `#[cfg(test)]` module — today
  3 tests touch 21 of 122 exports, and no test asserts an error **code by value**):
  - Store lifecycle **through the ABI**: `castore_store_create` on a temp path → add via
    `castore_store_add_*` → `castore_store_persist`/`flush` → `castore_store_free` → reopen via
    `castore_store_open` read-only. (Existing tests construct `CastoreStoreHandle` directly and
    never call these.)
  - Null-pointer sweep: for a representative export of each family (store op, reader op, key op,
    buffer op) pass a null handle and a null out-param; assert `CASTORE_ERR_NULL_POINTER` (1)
    exactly.
  - Invalid UTF-8 name (e.g. `b"wind\xff\x00"`): assert `CASTORE_ERR_INVALID_UTF8` (2), and that
    `castore_last_error_message` returns a non-empty message (that function has never been called in
    a test).
  - Error codes by value: `CASTORE_ERR_NOT_FOUND`, `CASTORE_ERR_DUPLICATE`, `CASTORE_ERR_READ_ONLY`
    via natural triggers.
  - Buffer-probe edges: `cap` non-zero but smaller than needed; index past
    `num_groups`/`num_entries`.
  - Dtype codes `F32(1)`, `I32(3)`, `U64(4)`, `Bool(5)` asserted through `castore_store_get_single`
    (only 0 and 2 are asserted today).
  - Do **not** test double-free or use-after-free — those are documented UB, not defined behavior to
    pin.

## 5. Phase 3 — proto, server, CLI

- **3.1 Proto conversion unit tests** (`crates/castore-proto/src/convert.rs`): dtype matrix — add
  F32/I32/U64/Bool (only F64/I64 today); `FeatureValue` `Float`/`Bool`/`Str` both directions plus
  the `value == None` → `MissingField` arm (only `Int` is ever exercised); **`Period::Months`
  through the wire types** — `P1M` resolution/horizon/interval in metadata, keys (including the
  empty-string optional-period sentinel), and `GetResp`; `full_key_from_pb` missing
  `initial_timestamp_rfc3339` / missing `length` / unknown enum; a metadata row with
  `time_series_type = DeterministicSingleTimeSeries`.
- **3.2 `ext` over gRPC — decision item.** `time_series_data_to_get_resp` hardcodes
  `ext: String::new()` on all five variants (convert.rs:358–422) while `metadata_to_pb` carries
  `ext` through. Do not change behavior: add a test pinning the blanking, and record it in Findings
  (§9) for the user to rule on. **Ruled on** — see F1; the test is
  `ext_is_always_empty_in_get_resp`.
- **3.3 Server integration** (`crates/castore-server/tests/`): request-validation matrix — missing
  key, `start` without `end`, malformed RFC3339, unparseable ISO period, unknown
  `owner_category`/`time_series_type` enum ints → each asserts `Code::InvalidArgument`; empty-result
  matrix — `ListTimeSeries`/`ListKeys` zero rows, keys for an unknown owner, `HasTimeSeries`
  **false**, `ListOwnerIds` empty, counts on an empty store; BulkRead edges — empty key list, one
  missing key among N, duplicate keys, a `time_range` applied to bulk; one end-to-end **Months**
  series (add `P1M` resolution to the backing store, list + get over the wire, assert the ISO string
  survives).
- **3.4 Server binary** (never executed by any test; bin name `castore-server`, so
  `env!("CARGO_BIN_EXE_castore-server")`): nonexistent store file in `[data].files` → nonzero exit
  with a useful stderr; `auth = "none"` end-to-end (the integration tests attach the interceptor
  manually and never run the binary's dispatch branch); `api_key` through the binary. Bind
  `127.0.0.1:0` if the config accepts port 0 and the binary logs the bound address; otherwise pick a
  high random port with a bounded retry loop.
- **3.5 Client `map_status`.** Unit-test the client's status→error table directly if visible;
  otherwise drive it through the server tests (`NotFound`, `InvalidArgument`, and the documented
  lossy collapse of `Internal`/`Unavailable` into `ConnectionError` — the collapse is intentional;
  pin it, don't "fix" it).
- **3.6 CLI** (`crates/castore-cli/tests/cli_round_trip.rs`; `run_err` should start asserting the
  `Error:` stderr prefix):
  - Bad CSV matrix: ragged row, non-numeric cell in an f64 column, `"abc"` in an i64 column,
    negative into `u64`, value-count/shape mismatch (the likeliest real user error), missing
    timestamp column for `non_sequential`, unsorted and duplicate timestamps, nonexistent CSV path,
    `has_header: true` (never exercised), duplicate add → `DuplicateTimeSeries` message and nonzero
    exit.
  - Descriptor matrix: unknown field (`deny_unknown_fields`), missing per-type required fields (each
    "requires 'X'" message), empty root array, root scalar, malformed JSON, zero `element_shape`
    dimension, non-divisible value count, scenario-count inference failure,
    `DeterministicSingleTimeSeries` in a descriptor → "run `cas transform`" error, a `features` map
    in a descriptor (never populated in any test), `owner_category: supplemental_attribute`.
  - Untested subcommands: `transform` (derive DST from an added STS, then `list` shows the DST tag),
    `copy` (real + `--dry-run`), `persist`, `compact`, `params`, `template`; real `clear` and
    `replace-owner`; single-series `remove --force` (non-`--all` path).
  - `export` → `add` round trip: export a store (csv and json), re-add into a fresh store,
    byte-compare via `get -f csv`. Include one non-f64 dtype and one forecast.
  - `get --time-range` (currently never invoked at all), `--limit`/`--full` truncation message,
    selector zero-match and multi-match messages, glob edges (`?`, `[ab]`, no-match, `*`).
  - `CASTORE_STORE` env fallback (set via `Command::env`), flag-beats-env precedence, missing
    `--store` error.
  - `verify` failing case: corrupt a store's sqlite with the 1.7 recipe (rusqlite dev-dep), assert
    exit code 1 specifically.
- **3.7 Auth breadth** (`tests/auth.rs`): valid key exercised on 3–4 different RPCs (only
  `get_counts` today); non-ASCII header value (the `to_str()` failure arm in `auth.rs:31`).

## 6. Phase 4 — cross-cutting pins

- **4.1 Timestamp precision.** Sub-second resolutions and timestamps through Python (µs `datetime`)
  and Julia (ms `DateTime`): pin whether nanosecond-precision core values truncate silently or error
  at each boundary. Pre-1970 initial timestamps and a century-span `NonSequentialTimeSeries` in
  core.
- **4.2 Concurrency contract pins** (no threads needed; pin the _contract_): a compile-time
  auto-trait assertion for whatever `Store` currently is (`Send`? `Sync`?) so a future change is
  deliberate; two `Store` handles on one path in one process (pin: second open succeeds/blocks
  behind the 5 s busy_timeout/errors); build a `StaticReader`, then mutate the store, then read — if
  the borrow checker forbids it, pin with a `compile_fail` doc-test; if bindings allow it
  (Python/Julia readers are owned objects), pin the observed behavior there and log a Finding if it
  is a torn read.

## 7. Suggested phase → commit mapping

Phase 0 and each of Phases 1–4 is one commit; Phase 3 may split into proto+server / CLI. Run the
full §1 gate before each. Total new-test estimate: ~120–150 test functions.

## 8. Out of scope (do not touch)

- All performance work (`TEST_COVERAGE_ASSESSMENT.md` §3 short-term items and P-list): indexes are
  done; projections, bulk_read batching, hashing, WAL, compact, chunk geometry are separate changes.
- `name_glob` in FFI/Julia (API addition), gRPC surface changes (streaming, message limits,
  `RemoteClient` auth — tracked in `GRPC_BACKLOG.md`), `cas-bench` CI wiring, ASAN/valgrind CI job
  (worthwhile follow-up; needs its own workflow change), the known multi-file `[data].files`
  truncation defect (documented in `GRPC_BACKLOG.md`; do not "fix" it in a test).

## 9. Findings log

Append entries here as `F<n>: <file:line> — <what the test pinned and why it looks wrong>`. Seeded:

- F1: `castore-proto/src/convert.rs:358–422` — `GetResp.ext` is always the empty string while the
  metadata path carries `ext` through. **RESOLVED as documented behavior** (user decision,
  2026-07-24): `GetResp.ext` stays empty and is documented as unused. On investigation this is not a
  value being dropped — three findings changed the picture:
  1. The field is dead in _both_ directions. The server writes `""` on all five variants and nothing
     reads it: `get_resp_to_time_series_data` never touches `resp.ext`, and `client.rs` does not
     mention `ext` at all.
  2. It cannot be populated where it sits. `time_series_data_to_get_resp` takes a `&TimeSeriesData`,
     and the core data variants carry no `ext` — it is a property of the association row. The
     function has nothing to forward.
  3. `ext` is already on the wire: `TimeSeriesMetadata.ext` (field 19, `optional string`) is carried
     by `metadata_to_pb`, so `GetMetadata` and `ListTimeSeries` both return it. The gap was one
     round trip, not a missing capability. Rejected alternatives: populating it at the server costs
     an extra catalog lookup on the hottest read RPC — multiplied across `BulkRead`, which reuses
     `GetResp` items — to serve a value the typed Rust client still could not surface without a core
     API change; and `reserved 10`, though idiomatic for this file (precedent: `reserved 5`), is a
     proto surface change that the plan froze and is better ridden along with other proto work.
     Documented on field 10 in `proto/castore/v1/store.proto` and on `time_series_data_to_get_resp`;
     pinned by `ext_is_always_empty_in_get_resp`.
- F2: Python cannot natively construct an `IncompatibleFormatError` fixture (no NetCDF attribute
  access from the test suite); the case is pinned in Rust only (1.7).
- F3: `castore-core/src/store.rs:1979` — `Store::verify_integrity` delegates straight to the NetCDF
  backend, which walks only its own hash index. A `data_hash` corrupted in the SQLite catalog is
  therefore **not** reported even though every read of that key then fails. Pinned by
  `verify_integrity_does_not_inspect_the_sqlite_catalog` (1.7). **RESOLVED as documented scope**
  (user decision, 2026-07-24): the behavior stays as it is and the check's scope is now stated
  wherever it is surfaced — `Store::verify_integrity` and `IntegrityReport` rustdoc,
  `cas verify --help` (and its success line, now "Array integrity OK"), the PyO3 docstring, the
  Julia docstring, the `castore_store_verify` header comment, and a "What it does not cover" section
  in `docs/src/explanation/content-addressing.md` that the API references link to. Rationale for not
  implementing: `verify_integrity` is public across four bindings plus the CLI and gRPC, so
  tightening it can turn a passing pipeline into a nonzero exit on a store that was working. The
  catalog already has purpose-built checks — `check_static_consistency` and `compact` — and SQLite
  enforces the `NOT NULL`/`CHECK`/unique-index invariants itself. If it is ever revisited, the
  cheapest worthwhile version is a dangling-`data_hash` sweep: `SELECT DISTINCT data_hash`
  cross-checked against `StorageBackend::contains`, which already exists on both backends and is an
  in-memory `HashMap` lookup (`netcdf.rs:1382`). That alone catches a truncated catalog, a corrupted
  one, and a catalog paired with the wrong `.nc`. Note that `PRAGMA integrity_check` would **not**
  catch this finding's case: a flipped `data_hash` is structurally valid SQLite.
- F4: `castore-core/src/store.rs:247` — a torn artifact (NetCDF present, `.sqlite` deleted) opened
  **read-write** silently recreates an empty catalog: the store reports zero time series while the
  arrays are still on disk as unreachable garbage. Read-only opens fail loudly instead. Pinned by
  `opening_a_store_whose_sqlite_half_is_missing_creates_an_empty_catalog` (1.7).
- F5: `castore-core/src/store.rs:248` — `Store::open` opens the SQLite catalog _before_ the NetCDF
  format-version check, so when both halves are wrong the caller sees `Sqlite(CannotOpen)` rather
  than the more informative `IncompatibleFormat`. Pinned by
  `the_catalog_half_is_opened_before_the_format_check` (1.7).
- F7: a closed store reports differently in the two bindings. Python raises
  `TimeSeriesError("store is closed")` from its own guard; Julia nulls the handle so the call
  reaches the ABI's null-handle path and `_check` maps code 1 to `InvalidParameterError`. Both are
  pinned (`test_api_additions.py`, `operations on a closed store raise`), but a caller porting code
  between the bindings cannot catch the same type. Needs a user decision on whether to unify.
- F8: `castore-core/src/metadata.rs` summary rows return `initial_timestamp` as an RFC3339
  **string** in Python (`static_summary`, `forecast_summary`, `check_static_consistency`), while
  `SingleTimeSeries.initial_timestamp` is a `datetime`. Pinned by `test_parity.py`; inconsistent but
  a behavior change to fix.
- F6: `castore-core/src/store.rs:69` (`ListFilter::name_glob`) — there is no escaping API, so a name
  containing `*`, `?`, or `[...]` is not addressable by passing its own text as a `name_glob`
  pattern; `wind[1]` used as a pattern matches nothing. Callers needing literal matching must use
  `ListFilter::name`. Pinned by `name_glob_follows_sqlite_glob_semantics` (1.4). Documentation gap
  rather than a defect, but worth stating in the user-facing docs.
- F9: `castore-server/src/client.rs` — `BulkReadResp` items carry no name, so
  `RemoteClient::bulk_read` fills in the empty string while `get_time_series` (which knows the name
  from the key it was handed) keeps it. Pinned by `a_time_range_applies_to_every_key_in_a_bulk_read`
  (3.3). The caller holds the keys positionally so nothing is lost, but the asymmetry is a trap.
- F10: `cas export` emits a **timestamped** CSV (`timestamp,value`), while a `single` descriptor's
  CSV holds values only — so `export` output is not directly re-addable as the same type, despite
  `export` being documented as the read-direction inverse of `add`. A caller must strip the
  timestamp column or re-add as `non_sequential`. Both routes are pinned by
  `export_then_add_reproduces_the_values` (3.6).
- F11: `castore-cli/src/descriptor.rs:292` — the "element_shape must not contain a zero dimension"
  guard is unreachable, because `per_step` is computed as `product().max(1)`, so a zero dimension
  becomes 1. `element_shape: [0]` instead surfaces later as a value-count mismatch against the
  degenerate shape (`expected 0 values for shape [3, 0]`). Still actionable, but not the intended
  message. Pinned by `a_zero_element_shape_dimension_is_rejected` (3.6).
- F12: `cas verify` writes its failing report to **stdout** and then exits 1, so unlike every other
  failing command it produces no `Error:` stderr diagnostic. A shell caller checking stderr for a
  problem sees nothing. Pinned by `verify_exits_one_on_a_corrupt_store` (3.6).
- F13: **precision mismatch between timestamps and periods.** A timestamp is stored as an RFC3339
  string and keeps nanoseconds; a `Period` is an integer count of milliseconds. The **smallest
  supported period is one millisecond** (`Period::Fixed`) or one month (`Period::Months`), enforced
  in three independent places: `is_positive` tests `num_milliseconds() > 0`, `to_iso8601` emits at
  most three fractional digits, and `from_iso8601` _rejects_ more than three — so a finer period
  cannot be written to or read from disk or the wire. That floor is now stated in the `period.rs`
  module docs. Three consequences, all covered in `castore-core/tests/cross_cutting.rs` (4.1):
  1. **Documented, not fixed.** A sub-millisecond resolution is not rejected at construction —
     `SingleTimeSeries::new` does not validate — and reads back as `PT0S`. Forecast constructors do
     reject it. Same from Python (`test_a_microsecond_resolution_is_silently_truncated_to_zero`) and
     Julia (`a Microsecond resolution is silently flattened to zero`). Callers building a period
     from a sub-millisecond duration must round it themselves.
  2. **Documented, not fixed.** `Period::to_iso8601` drops a sub-millisecond remainder, so `1500us`
     encodes as `PT0.001S`.
  3. **FIXED** (authorized by the user, 2026-07-24). Inside `Period::steps_between` the `Fixed`
     branch tested only `delta_ms % step_ms == 0`, and `delta_ms` truncates, so a forecast
     `time_range` start in the open range `(window boundary, boundary + 1ms)` passed the alignment
     check and was then excluded by `resolve_windows`' exact `>=` filter — silently selecting the
     _next_ window, so a caller asking for hour 1 got hour 2 with no error. The `Months` branch had
     always verified the exact landing via `add_to(start, k) == at`; `Fixed` now does too, making an
     off-grid forecast bound a clean `InvalidParameter` as that function's contract already claimed.
     Documenting the millisecond floor could not close this one: the affected input is a
     _timestamp_, and sub-millisecond timestamps are genuinely supported — an `initial_timestamp`
     keeps nanoseconds, so a grid can be millisecond-spaced while nanosecond-offset in its phase. No
     format change (read-path validation only, so no `DATA_FORMAT_VERSION` bump). The static read
     path is unaffected: it floors/ceils arbitrary bounds by design, via `floor_steps` /
     `ceil_steps`, which stay lenient. Now asserted by
     `a_sub_millisecond_offset_from_a_forecast_window_boundary_is_rejected`,
     `a_forecast_on_a_nanosecond_offset_grid_reads_at_its_own_boundaries`, and the
     `steps_between_rejects_sub_millisecond_offsets` unit test.
- F14 (not a defect, recorded for the record): `Store` is `Send` but **not** `Sync` — `rusqlite`'s
  `Connection` holds `RefCell`s. A caller wanting concurrent readers must wrap it or open one store
  per thread. Pinned by `store_is_send_but_not_sync` (4.2). Readers, keys, and `TypedArray` are both
  `Send` and `Sync`.
