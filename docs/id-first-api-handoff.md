# Id-first API: state and handoff

Working notes for the in-flight change that makes the catalog association `id` the only way to
address a stored time series. Written 2026-08-30, revised the same day after a design review.

**Read this first if you are resuming.**

## The philosophy this implements

- Minimize the number of public methods, especially get / remove / rename.
- Prefer by-id methods.
- Keep flexible ways to _identify_ an id.
- Parent applications (InfrastructureSystems.jl, infrasys) wrap the two halves — identify, then act
  — into one step for their users.

One deliberate exception, agreed up front: **`has_*` stays attribute-addressed.** Every form bottoms
out in `MetadataStore::exists`, a covering-index probe that hydrates no row. Routing it through a
resolution would trade an index seek for a row fetch in exactly the hot per-component loops it
exists for. An existence question is an _identify_ operation anyway, which is the half of the split
that keeps its flexibility.

## The public surface, as decided

Four methods cover every identify-or-address question, and they all return the same row type:

| Call                        | Addressed by | Returns                                |
| --------------------------- | ------------ | -------------------------------------- |
| `list_metadata(filter)`     | attributes   | `Vec<TimeSeriesMetadata>` (0..N)       |
| `list_metadata_by_ids(ids)` | ids          | `Vec<TimeSeriesMetadata>`, in id order |
| `get_metadata_by_id(id)`    | one id       | `Option<TimeSeriesMetadata>`           |
| `association_exists(id)`    | one id       | `bool` (index probe, no row)           |

Reads and removals take ids only:

    read_by_id(id, ReadWindow)          read_by_ids(ids, ReadWindow)
    read_by_ids_range(ids, TimeRange)   remove_by_ids(ids)   remove_by_filter(filter)
    rename_time_series(id, name)        copy_time_series(id, …)

`ListFilter` is the single identity vocabulary: `owner_id`, `owner_category`, `owner_type`,
`time_series_type` (interpreted by `TimeSeriesType::accepts`, so `Deterministic` spans a stored
`DeterministicSingleTimeSeries`), `name`, `name_glob`, `component_field`, `zoneless`, `resolution`,
`interval`, `features` (subset) and `exact_features` (whole set).

### Design decisions from the review

1. **`TimeSeriesId(i64)` exists** (`crates/infrastore-core/src/types/id.rs`). The store hands out
   four unrelated integer id streams — this one, `owner_id`, and the two association catalogs' own
   ids — and every read, removal and rename takes one of them, so a bare `i64` made
   `read_by_id(owner_id)` compile. `#[serde(transparent)]`, so SQLite, gRPC and the OpenAPI document
   are unchanged, and every binding still exchanges a plain integer.
2. **Writes return only ids.** `add_time_series` / `add` → `TimeSeriesId`; `add_time_series_bulk`
   and `BulkAdd::commit` → `Vec<TimeSeriesId>`. `AddedTimeSeries` is gone from every language. A
   caller wanting more calls `get_metadata_by_id`.
3. **`resolve_metadata` / `resolve_id` are deleted**, in the core, the C ABI, Python, Julia and the
   gRPC service (the `ResolveMetadata` RPC and its request message are out of the proto). They were
   `list_metadata` with an exactly-one wrapper, and `ListFilter` already carries both rules they
   needed — `accepts` type matching and `exact_features`. What is lost is the ambiguity error naming
   its candidates; a caller now gets the candidate rows themselves and decides.
4. **One listing, not two.** Core's `list_time_series` (which loaded every irregular row's timestamp
   vector) is now the crate-private `list_with_timestamps`; `list_metadata` is the public listing
   and never loads a time axis. This cost nothing in the C ABI, where
   `infrastore_store_list_time_series` and `infrastore_store_list_keys` were producing
   byte-identical JSON — `metadata_to_map` never emitted a `timestamps` field — so the former was a
   strictly slower duplicate and is deleted. Python's `list_time_series` did emit `timestamps`; it
   is now `list_metadata` and that key is always `None`. Read the series for its axis.
5. **`infrastore_store_list_keys` → `infrastore_store_list_metadata`**, matching Julia's `list_keys`
   → `list_metadata`.
6. **The gRPC contract is renamed to match.** Every RPC is now named for the `Store` method it
   exposes, and every request/response is `<Rpc>Req` / `<Rpc>Resp` — including the four that carry
   no field today (previously one shared `EmptyReq`), so a later filter lands as an added field
   rather than a new message and a second RPC. There is no key on the wire any more.

   | was              | now                |
   | ---------------- | ------------------ |
   | `ListTimeSeries` | `ListMetadata`     |
   | `GetTimeSeries`  | `ReadById`         |
   | `BulkRead`       | `ReadByIds`        |
   | `GetMetadata`    | `GetMetadataById`  |
   | `HasTimeSeries`  | `HasAnyTimeSeries` |

   Added: `ListMetadataByIds` and `AssociationExists`, which the core, C ABI, Python and Julia all
   had and the wire did not. Without the latter a remote consumer validating stored references had
   to call `GetMetadataById` and catch `NOT_FOUND`, hydrating a row to answer a yes/no.

   `RemoteClient` follows one for one, and its id-taking methods now accept `TimeSeriesId` rather
   than `i64`. The server config key `max_bulk_read_keys` is `max_read_ids` (`[server]` section,
   `examples/server.toml`) — it bounds how many ids one `ReadByIds` may name, and there are no keys
   to count.

   One thing the rename exposed: `ReadByIdResp` had **no `name` field**, because the client used to
   reconstruct the name from the key a read named. A read names only an id now, so every series read
   over gRPC came back with an empty name where the identical local call returned it. Added as field
   22, populated on all five variants, and `read_resp_to_time_series_data` no longer takes a name
   from its caller. This also closes the old FINDING F9 asymmetry, where the bulk read lost the name
   and the single read did not — both carry it now.

### Method-count ledger

Deleted this round: `resolve_metadata`, `resolve_id`, public `list_time_series` (core, C ABI,
Python, Julia). Added: `list_metadata_by_ids` (core, C ABI, Python, Julia). Net **−3** on the core's
public surface, with the C ABI down one symbol and up two (`list_metadata_by_ids`,
`read_by_ids_range`).

## Branch state

| Branch                    | Head      | Builds                | Tests                 |
| ------------------------- | --------- | --------------------- | --------------------- |
| `main`                    | `367b596` | yes                   | yes                   |
| `dt/id-first-read-remove` | `b632966` | yes                   | all green             |
| `dt/id-first-step3-wip`   | working   | **all source, clean** | **Rust all green**    |
| `dt/read-by-id-window`    | `02857d5` | yes                   | stale, safe to delete |

`dt/read-by-id-window` predates the merge of PR #62. Its extra commit (the `ReadWindow::extent`
overflow fix) is already in `main`, so nothing is stranded there and the branch can go.

### What builds today

Every Rust crate builds and its tests pass; see below. The Julia sources parse (`Meta.parseall`) but
the binding does not load yet.

Verified end to end through the CLI against a real store: `init` → `add` (returns `"id": 1`) →
`list` (row carries `id`) → `get --id 1` → `info --id 1`.

Verified end to end over a live gRPC server too, driving `RemoteClient` against a running
`infrastore-server`: `get_counts`, `list_metadata`, `list_metadata_by_ids`, `association_exists`
(true for a live id, false for a stale one), `get_metadata_by_id` (and `NotFound` for a stale id),
`read_by_id`, `read_by_ids` with a repeated id.

### Rust is done and green

`cargo test --workspace --all-features` — **939 passing, 0 failing**. The full CI gate passes:
`cargo fmt --all --check`, `cargo clippy --workspace --all-features --all-targets -- -D warnings`,
`dprint check`, and `cargo deny check --config deny.toml`. The C header is regenerated and matches.

Migrating the core suite turned up three things the source had wrong, all now fixed:

- **`MetadataStore::rename_by_id` leaked its SQLite error.** A rename onto a name a sibling already
  holds hits `uq_ts_assoc_coalesced`, and it surfaced as a raw
  `Sqlite(SqliteFailure(… UNIQUE constraint failed …))` instead of
  `TimeSeriesError::DuplicateTimeSeries`. The old key-addressed rename went through a path that
  mapped it; the by-id one did not. Mapped now, matching the insert path.
- **`TransformOutcome.written` was still `Vec<i64>`** while everything around it had moved to
  `TimeSeriesId`.
- **`examples/basic.rs` still called `get_time_series(key.identity(), None)`** — examples are not in
  the test census, so it went unnoticed until `--all-targets`.

Three behaviours changed shape rather than breaking, and their tests now assert the new one:

- **A rename preserves the id.** `rename_moves_the_association` asserted "the old key must be gone",
  true when the name was part of the address. The id is the address now and a rename moves the name,
  not the reference — so the test asserts the id still resolves, to the renamed row.
- **A reader's columns resolve live.** `a_reader_survives_a_rename_of_the_series_it_points_at`
  pinned that a reader's cached keys still showed the _old_ name after a rename. A reader carries
  ids, so resolving a column now reports the new one. What still needs pinning — the reader keeps
  working and its column layout is unchanged — is what the test asserts.
- **A listing no longer touches the time axis.**
  `a_missing_time_axis_is_reported_and_refuses_the_read` unlinks a timestamp vector behind the
  catalog's back and used to assert that _listing_ the store failed. `list_metadata` loads no axis,
  so it succeeds; the corruption surfaces on the read, which is where the axis is needed.

### The C ABI's in-file tests

Migrated; all 32 are still there (the count is unchanged against `HEAD` — two were renamed to match
what they now cover). The shape of the change:

- `abi_try_add` returns the id instead of an out-key, so `abi_add_f64` and every caller carry one.
  The old `add_single` call sites passed **both** `out_key` and `out_id`; the signature has only the
  latter now.
- A new `abi_resolve_id(store, owner, name)` is the identify half: it lists for the attributes and
  takes `id` off the row, asserting exactly one match. `abi_get_single` is that plus the new
  `abi_read_by_id`, which reads through `infrastore_store_read_by_id` and decodes with the same
  `infrastore_bulk_result_*` accessors a bulk read uses — a single read comes back in the same
  handle, holding one item.
- `get_single_on_a_missing_key_is_not_found` → `reading_a_stale_id_is_not_found`. An `int64` cannot
  be malformed, so a stale reference is the whole failure surface a read has; the test also pins
  that `infrastore_store_association_exists` answers `false` where the read errors.
- `key_attributes_resolution_is_optional` → `get_metadata_by_id_probes_before_it_fetches`. The
  attributes used to come off a key handle the caller held; a caller holds an id now, and the row it
  names carries every attribute plus descriptors a key never did. The probe-then-fetch contract is
  what that test was really about, and it still holds.
- The null-pointer sweep lost its "null key handle" case — there is nothing to be null — and gained
  the id-taking exports' out-param checks.
- The `name_glob` symbol table dropped from four listings to two: the three key-shaped ones were the
  same query projected differently.

### Tooling worth reusing

Four span-driven scripts did the mechanical ~80% of the core migration and most of the FFI, since
`cargo fix` refuses while a crate's lib tests are broken. Each reads
`cargo check --message-format=json` and edits at the exact reported column, bottom-up, one edit per
line per pass. Run each **per target** (`--test <name>`): `--all-targets` stops at the first failing
target, so the fixer would only ever see one file. The four shapes:

- apply rustc's own `MachineApplicable` suggestions (derefs, `&`-mismatches);
- `expected TimeSeriesId, found &TimeSeriesMetadata` → append `.id.unwrap()`;
- `field, not a method` → drop the call parens (accessor → field, which is most of what changed);
- the same over `cargo clippy` output, for `clone_on_copy` and friends afterwards.

Run each to a fixed point, then fix the residue by hand. The residue is the part that needed
judgment, and it is where all three source bugs above surfaced.

### Julia binding — done and green

`julia --project=julia/InfraStore.jl -e 'using Pkg; Pkg.test()'` — **116 testsets, 0 Fail / 0 Error
/ 0 Broken**, including the `InfraStoreTimeZonesExt` tests that only load through the test target.
Verified the way the trap below demands: `Meta.parseall` over every source and both test files, the
formatter run clean, and `Test Summary` lines counted rather than grepping for failure words.

Deleted outright: the `TimeSeriesKey` handle type and its finalizer, `AddedTimeSeries`,
`TimeSeriesRef`, `key_info`, `KeyInfo` / `KeyRow` / `ArrayGroupRow`, `get_time_series_keys`,
`list_array_groups`, `get_time_series` in all its forms, `bulk_read`, `remove_time_series!`, the
exact-key `has_time_series`, the key-addressed `get_metadata`, and `Base.==` / `hash` / `show` for
keys. The whole keyed forecast-read path went with them — `_get_forecast_raw`,
`_decode_forecast_outputs`, `_forecast_from_raw`, `_forecast_request_matches`,
`_forecast_result_type` — because `read_by_id` already dispatches on the row's stored type.

Reshaped:

- `add_time_series!` and `add_time_series_bulk!` return `Int64` / `Vector{Int64}`.
- `rename_time_series!(store, id, new_name)` returns the same id — a rename moves the name, not the
  reference.
- `copy_time_series!(store, src_id, dst_owner_id, dst_owner_type; new_name)` returns the copy's own
  id.
- `read_by_ids(store, ids; time_range=nothing)` absorbed `bulk_read`'s ranged form, over
  `infrastore_store_read_by_ids_range`. A range _clips_ where `read_by_id`'s window is _checked_.
- `StaticGroup.keys` → `.ids`, `ForecastEntry.key` → `.id`. A column resolves through
  `get_metadata_by_id`, so it reports the row as it is _now_ — a rename between building a reader
  and reading a column is visible, where a key snapshot froze it.
- An id is a plain `Int64`, matching Python's plain `int`. The Rust core's `TimeSeriesId` newtype
  buys type safety a dynamic binding cannot use, and a wrapper would only add unwrapping at every
  call site.

One C ABI gap this turned up: **`infrastore_store_copy_time_series` discarded the id the core
returns.** A caller copying a series had to list for the copy to reference it. It now takes an
`out_id` (nullable to discard), and the header is regenerated.

The test suite gained three local helpers rather than repeating the resolution at ~150 call sites:
`resolve_metadata` / `resolve_id` (list, then assert exactly one — throwing `NotFoundError` for none
and `InvalidParameterError` for several, which is the contract the deleted resolver had) and
`owner_ids` (what `get_time_series_keys` was underneath).

Three testsets changed subject rather than breaking:

- _"a key-addressed forecast read checks the type it was asked for"_ → _"an id read returns the
  concrete stored type, unasked"_. The bug it guarded — asking for a `Deterministic` with a
  `Probabilistic` key silently mis-decoding — is structurally impossible now: a read names an id and
  never a type.
- _"a parameterized request type is rejected"_ moved wholly into the identify half, since a read no
  longer takes a type at all.
- The `Base` interface testset's key `show` became an assertion that an id is a plain value that
  works as a `Dict` key, with the name living on the row it resolves to.

### Python — done and green

`pytest python/tests -q` — **329 passed, 1 skipped.**

`crates/infrastore-py/src/lib.rs`: `list_metadata`, `list_metadata_by_ids`, `read_by_ids_range`
(new), no `resolve_*`, no `PyTimeSeriesKey`, ids in and out as plain `int`. `infrastore.pyi` and its
drift guard track it: `AddedTimeSeries` and `TimeSeriesKey` are gone from the stub, every `add_*`
declares `-> int`, and the three listings collapsed to `list_metadata`.

All 16 test files migrated. `test_parity.py` grew a test-local `_resolve_one` helper — the same
list-then-assert-exactly-one shape the Julia suite adopted — rather than repeating it per call site.
Three tests changed subject: `test_a_filter_resolves_to_one_row_and_its_id`,
`test_the_deterministic_family_filter_finds_a_transformed_dst`, and
`test_one_listing_groups_by_the_underlying_array` (the last because `list_metadata` already carries
`data_hash`, so the separate array-group listing had nothing left to be).

### Docs — done

All 22 files, plus `julia/InfraStore.jl/README.md` and the Rustdoc/docstring prose in the Julia
sources and `crates/infrastore-ffi/src/lib.rs` (whose stale symbol names were leaking into the
generated header).

Two structural changes worth knowing:

- `docs/src/explanation/data-model.md` §Keys became §Identity, and the `#keys` anchor became
  `#identity` repo-wide.
- `docs/src/reference/julia-api.md`'s exported-names list is now generated from the module's own
  `export` block rather than hand-maintained; it had drifted independently of this change (missing
  the whole `TimeReference` family) and carried three duplicate `list_metadata` entries from the
  blanket rename.

`docs/src/reference/c-abi.md` still documented `infrastore_store_list_array_groups`, which no longer
exists — its `data_hash` is on every `list_metadata` row now.

## Verification

The full gate, green as of this commit:

```
cargo fmt --all -- --check                                          clean
cargo clippy --workspace --all-targets --all-features -- -D warnings  clean
cargo test --workspace --all-features                               939 passed, 0 failed
dprint check                                                        clean
cargo deny check --config deny.toml                                 advisories/bans/licenses/sources ok
julia --project=julia/InfraStore.jl -e 'using Pkg; Pkg.test()'      116 testsets, tests passed
pytest python/tests -q                                              329 passed, 1 skipped
```

Also verified end to end outside the suites: the CLI (`init` → `add` → `list` → `get --id 1` →
`info --id 1`), a live gRPC server over the renamed contract, and a live Julia session.

## Two earlier design decisions, both still standing

**1. `read_by_ids_range(ids, TimeRange)` exists beside `read_by_ids(ids, ReadWindow)`.** A window is
_checked_: it says "these exact steps". An export names bounds and does not know how many steps each
series has inside them — asking that with a window is asking it to fail. Both are id-addressed. The
gRPC `GetTimeSeries` / `BulkRead` and the CLI's `show` / `plot` / `export` use the range form, and
the C ABI and Python now expose it too (`infrastore_store_read_by_ids_range`,
`Store.read_by_ids_range`).

There is deliberately **no** `read_by_id_range`. A single-id ranged read is
`read_by_ids_range(&[id], r)` with a `remove(0)`, which is what the CLI and the gRPC service do.

**2. `ListFilter.features_exact` exists**, with an `exact_features(features)` builder. `ListFilter`
matches features as a subset, which is right for a listing and wrong for an existence check, where a
sibling carrying an extra feature would answer yes about a series that does not exist. It pins the
features hash, an equality on an indexed column, so the probe stays on the index. The CLI's
`--replace` path and the FFI's `has_by_attrs` / `has_typed` use it.

## Status

Every step is landed:

1. ~~**Core tests + `reader.rs` unit tests.**~~
2. ~~**FFI in-file tests.**~~
3. ~~**gRPC**~~ — proto, service, client, reference docs, and all five server test files, verified
   against a live server.
4. ~~**Julia binding + its tests.**~~
5. ~~**Python tests + the `.pyi` stub.**~~
6. ~~**Docs.**~~

What remains is downstream, not in this repo — see **Downstream: InfrastructureSystems.jl** below.

## A verification trap worth knowing about

While migrating the Julia suite, an automated rewrite produced malformed Julia (`...Hour(1)]))` — a
regex that could not handle parentheses inside keyword arguments. It survived three consecutive
"green" checks because the check grepped for `MethodError|Test Failed|did not pass`, and a Julia
`ParseError` matches none of those: the whole suite was failing to _load_ and that read as passing.
The Julia formatter caught it.

The same class of thing bit the Rust side once here: a blanket `s.replace("TimeSeriesKey,", "")`
over the test files turned `key: &TimeSeriesKey,` into `key: &,`. Bulk rewrites over Rust need word
boundaries, and the recovery is `git checkout` on the file, not another regex.

When bulk-migrating Julia, verify with `Meta.parseall` or the formatter, and count `Test Summary`
lines rather than grepping for failure words:

```sh
julia -e 'Meta.parseall(read("julia/InfraStore.jl/test/runtests.jl", String))'
julia --project=julia/formatter julia/formatter/format.jl      # reports ParseError
... runtests.jl 2>&1 | grep -c '^Test Summary'                 # expect 107
```

## Environment

Both halves are needed to test a local build; the binding alone still resolves the artifact cdylib,
so a new FFI symbol fails to link without `INFRASTORE_LIB`.

```sh
cargo build -p infrastore-ffi
export INFRASTORE_LIB=$PWD/target/debug/libinfrastore_ffi.dylib
julia --project=julia/InfraStore.jl julia/InfraStore.jl/test/runtests.jl
source .venv/bin/activate && maturin develop --manifest-path crates/infrastore-py/Cargo.toml
python -m pytest python/tests -q
```

## Downstream: InfrastructureSystems.jl

Branch `dt/key-by-association-id` in `~/repos/sienna/InfrastructureSystems.jl`, head `1b93c30f4`,
working tree clean, full suite **9570 passing / 0 failures**. Three commits, all consuming the
_released-plus-`main`_ InfraStore surface:

- `1763b6dc1` — address time series keys by `association_id`
- `05cb2c9dd` — check a key's owner against the key, not the catalog row
- `1b93c30f4` — read a keyed time series in one store call

Measured, not assumed: every keyed read there is now exactly one store call (instrument `read_by_id`
/ `read_by_ids` / `get_metadata_by_id` / `list_metadata` in the binding and count). By-name reads
are two: identify, then the keyed fast path.

**Outstanding:** `[compat] InfraStore = "0.10"` in both `Project.toml` and `test/Project.toml` needs
tightening to whatever release ships `read_by_id`, `remove_by_ids!` and `list_metadata`. CI resolves
from the registry and 0.10.0 has none of them.

Two things to re-check in IS when the Julia binding lands:

- it calls `resolve_metadata` / `resolve_id` nowhere, so decision 3 costs it nothing; its by-name
  path becomes `list_metadata` plus an exactly-one check, which it can own;
- it uses `key_info` on reader entries to recover an owner (now
  `infrastore_forecast_reader_entry_id` plus `get_metadata_by_id`) and `copy_time_series!`
  attribute-addressed (now id-addressed).
