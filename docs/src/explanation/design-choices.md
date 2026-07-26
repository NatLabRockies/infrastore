# Design Choices

infrastore is a foundation library. End users rarely call it directly — they reach it through a
parent package such as
[InfrastructureSystems.jl](https://github.com/NREL-Sienna/InfrastructureSystems.jl) (IS.jl) or
[infrasys](https://github.com/natlabrockies/infrasys), which embed infrastore to persist the
time-series data behind their component models. This page records the decisions that shape the API
and the on-disk format, and the reasoning behind them, so that developers of those parent packages
understand what infrastore optimizes for — and, just as importantly, what it deliberately does not.

## Data Orientation: Optimize for Reading Every Component at One Timestamp

**The decision.** In the NetCDF file, `SingleTimeSeries` arrays that share a
`(dtype, element_shape, length, resolution)` are packed as columns of one dataset: **columns are
series, rows are timesteps**, and the HDF5 chunking is `(1, cols, *element_shape)` so that a single
chunk holds one timestamp across every column. We optimize for reading **all components' values at a
given timestamp**, and accept that reading **one component's entire array** is comparatively slow.

**Why.** The workload that matters is simulation. A production-cost or power-flow model steps
through time and, at each step, needs the value of every generator, load, and branch for that one
timestamp — a slice _across_ series, not _down_ one. With this layout that slice is a single chunk
read; the [`ForecastReader` / `StaticReader`](./storage-model.md#the-array-side-netcdf4) columnar
surface is built directly on it. The inverse access — pulling one component's full history — has to
touch every chunk band and is slow by design. That trade is deliberate: the simulation read path is
the hot one, and it is the one parent packages hand to their users.

**What this means for parent-package developers.**

- Lay out bulk writes so that series sharing a shape land in the same dataset — that is what fills
  whole chunks in one pass and keeps the timestamp-slice read fast. See
  [`add_time_series_bulk` and `bulk_add`](./storage-model.md#keeping-the-two-files-consistent).
- Do not build a user-facing feature whose common path is "read this one component's entire array"
  and expect it to be cheap. It works, but it is the slow direction. If a downstream workload
  genuinely needs that orientation, that is a signal to raise with infrastore, not to work around
  with many single-series reads.
- The orientation is a property of the _packed_ NetCDF layout only. `NonSequentialTimeSeries` and
  the dense forecast types are stored as standalone per-array variables and do not participate in
  it.

**Values are immutable.** There is no API — in any binding, by design — to edit a single value,
slice, row, or column of an array already in the store. A stored array is added or deleted as a
whole; changing data means writing a _new_ array (content-addressed, so unchanged neighbors are not
rewritten) and deleting the old one. This falls out of the two priorities above. A chunk holds one
timestamp across many series, so editing one value would force a read-modify-write of a whole chunk
band — the slow direction turned into the write path. And because arrays are content-addressed, an
array's identity _is_ its bytes: mutating it in place would invalidate the hash that every
association row and every dedup reference depends on. Parent packages that expose "update this
value" to their users must implement it as replace-the-array, not edit-in-place.

## Forecast Storage: Chunked So One Window Is Cheap

Dense forecasts (`Deterministic`, `Probabilistic`, `Scenarios`) do **not** use the packed,
cross-component layout above — each forecast is stored as its own standalone array, `[H, count, *E]`
for a `Deterministic`, where `H` is the horizon length and `count` the number of forecast windows.
But reading one window at a time is a first-class access pattern (a simulation stepping the forecast
timeline, one issue time per step), so the array is **chunked in bounded blocks along the `count`
axis** rather than as one whole-array chunk. A window read then decompresses one block instead of
the entire year of windows.

Two consequences a parent-package developer should know:

- **A window sweep is cheap; naive per-window reads are not, unless you go through the reader.**
  Because a chunk is the decompression unit, reading a single window still pulls its whole block.
  The [`ForecastReader`](./storage-model.md) sizes its in-memory cache to that same block width, so
  stepping the window timeline decompresses each block exactly once. Reach for `ForecastReader` (or
  read the whole array once and index it) rather than issuing an independent whole-array read per
  window — the latter re-decompresses overlapping data.
- **Cross-component savings come from dedup, not packing.** Where static series share storage by
  packing many components into one dataset, forecasts share it by
  [content addressing](./content-addressing.md): identical forecast arrays are stored once, and
  `ForecastReader` reads each unique array a single time and fans it out to every component that
  references it.

The block width is a write-time storage choice only — it reads transparently regardless of the width
a store was written with, so it does not change the on-disk format version.

## Split Arrays From Metadata

Numerical arrays live in NetCDF4 and metadata associations live in a companion SQLite catalog,
because the two have opposite size, access, and mutation profiles and each format is strongest at
one of them. The full rationale, the consistency ordering that keeps the two files in step, and the
compaction behavior are covered in the [Storage Model](./storage-model.md).

## Content-Address and Deduplicate Arrays

Arrays are keyed by the SHA-256 of their contents, so two associations with identical data share one
stored array and writes are idempotent on hash. This is what lets many components reference the same
profile without duplicating storage, and it is why deletes are reference-counted. See
[Content Addressing](./content-addressing.md).

## Keep the Multi-Language Surface Consistent

`infrastore-core` is the single source of truth; the Rust, Python (PyO3), Julia (C ABI), CLI, and
gRPC interfaces are thin wrappers over the same `Store`. A capability is not considered done until
it behaves the same across the bindings that support it, and unsupported operations return an
explicit error rather than silently changing semantics. This keeps a parent package free to move
between bindings — for example, Julia via the C ABI and Python via the wheel — without the data
model shifting underneath it. See [Language Bindings](./bindings.md).

## Make Transactions Span Operations, Without Enlisting NetCDF

Every mutating entry point is atomic on its own, and a [bulk add](./storage-model.md) commits a
whole batch in one catalog transaction. Neither helps when several _operations_ have to succeed or
fail together — add a series and remove the one it replaces, or write a batch and derive a forecast
from it. `Store::begin_transaction` opens a unit of work spanning any number of operations, and only
its outermost commit makes anything durable.

The obstacle is that a store is two artifacts and only one of them has transactions. SQLite rolls
back its own statements; NetCDF has nothing to enlist. Rather than trying to give NetCDF a
transaction, the array store is made **append-only for the transaction's duration**, which content
addressing makes cheap:

- **Writes** are recorded as they happen and removed on rollback. An array is recorded only if it
  was _physically written_ — a write of content that already exists is a no-op on hash, so there is
  nothing to undo.
- **Frees are deferred** to the outermost commit. While the transaction is open, an array whose last
  association was removed must keep its bytes, because a rollback restores the rows that point at
  it. At commit the reference count is rechecked against the state the commit is about to make
  permanent, so a hash removed and re-added inside the same transaction is never freed.

That deferral is what makes **removals reversible inside a transaction**, which they are not outside
one. It is the capability a caller cannot build for itself: a client-side undo log can re-insert a
catalog row, but it cannot bring back array bytes the store already reclaimed.

Two consequences fall out of the mechanism rather than being designed in. Reads inside a transaction
see its uncommitted writes, because they go through the same connection — so a binding needs no
staging overlay to give a caller read-your-own-writes. And nesting is free: each level is a SQLite
savepoint, so an inner failure unwinds only its own work and leaves the enclosing transaction
usable.

The costs are real and bound where this is worth using. A transaction holds the SQLite write lock
until it finishes, so a concurrent writer on the same artifact blocks and then fails on its busy
timeout. And a transaction is not a substitute for batching: block-sized NetCDF writes and
feature-set dedup come from `bulk_add`, and a loop of single adds gets neither just because it is
wrapped in a transaction. The two compose — batch each operation, and use a transaction when several
of them must be atomic together.
