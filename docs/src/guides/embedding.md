# Embedding in a Parent Package

This guide is for developers of a package that uses infrastore as the time-series layer behind its
own component model — the shipped examples are [infrasys](https://github.com/NatLabRockies/infrasys)
(Python) and [InfrastructureSystems.jl](https://github.com/NREL-Sienna/InfrastructureSystems.jl)
(Julia). It collects the contracts such a package has to honor and the patterns both consumers
already use, in one place. The per-language guides ([Python](./python.md), [Julia](./julia.md),
[Rust](./rust.md)) show each call; this page says which calls to reach for and why. The reasoning
behind the trade-offs is in [Design Choices](../explanation/design-choices.md).

## What the Store Owns, and What You Do

infrastore is deliberately narrow. It stores arrays, associates each one with an owner, and records
a few relationships between owners. Everything that makes those owners _mean_ something lives in the
parent package.

| Concern                                             | Owner              |
| --------------------------------------------------- | ------------------ |
| Array bytes, dedup, hashing, compression            | infrastore         |
| Which series belongs to which owner, under what key | infrastore         |
| Component ↔ attribute and parent ↔ child edges      | infrastore         |
| The components and attributes themselves            | **parent package** |
| The type hierarchy (abstract types, subtypes)       | **parent package** |
| Mapping your object identities to integer ids       | **parent package** |
| Unit conversion, per-unit bases                     | **parent package** |
| Partial / fuzzy lookups over features               | **parent package** |

The store never parses a component, never walks a type tree, and never rescales a value. Every
filter it offers takes concrete strings and exact scalars; a parent package that exposes anything
richer builds it on top.

## Map Your Model Onto the Catalog

A series is addressed by an **owner** plus a **key**
([Data Model § Keys](../explanation/data-model.md#keys)). Deciding how your objects project onto
those fields is the first integration decision, and the one hardest to change later because it is
written into every user's artifact.

**`owner_id` is a stable `i64`.** The store keys on it and never sees your object identities. If
your components are identified by UUIDs (both shipped consumers are), you allocate an integer per
component, persist that mapping alongside the store, and keep it stable across save/load — the store
cannot reconstruct it. Component ids and supplemental-attribute ids are independent streams, so the
same integer may name one of each; the **`owner_category`** (`Component` / `SupplementalAttribute`)
is part of the owner identity and every owner-scoped call takes it.

**`owner_type` is the concrete type name and nothing more.** It is descriptive (not part of the
uniqueness constraint) and the store has no view of subtyping, so a query for "every `Generator`"
must expand the abstract type into its concrete subtypes yourself and pass each one — this is what
`get_all_subtype_names` does on the InfrastructureSystems.jl side. An empty type list is a
deliberate "none of these" and matches nothing.

**`name` identifies, `component_field` describes.** `name` is part of the key; `component_field`
names the field on the owning component whose time-varying form these values are
(`"max_active_power"`). They often coincide by convention, but a component may carry several series
for one field — an actual and a forecast, several weather years — and only `name` plus `features`
distinguishes them. Set `component_field` whenever your model knows it: it is the one descriptor
that is also a filter, and a parent package that wants "every series that varies `rating`" gets it
for free.

**`features` are typed scalars** (`int` / `float` / `bool` / `str`) and part of the key. A handful
of names are [reserved](../explanation/data-model.md#reserved-feature-names) because consumers
spread feature maps into keyword queries; the store refuses them on write.

**`units`, `quantity_kind`, `unit_system`** describe the values and affect neither identity nor
storage. `unit_system` is a label the store never acts on — `component_base` means "per-unit against
a base the owning component holds in _your_ object graph", and converting back is your job. Unset
means _unspecified_, which is not the same as `natural_units`; do not read one as the other. See
[Optional Descriptors](../explanation/data-model.md#optional-descriptors).

**`application_data` is yours.** It is an opaque payload stored and returned verbatim, so a parent
package can carry whatever it needs per association (a serialized type tag, a provenance record)
without the store knowing. Because the store never validates it, version it yourself if its shape
may change.

## Exact Keys, Subset Filters

The two lookup families match features differently, and a parent package that offers its own key
resolution must know which it is calling:

- A **`TimeSeriesKey`** — whether built from attributes or returned by an add — is matched
  **exactly**: every field including the full feature map. `get_time_series(key)`,
  `has_time_series(key)`, `remove_time_series(key)` and the attribute-based getters behind them find
  one row or none.
- A **list filter** (`list_time_series`, `list_keys`, `has_any_time_series`, `remove_by_filter`,
  `list_names`, …) matches `features` as a **subset**: a series matches when it carries every
  requested pair, whatever else it carries.

InfrastructureSystems.jl resolves user queries by subset, so it cannot delegate that resolution to a
keyed lookup; it lists with the filter and then decides what more than one match means. A parent
package that inherits the same semantics should do the same, and treat ambiguity as its own error to
raise — the store will happily return two rows.

Two more identity facts that surface in parent-package code:

- A `DeterministicSingleTimeSeries` is derived from a stored `SingleTimeSeries` with
  `transform_single_time_series` and **reads back as a `Deterministic`**. The tag stays visible in
  keys, metadata, and counts, and a `Deterministic` filter matches both. Your reads should not
  special-case it; your catalog displays may.
- Descriptors are outside the key, so two adds that differ only in `units` or `application_data` are
  a duplicate. Changing a descriptor means remove and re-add.

## The Store Lifecycle Inside a System Object

Both consumers follow the same shape, and the API was shaped around it. The arrays are never held in
RAM — a system with hundreds of thousands of series does not fit — so the working store is on-disk
from the start, in a scratch directory that lives as long as the system object.

### Build: scratch directory, in-memory catalog

```python
# Python
store = Store.create(scratch / "time_series.h5", catalog="memory")
```

```julia
# Julia
store = Store(; path=joinpath(scratch, "time_series.h5"), catalog=:memory)
```

`catalog="memory"` keeps the SQLite half in RAM and skips the per-commit journaling an attached
catalog pays. That is the right trade for a store beside volatile in-process state: a crash loses
the system under construction regardless, so durability of the scratch catalog buys nothing. Arrays
still stream to the HDF5 file, so memory use does not grow with the data. The scratch directory
holds **no `.sqlite`** until the first save — a half-artifact by design, and one the store refuses
to open as attached later, because arrays without a catalog naming them are not a store. See
[Where the Catalog Lives](../explanation/storage-model.md#where-the-catalog-lives).

Use `in_memory=True` (no file at all) for unit tests and for stores you know are small. It is not a
substitute for the scratch-directory store in a real system.

### Save: one call, atomic pair

```python
store.persist_to(dest)          # Python
```

```julia
persist!(store, dest)           # Julia
```

`persist_to` writes both halves to uniquely named temporaries, fsyncs, and renames them into place,
stamping the pair with a fresh generation so a save interrupted between the two renames is detected
on the next open (`MismatchedArtifact`) rather than read as a valid store. This replaced the
copy-both-files-by-hand dance both consumers once carried, including the close/reopen needed for
HDF5's Windows file lock. Two things to carry into your own save path:

- **A failed save may have destroyed the destination.** The renames replace whatever was there.
  Retry from the still-live store rather than assuming the old artifact survived.
- **Saving an attached store onto its own path is a no-op**, while the in-memory-catalog case is the
  real work — the arrays are already at `path` and the save is what writes the catalog beside them.
  `persist_catalog` does only that half when the arrays are already where they belong, and is a
  checkpoint rather than a mode switch: the catalog stays in RAM afterwards.

### Load for editing: always a copy

```python
store = Store.open_copy(src, scratch / "time_series.h5", catalog="memory")   # Python
```

```julia
store = open_copy(src, joinpath(scratch, "time_series.h5"); catalog=:memory)  # Julia
```

`open` defaults to read-write in every binding, and a read-write open on a user's artifact is the
one way this library will damage a file they care about: HDF5 has no journal and no repair tool, so
an interrupted in-place write is unrecoverable. `open_copy` copies both halves and opens the copy;
the original is only replaced by the final atomic rename of the next `persist_to`. Both consumers
did this by hand before the call existed, and a test in infrasys asserts the loaded directory
differs from the source — keep an assertion like that, because the copy is load-bearing.

For a read-only load (a viewer, a reporting script), `open(path, read_only=True)` is the right call:
nothing is copied and any mutation raises `ReadOnlyStoreError`.

### Create: refuse to clobber

`Store.create` on a path that already holds either half raises `StoreExists`. The failure mode it
prevents is a re-run build script producing an empty array file paired with last week's catalog — a
store that opens cleanly and has nothing behind any row. Pass `overwrite=True` (`overwrite=true` in
Julia) only on a path your package owns and means to discard. See
[Protecting a Saved Artifact](../explanation/storage-model.md#protecting-a-saved-artifact).

### Close, and move the pair together

Close the store explicitly when the system object is done (both bindings offer a context-manager /
do-block form). The `.h5` and `.h5.sqlite` files are one artifact: move, copy, and delete them
together, and never ship one without the other — the paired generation stamp makes a lone half a
`MismatchedArtifact` on open.

### One writer, local disk

A `Store` handle is not thread-safe. The Python class is `unsendable` — touching it from a thread
other than the one that created it raises — and the Julia one is unsynchronized, so concurrent calls
from two tasks are undefined behavior; confine a store (and any reader built from it) to one thread
or task, or guard every call with your own lock. On disk, assume a single writer, and keep a live
store off network filesystems — HDF5's file lock is best-effort and silently absent on Lustre, GPFS,
and NFS, and SQLite's WAL is unsafe there too. Build locally, then copy the finished artifact to
shared storage. See
[One writer, and not on a network filesystem](../explanation/storage-model.md#one-writer-and-not-on-a-network-filesystem).

## Writing: Batch, and Make Multi-Step Changes Atomic

Two mechanisms compose, and neither substitutes for the other:

- **Bulk add** (`add_time_series_bulk` / `AddBatch` + `add_time_series_bulk!`) commits a whole batch
  in one catalog transaction and takes the block-sized HDF5 write path. Series sharing a
  `(dtype, element_shape, length, resolution)` pack into one dataset whose chunks hold one timestamp
  across every column, which is what makes the simulation read below fast — so land same-shaped
  series in the same batch. A loop of single adds is an order of magnitude slower and fills chunks
  one column at a time.
- **Transactions** (`with store.transaction():` / `transaction(store) do … end`) make several
  operations succeed or fail together. Inside one, a removal is reversible; outside one it is not,
  because the array bytes are reclaimed immediately. A transaction holds the SQLite write lock until
  it ends, and does not batch anything by itself.

Values are **immutable**: there is no API to edit a value, slice, or column in place, in any
binding. "Update this series" in a parent package is _add the new array, remove the old one_ —
inside a transaction if the user must never observe the gap. Content addressing makes the add cheap
when the data did not actually change. See
[Design Choices](../explanation/design-choices.md#data-orientation-optimize-for-reading-every-component-at-one-timestamp).

## Reading in a Simulation Loop

The layout is optimized for "every component at one timestamp", and the readers are the API for that
access: build a `StaticReader` or `ForecastReader` once, then step it. A `ForecastReader` reads each
distinct backing array once per step and fans it out to every component referencing it, so a
forecast shared by a hundred components costs one decompression; `entry_slot` lets your own
per-component work dedup the same way. The inverse access — one component's full history — is the
slow direction, and `bulk_read` over many keys is the right call when you need it, not a loop of
`get_time_series`. See the per-language sections:
[Python](./python.md#per-timestamp-reads-simulation-loop),
[Julia](./julia.md#per-timestamp-reads-simulation-loop), [Rust](./rust.md).

## Time

- Every stored instant is a whole number of **milliseconds**; the write path raises
  `InvalidParameter` rather than truncating, because the C ABI exchanges instants as Unix
  milliseconds while Python's `datetime` is microsecond. Quantize `now()` before storing it. Query
  bounds are unconstrained.
- **Python** requires timezone-aware `datetime`s everywhere and returns UTC; a naive value raises.
- **Julia** treats a bare `DateTime` as UTC and returns `DateTime`; with `using TimeZones` a
  `ZonedDateTime` is accepted anywhere a timestamp goes and converted to the instant it names. Reads
  still return `DateTime`, because InfrastructureSystems.jl destructures them.

A parent package that has a notion of local time therefore converts at its own boundary and stores
instants. See [Timestamp precision](../explanation/data-model.md#timestamp-precision).

## Associations Beyond Time Series

The catalog records two relationship tables that have nothing to do with time series:
supplemental-attribute attachments (component ↔ attribute, with counts and grouped summaries) and
parent/child edges (directed component ↔ component). Both hold **only the relationship** — bare ids
and type names — so a parent package keeps the objects in its own graph and uses the store as the
index. `replace_owner` / `reassign` renumbers a component in every catalog at once. Bulk inserts are
all-or-nothing, removals return a count (matching nothing is `0`, not an error), and the `*_types`
filters take concrete type names, exactly like `owner_type` above. See
[Associations Between Entities](../explanation/data-model.md#associations-between-entities).

## Versions and Errors

**Pin a minor range.** The on-disk format is governed by `DATA_FORMAT_VERSION`; opening an artifact
written by an incompatible version raises `IncompatibleFormat` rather than misreading it. The
bindings track the workspace version, so a parent package depends on a compatible range
(`infrastore>=0.8,<0.9` in `pyproject.toml`; a `[compat]` entry in `Project.toml`) and bumps it
deliberately. Do not cite core source line numbers in your own docs — they move.

**Map the error taxonomy, do not flatten it.** Every binding exposes the core's `TimeSeriesError`
variants as distinct types (`NotFound`, `DuplicateTimeSeries`, `DuplicateAssociation`,
`InvalidParameter`, `ReadOnlyStore`, `StoreExists`, `MismatchedArtifact`, `IncompatibleFormat`,
`Integrity`, …). The ones a parent package typically translates into its own exceptions are
`NotFound` and `DuplicateTimeSeries` (user-facing), and `StoreExists` / `MismatchedArtifact` /
`IncompatibleFormat` (artifact-level, usually re-raised with the path). See
[Python exceptions](../reference/python-api.md#exceptions) and
[Julia errors](../reference/julia-api.md#errors).

## Testing an Integration

- Use `in_memory=True` stores in unit tests; they exercise the same core.
- `verify_integrity` re-hashes every array; `is_empty` is the cheap "nothing here" probe.
- The `infrastore` CLI reads any artifact your package writes. `infrastore list` and
  `infrastore info` are the fastest way to see what a consumer actually stored, and
  `infrastore diff --against` compares two artifacts by hash without reading arrays — a usable CI
  gate for "the rewrite produced the same store". See
  [Use the `infrastore` CLI](../how-to/use-cli.md).
- To test unreleased core changes from a consumer checkout, install the binding into the consumer's
  environment (`maturin develop --manifest-path crates/infrastore-py/Cargo.toml` with the consumer's
  venv active; `Pkg.develop(path=...)` plus `INFRASTORE_LIB` for Julia).

## Checklist

- [ ] Integer owner ids are allocated by the package, persisted with the system, and stable across
      save/load.
- [ ] Abstract-type queries expand to concrete `owner_type` names before reaching the store.
- [ ] Feature resolution knows whether it wants exact-key or subset semantics, and ambiguity is the
      package's error.
- [ ] The working store is on disk in a scratch directory with an in-memory catalog; `in_memory`
      stores are for tests.
- [ ] Save goes through `persist_to`; load-for-edit goes through `open_copy`; read-only loads pass
      `read_only`.
- [ ] Multi-series writes use bulk add; multi-step changes that must be atomic use a transaction.
- [ ] Timestamps are instants at millisecond precision, converted at the package boundary.
- [ ] The dependency pins a compatible minor range of infrastore.
