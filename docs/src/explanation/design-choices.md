# Design Choices

infrastore is a foundation library. End users rarely call it directly — they reach it through a
parent package such as
[InfrastructureSystems.jl](https://github.com/Sienna-Platform/InfrastructureSystems.jl) (IS.jl) or
[infrasys](https://github.com/natlabrockies/infrasys), which embed infrastore to persist the
time-series data behind their component models. This page records the decisions that shape the API
and the on-disk format, and the reasoning behind them, so that developers of those parent packages
understand what infrastore optimizes for — and, just as importantly, what it deliberately does not.
The practical counterpart — which calls a parent package should make, and in what order — is
[Embedding in a Parent Package](../guides/embedding.md).

## Data Orientation: Optimize for Reading Every Component at One Timestamp

**The decision.** In the HDF5 file, `SingleTimeSeries` arrays that share a
`(dtype, element_shape, length, resolution)` are packed as columns of one dataset: **columns are
series, rows are timesteps**, and the HDF5 chunking is `(1, cols, *element_shape)` so that a single
chunk holds one timestamp across every column. We optimize for reading **all components' values at a
given timestamp**, and accept that reading **one component's entire array** is comparatively slow.

**Why.** The workload that matters is simulation. A production-cost or power-flow model steps
through time and, at each step, needs the value of every generator, load, and branch for that one
timestamp — a slice _across_ series, not _down_ one. With this layout that slice is a single chunk
read; the [`ForecastReader` / `StaticReader`](./storage-model.md#the-array-side-hdf5) columnar
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
- The orientation is a property of the _packed_ HDF5 layout only. `NonSequentialTimeSeries` and the
  dense forecast types are stored as standalone per-array variables and do not participate in it.

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

Numerical arrays live in HDF5 and metadata associations live in a companion SQLite catalog, because
the two have opposite size, access, and mutation profiles and each format is strongest at one of
them. The full rationale, the consistency ordering that keeps the two files in step, and the
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

## Make Transactions Span Operations, Without Enlisting HDF5

Every mutating entry point is atomic on its own, and a [bulk add](./storage-model.md) commits a
whole batch in one catalog transaction. Neither helps when several _operations_ have to succeed or
fail together — add a series and remove the one it replaces, or write a batch and derive a forecast
from it. `Store::begin_transaction` opens a unit of work spanning any number of operations, and only
its outermost commit makes anything durable.

The obstacle is that a store is two artifacts and only one of them has transactions. SQLite rolls
back its own statements; HDF5 has nothing to enlist. Rather than trying to give HDF5 a transaction,
the array store is made **append-only for the transaction's duration**, which content addressing
makes cheap:

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
timeout. And a transaction is not a substitute for batching: block-sized HDF5 writes and feature-set
dedup come from `bulk_add`, and a loop of single adds gets neither just because it is wrapped in a
transaction. The two compose — batch each operation, and use a transaction when several of them must
be atomic together.

## Upgrade a Store In Place Rather Than Bricking It

`DATA_FORMAT_VERSION` used to be checked by strict equality on open, which made every bump a wall:
"re-create the store." It was bumped six times between 0.12 and 0.19 and each one meant exactly
that. The reason it had to be a wall is that the catalog DDL is `CREATE TABLE IF NOT EXISTS` —
idempotent, so a new _table_ or _index_ lands on an existing store for free, but not
version-agnostic: it will not alter a table that already exists, so a new column or a changed
`CHECK` never reaches an old catalog at all.

This release replaces the wall with a **three-tier compatibility model** plus a migration ladder.

### Two revisions, answering different questions

- **`DATA_FORMAT_VERSION`** describes the **artifact as a whole** and is stamped on the HDF5 root.
  It moves for anything that changes the meaning of bytes already on disk: the array layout, the
  dtype encoding, the timestamp encoding, a hash domain.
- **`CATALOG_SCHEMA_REVISION`** describes the **SQLite catalog** alone and lives in its
  `schema_version` table. **Any catalog change the idempotent DDL cannot make to an existing table —
  a new column, a changed `CHECK`, a rebuilt table, a backfill — needs a `CATALOG_SCHEMA_REVISION`
  bump plus an append-only entry in `MIGRATIONS`.**

`MIN_UPGRADABLE_VERSION` says how far back the ladder reaches. A stamp between it and
`DATA_FORMAT_VERSION` is `Upgradable`; anything older, anything newer, and anything unparseable is
`Incompatible` and is refused exactly as before. When a bump genuinely does strand older stores,
`MIN_UPGRADABLE_VERSION` is raised to match it; when the ladder can absorb it, it is left alone.

The two constants therefore answer opposite questions, and a bump can move only one of them.
`DATA_FORMAT_VERSION` also moves for something no ladder is involved in: a change an **older** build
must not silently accept. Adding the `PersistentTimeSeries` storage code in `0.20.0` is the example
— nothing on disk moved, so the floor stayed at `0.19.0` and existing stores still upgrade, but a
build that has never heard of code 6 has to be turned away at the door rather than opening the store
as current and then reporting the unknown code as catalog corruption.

### A writable open upgrades; a read-only open reports

Opening a store for **writing** runs every migration above the catalog's recorded revision, in
order, each in its own transaction. Opening it **read-only** cannot change anything, so it reports
`CatalogMigrationRequired` — an error that names the remedy (open it once for writing) instead of a
raw `no such column` from inside some later query. This is what a read-only consumer such as the
gRPC server now sees against a store that has not yet been upgraded.

A catalog written by a _newer_ build is `CatalogTooNew` and is refused in both directions. There is
no downgrade path, and this build's DDL and ladder both describe an older shape.

### The ordering that makes it safe

Three steps, and each one is load-bearing:

1. Open the HDF5 half and evaluate its version stamp. This must come first so a store too old to
   migrate reports `IncompatibleFormat` rather than a confusing SQLite error, and so a bad path does
   not leave a freshly created empty `.sqlite` behind. An upgradable stamp is _noted_, not yet
   rewritten.
2. Open the catalog, which is where the ladder runs.
3. Only if (2) succeeded, and only for a writable open, re-stamp the HDF5 half.

The two stamps cannot be written atomically together, so the order decides which half is ahead when
something fails in between. Catalog-first leaves a migrated catalog under an older array-file stamp:
an older build then opens the store and simply never writes a row of a type it does not know, which
is harmless. The reverse would leave a store _claiming_ the new format over an un-migrated catalog —
the exact failure the ladder exists to eliminate.

Migration is **not a save**: the paired generation stamps that pair the two halves are carried
across untouched.

### Append-only, never edited

A landed `MIGRATIONS` entry is frozen. Stores in the wild have already run it, so editing it changes
nothing for them and silently diverges the shape a fresh store gets from the shape an upgraded one
gets. Add a new entry instead. For the same reason each migration carries a **frozen snapshot** of
the table shape it produces rather than deriving it from the live DDL, which will keep moving.

Revision 1 is _defined_ as whatever a pre-ladder build stamped — which is exactly what every
existing store already says, since the `schema_version` table was seeded with a literal `1` and
never read back. That is why the ladder needs no detection heuristic. It is deliberately not a claim
about one particular table shape, though: nothing stamped a revision while the catalog was still
moving, so `1` spans several, and a migration must tolerate any of them. The ladder starts there
rather than trying to resurrect the 0.12–0.16 formats, which changed the meaning of bytes on disk
and are still rejected outright.

### The first rung

Revision 2 widens the `time_series_associations` `time_series_type` `CHECK` from `BETWEEN 0 AND 5`
to `>= 0`. `TimeSeriesType::from_code` is the real gate on that domain and runs on every write and
every read, so the numeric bound bought nothing SQLite had to enforce — while turning the eventual
appending of a seventh type into a table rebuild. Moving the domain onto the enum leaves the `CHECK`
as a non-negativity test, which still refuses a corrupted value.

SQLite has no `ALTER TABLE … DROP CONSTRAINT`, so applying it _is_ the table rebuild, and the
rebuild is what makes the two subtle parts of the ladder concrete: the `time_series_readable` view
has to be dropped first (SQLite refuses to drop a table a view still names), and the `AUTOINCREMENT`
high-water mark has to be carried across by hand, because `DROP TABLE` takes the old table's
`sqlite_sequence` row with it and the copy would restart the counter at `max(id)` — handing back an
id a deleted row already used, which is the one thing `AUTOINCREMENT` is there to prevent.
