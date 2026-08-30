# Id-first API: state and handoff

Working notes for the in-flight change that makes the catalog association `id` the only way to
address a stored time series. Written 2026-08-30, mid-flight.

**Read this first if you are resuming.** Two branches matter and only one of them builds.

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

## Branch state

| Branch                    | Head      | Builds      | Tests                 |
| ------------------------- | --------- | ----------- | --------------------- |
| `main`                    | `367b596` | yes         | yes                   |
| `dt/id-first-read-remove` | `b632966` | yes         | **all green**         |
| `dt/id-first-step3-wip`   | `4fa41ee` | source only | **no — 201 errors**   |
| `dt/read-by-id-window`    | `02857d5` | yes         | stale, safe to delete |

`dt/read-by-id-window` predates the merge of PR #62. Its extra commit (the `ReadWindow::extent`
overflow fix) is already in `main`, so nothing is stranded there and the branch can go.

`dt/id-first-step3-wip` is stacked on `dt/id-first-read-remove`.

### `dt/id-first-read-remove` — done, verified

Two commits, both green against clippy (0 warnings), 43 Rust test binaries, 107 Julia testsets, 329
pytest, dprint, and cargo-deny.

**`38d948e` — resolve attributes to a catalog row, not a key.** `Store::resolve_forecast_key` was
already the general resolver (its own FFI header said "Despite the name, the underlying
`Store::resolve_forecast_key` is not forecast-specific") and already held the whole
attribute-to-identity rulebook: `Deterministic` matching a stored `DeterministicSingleTimeSeries`,
ambiguity reported with its candidates rather than silently picked. It just handed back a key.

It now returns the catalog row it had already built, as `resolve_metadata`, with `resolve_id` taking
`.id` off it. The row rather than the bare id because the resolution builds one either way: a caller
wanting the id reads one field, and a caller wanting the concrete stored type, the grid, or the
content hash gets them without a second lookup. That is what lets it be the only attribute-addressed
entry point each binding needs — and it is why the two attribute-addressed `get_metadata` overloads
in the Julia binding are _gone_ rather than rewritten. They were this call, spelled twice, one of
them paying a key round-trip first.

The C ABI has one symbol for this (`infrastore_store_resolve_metadata`, probe-then-fetch JSON like
every other metadata getter). Julia and Python each expose `resolve_metadata` + `resolve_id`. The
gRPC handler still returns a key on the wire and builds it from the row, so `ResolveForecastKey` was
unchanged there and still cost one query.

**`b632966` — delete the attribute-addressed reads and removals.** 135 insertions, 909 deletions.

Nothing lost a round trip. `infrastore_store_get_forecast` already resolved-then-read internally —
two catalog queries — so `resolve_id` + `read_by_id` is the same work with the resolution hoisted
where a caller can keep its result. `remove_by_attrs` and `remove_typed` built a `KeyIdentity` in
Rust and called the keyed core method.

Removed: `infrastore_store_get_forecast`, `infrastore_store_remove_by_attrs`,
`infrastore_store_remove_typed` from the C ABI; the `SingleTimeSeries` / `NonSequentialTimeSeries` /
forecast attribute readers and both attribute removals from Julia, along with the 180-line
attribute-addressed forecast ccall wrapper they were the only callers of. Python had none to begin
with.

Julia test call sites moved straight to `resolve_id` + `read_by_id` / `remove_by_ids!` rather than
through the key form, since keys go next. Where a test drove the old reader with a `time_range` it
now asks for a window, and the four cases that turned on a range being _clamped_ assert the window's
checked behaviour instead — including the zero-width range that used to return an empty selection
and is now the caller error it always was.

### `dt/id-first-step3-wip` — incomplete, do not merge

Every Rust **source** crate compiles with `TimeSeriesKey` deleted. Test suites, the Julia binding,
and the docs are not migrated.

What is in it:

- **Core.** All key-addressed reads, removals, metadata reads and key listings gone. The surface is
  now `read_by_id(id, ReadWindow)`, `read_by_ids(ids,
  ReadWindow)`,
  `read_by_ids_range(ids, TimeRange)`, `remove_by_ids`, `remove_by_filter`,
  `rename_time_series(id, name)`, `copy_time_series(id, …)`, `resolve_metadata` / `resolve_id`,
  `list_time_series`, `list_metadata`.
- **`AddedTimeSeries` is gone.** Writes return `i64` / `Vec<i64>`.
- **Four key listings collapsed into one.** `list_keys`, `list_keys_with_hash`, `list_keys_with_id`
  and `list_array_groups` were the same query projected four ways; the row already holds everything
  they projected. They are now `list_metadata(filter)`, which differs from `list_time_series` only
  in not loading an irregular series' timestamp vector.
- **Readers carry ids.** `StaticGroup::ids()`, `ForecastEntry::id()`.
- **`key.rs` went 402 → 36 lines**: only `KeyIdentity`, now internal to the write path and
  documented as _not_ an address.
- **C ABI: 17 entry points deleted**, including the whole key-handle lifecycle
  (`make_key_from_attrs`, `key_free`, `key_eq`, `key_identity_hash`, `key_attributes`,
  `keys_buffer_free`).
- **Python, CLI, bench, proto, gRPC server + client** all migrated to ids.

## Two design decisions made inside the WIP branch — review these

**1. `read_by_ids_range(ids, TimeRange)` was added.** Deleting `bulk_read_range` would have cost the
CLI's `export` a real capability. A window is _checked_: it says "these exact steps". An export
names bounds and does not know how many steps each series has inside them — asking that with a
window is asking it to fail. So both exist, id-addressed: `read_by_ids` takes a `ReadWindow`,
`read_by_ids_range` takes a `TimeRange` and clips. The gRPC `GetTimeSeries` / `BulkRead` and the
CLI's `show` / `plot` / `export` all use the range form.

If you would rather have one method, the alternative is a `ReadWindow` variant meaning "clamp",
which muddies the checked-not-clamped contract that made `ReadWindow` worth having.

**2. `ListFilter.features_exact` was added.** Once `has_time_series(key)` was gone, the `has_*`
probes needed a way to keep matching the _whole_ feature set: `ListFilter` matches features as a
subset, which is right for a listing and wrong for an existence check, where a sibling carrying an
extra feature would answer yes about a series that does not exist. `features_exact` pins the
features hash, which is an equality on an indexed column, so the probe stays on the index. The CLI's
`--replace` path and the FFI's `has_by_attrs` / `has_typed` use it.

## What remains on the WIP branch

### Rust tests — 201 errors

    91  crates/infrastore-core/tests/api_additions.rs
    51  crates/infrastore-ffi/src/lib.rs          (the in-file ABI tests)
    42  crates/infrastore-core/tests/forecasts.rs
    34  crates/infrastore-core/tests/array_sharing.rs
    22  crates/infrastore-core/src/reader.rs      (unit tests)
    15  crates/infrastore-core/tests/standalone_refcount.rs
     4  crates/infrastore-core/tests/sidecar_hdf5_serde.rs
     4  crates/infrastore-core/tests/is_empty.rs

The count only shows crates the check reaches; it grows as each is fixed. Every file still naming a
key:

    core tests:   api_additions, array_sharing, association_ids,
                  bulk_add_in_transaction, cross_cutting, disk_roundtrip,
                  edge_values, forecasts, indexing, is_empty, openapi,
                  round_trip, standalone_refcount, time_reference, transactions
    server tests: grpc_forecast_round_trip, grpc_round_trip,
                  grpc_time_reference, grpc_validation

Mostly mechanical — `let key = store.add(...)` becomes `let id = ...`, and
`store.get_time_series(&key, tr)` becomes `read_by_id(id, window)` or
`read_by_ids_range(&[id], range)`. **But not all of it.** `reader.rs`'s unit tests assert things
like `g0.keys()[0].owner_id()`, which now needs the id resolved to a row; those need judgment, not
substitution.

### Julia binding — not started

Six source files still call deleted FFI symbols: `base.jl`, `batch.jl`, `catalog.jl`,
`forecasts.jl`, `operations.jl`, `store.jl`. The whole Julia key surface goes with them — the
`TimeSeriesKey` handle type, `key_info`, `get_time_series(store, key)`,
`remove_time_series!(store, key)`, `bulk_read`, `rename_time_series!`, `get_time_series_keys`,
`list_keys`, `list_array_groups`, and the reader entry key accessors. Then ~200 call sites in
`julia/InfraStore.jl/test/runtests.jl`.

### Python tests — not started

14 files touch the removed surface: `test_api_additions`, `test_api_round2`, `test_artifact_safety`,
`test_association_ids`, `test_catalog_mode`, `test_element_type_codec`, `test_forecasts`,
`test_hdf5_interop`, `test_is_empty`, `test_openapi`, `test_parity`, `test_round_trip`,
`test_time_reference`, `test_transactions`. The `infrastore.pyi` stub and its drift guard go with
them.

### Docs — not started

19 files: `CLAUDE.md`, `README.md`, and
`docs/src/{explanation/{bindings,
content-addressing,data-model,storage-model}, getting-started/{quick-start-julia,
quick-start-python}, guides/{benchmarks,embedding,julia,python,rust,server},
reference/{c-abi,grpc-api,julia-api,python-api,rust-api}}.md`.

## Suggested order for resuming

Land it as green-at-each-step commits rather than one:

1. **Core + FFI + their tests.** Biggest chunk, self-contained, and it settles the two design
   decisions above before anything else depends on them.
2. **Julia binding + its tests.** Independent of Python.
3. **Python tests + the `.pyi` stub.**
4. **gRPC: proto, server, server tests.** Already migrated in source; only the tests remain.
5. **Docs.**

## A verification trap worth knowing about

While migrating the Julia suite in step two, an automated rewrite produced malformed Julia
(`...Hour(1)]))` — a regex that could not handle parentheses inside keyword arguments. It survived
three consecutive "green" checks because the check grepped for
`MethodError|Test Failed|did not pass`, and a Julia `ParseError` matches none of those: the whole
suite was failing to _load_ and that read as passing. The Julia formatter caught it.

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
/ `read_by_ids` / `get_metadata_by_id` / `list_time_series` in the binding and count). By-name reads
are two: resolve, then the keyed fast path.

**Outstanding:** `[compat] InfraStore = "0.10"` in both `Project.toml` and `test/Project.toml` needs
tightening to whatever release ships `read_by_id`, `remove_by_ids!`, `resolve_metadata` and
`resolve_id`. CI resolves from the registry and 0.10.0 has none of them.

When `TimeSeriesKey` finally goes, IS is the biggest consumer to re-check: it already calls no
attribute- or key-addressed read (its whole read surface is `read_by_id` plus the columnar readers),
but it does use `key_info` on reader entries to recover an owner, and `copy_time_series!`
attribute-addressed.
