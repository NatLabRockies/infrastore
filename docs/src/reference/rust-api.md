# Rust API

The public surface of `infrastore-core`. Import paths below are relative to the crate root.

```rust
use infrastore_core::{
    create_store, open_store, Store, BulkAdd, TimeSeriesId, KeyIdentity,
    SingleTimeSeries, NonSequentialTimeSeries, PersistentTimeSeries,
    Deterministic, Probabilistic, Scenarios,
    TimeSeriesData, TimeSeriesType, Period,
    TypedArray, Dtype, Compression, OwnerCategory, FeatureValue, Features, TimeSeriesMetadata,
    ListFilter, AddRequest,
    SupplementalAttributeAssociation, SupplementalAttributeFilter, SupplementalAttributeSummaryRow,
    ParentChildAssociation, ParentChildFilter,
    StaticReader, StaticGroup, ForecastReader, ForecastEntry, WindowSlot,
    TimeSeriesCounts, TimeSeriesCountsDetailed, StaticSummaryRow, ForecastSummaryRow,
    ForecastParameters, StaticConsistency, CompactionReport, IntegrityReport,
    TimeSeriesError, Result, DATA_FORMAT_VERSION,
};

// `array_hash` and `hash_hex` are also re-exported at the crate root; `features_hash` is not:
use infrastore_core::hash::{array_hash, features_hash, hash_hex};
use infrastore_core::storage::StorageBackend;
```

All time spans in this API — resolutions, horizons, and intervals — are the crate's
[`Period`](#period), a calendar-aware span. Builders and constructors take `impl Into<Period>`, so
you can pass a fixed `chrono::Duration` (e.g. `Duration::hours(1)`, via `From<Duration>`) or a
calendar span (`Period::months(n)`, for the monthly/annual resolutions a fixed `Duration` cannot
represent). Values read back — struct fields, `get_resolutions`, and the reader accessors — are
always `Period`. Instants (`DateTime<Utc>`) remain chrono types.

## Constructors

```rust
pub fn create_store(path: Option<&Path>, in_memory: bool) -> Result<Store>
pub fn create_store_with_compression(
    path: Option<&Path>,
    in_memory: bool,
    compression: Compression,
) -> Result<Store>
pub fn create_store_replacing(
    path: &Path,
    compression: Compression,
    catalog: CatalogMode,
) -> Result<Store>
pub fn open_store(path: &Path, read_only: bool) -> Result<Store>
pub fn open_store_copy(src: &Path, dest: &Path, catalog: CatalogMode) -> Result<Store>
```

- `create_store(None, true)` — in-memory store, no filesystem I/O.
- `create_store(Some(path), false)` — creates `path` (HDF5) and `path.sqlite` (metadata). **Fails
  with [`StoreExists`](#errors) if either half is already there**; see
  [protecting a saved artifact](../explanation/storage-model.md#protecting-a-saved-artifact) for why
  creating over an existing store is refused rather than allowed to truncate it.
- `create_store_with_compression(...)` — as above but with an explicit HDF5 compression policy.
- `create_store_replacing(...)` — discards any artifact already at `path`, both halves plus the
  catalog's `-wal`/`-shm` sidecars, then creates. Destructive and not atomic: an interrupted call
  can leave neither the old store nor the new one.
- `open_store(path, read_only)` — opens an existing pair. `read_only = true` rejects all writes.
- `open_store_copy(src, dest, catalog)` — copies both halves to `dest` and opens the copy
  read-write, leaving `src` untouched. The safe way to load a store you intend to change: mutating
  an artifact in place is unrecoverable if interrupted, since HDF5 has no journal.

`Store::create` / `Store::create_with_compression` / `Store::create_replacing` / `Store::open` /
`Store::open_copy` are the inherent-method equivalents.

```rust
pub enum Compression {
    None,
    Deflate { level: u8, shuffle: bool }, // level 0–9
}
```

`create_store` uses `Compression::default()` (DEFLATE level 3 + shuffle). The policy is persisted
and restored when the store is reopened for appends, applies only to on-disk stores, and never
changes how data is read back — see the [storage model](../explanation/storage-model.md).

## `Store`

```rust
impl Store {
    pub fn read_only(&self) -> bool;

    // The compression policy applied to writes (restored from the file on open;
    // `Compression::None` for in-memory stores).
    pub fn compression(&self) -> Compression;

    pub fn add_time_series(
        &mut self,
        owner_id: i64,
        owner_type: &str,
        owner_category: OwnerCategory,
        data: TimeSeriesData,
        features: Features,
    ) -> Result<TimeSeriesId>;   // the catalog id its row was filed under
    // The same write from a prebuilt request (sets `application_data`, the unit
    // descriptors, …).
    pub fn add(&mut self, request: AddRequest) -> Result<TimeSeriesId>;

    // A managed batch: packed series are written into batch-sized datasets that
    // fill whole HDF5 chunks (the optimized bulk-write path).
    pub fn add_time_series_bulk(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesId>>;

    // Begin a buffered bulk add. Requests pushed onto the returned guard are
    // accumulated in memory and written together by `BulkAdd::commit` (same
    // block-write path as `add_time_series_bulk`); dropping without committing
    // discards the buffer.
    pub fn bulk_add(&mut self) -> BulkAdd<'_>;

    // Copy one association onto another owner (metadata only; the array is shared).
    pub fn copy_time_series(
        &mut self,
        src: TimeSeriesId,
        dst_owner_id: i64,
        dst_owner_type: &str,
        new_name: Option<&str>,   // None keeps the source name
    ) -> Result<TimeSeriesId>;    // the copy's own id

    // Read many full series at once. Packed `SingleTimeSeries` are read in one
    // decompress-once pass per dataset. Results follow the order the ids are
    // given, repeats included; `NotFound` if any id names no row.
    pub fn read_by_ids(
        &self,
        ids: &[TimeSeriesId],
        window: ReadWindow,
    ) -> Result<Vec<TimeSeriesData>>;

    // The bounds read beside the window read. A window says "these exact steps"
    // and is checked; a range says "whatever falls between these instants" and
    // clips -- which is what an export wants, since it knows the bounds and not
    // the step count.
    pub fn read_by_ids_range(
        &self,
        ids: &[TimeSeriesId],
        time_range: TimeRange,
    ) -> Result<Vec<TimeSeriesData>>;

    // The projection read: a `PersistentTimeSeries` evaluated at each instant in
    // `at`, in the order given. `Instants::zoned` / `Instants::zoneless` name the
    // spelling, which is checked against the series exactly as a `TimeRange`
    // bound is. `InvalidParameter` for any other stored type.
    pub fn read_projected(&self, id: TimeSeriesId, at: Instants<'_>) -> Result<TypedArray>;

    pub fn read_projected_by_ids(
        &self,
        ids: &[TimeSeriesId],
        at: Instants<'_>,
    ) -> Result<Vec<TypedArray>>;

    // One series by id, whole or windowed, in a single call: the id is a
    // primary-key lookup and its row carries the grid the window resolves
    // against. `len` counts timesteps (static types), `count` counts windows
    // (forecasts); supplying the other is `InvalidParameter`, as is a start off
    // the grid or an extent past the end -- a window is checked where a
    // `TimeRange` is clamped. `ReadWindow::full()` reads everything.
    pub fn read_by_id(&self, id: TimeSeriesId, window: ReadWindow) -> Result<TimeSeriesData>;

    pub fn transform_single_time_series(
        &mut self,
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        owner_category: Option<OwnerCategory>,
        resolution: Option<Period>,
    ) -> Result<usize>;

    // One all-or-nothing transaction: `NotFound` if any id names no row, and
    // nothing removed. A repeated id is removed, and counted, once.
    pub fn remove_by_ids(&mut self, ids: &[TimeSeriesId]) -> Result<usize>;
    pub fn clear_time_series(
        &mut self,
        owner: Option<(i64, OwnerCategory)>,
    ) -> Result<usize>;
    pub fn replace_owner(
        &mut self,
        old_owner: i64,
        new_owner: i64,
        owner_category: OwnerCategory,
    ) -> Result<usize>;

    // The identify half: which series exist, what each is, which array each
    // resolves to (`data_hash`), and the `id` to address it by. It replaced five
    // key-shaped listings, each of which was this one query projected
    // differently. Rows carry no time axis -- read the series for that.
    pub fn list_metadata(&self, filter: ListFilter) -> Result<Vec<TimeSeriesMetadata>>;
    // The same listing addressed by id: one catalog query for a whole model's
    // worth of recorded references. `NotFound` if any id names no row.
    pub fn list_metadata_by_ids(
        &self,
        ids: &[TimeSeriesId],
    ) -> Result<Vec<TimeSeriesMetadata>>;
    // Existence over a filter without listing: "does this owner have any time
    // series (of type T)?". Both probes answer from a covering index and are
    // safe for hot loops.
    pub fn has_any_time_series(&self, filter: ListFilter) -> Result<bool>;
    // Whether the store holds no content of any kind — no time series, no
    // associations in any catalog. One short-circuited existence probe per
    // catalog table, so it is O(1) in store size, and it covers tables a
    // client-side conjunction over the count APIs would miss.
    pub fn is_empty(&self) -> Result<bool>;

    // The row filed under `id`, or `None` if the catalog holds no such row --
    // a consumer validating references it persisted earlier is asking whether
    // one still resolves, and a stale reference is an answer.
    pub fn get_metadata_by_id(&self, id: TimeSeriesId) -> Result<Option<TimeSeriesMetadata>>;
    // The same question without fetching the row: a primary-key probe, cheap
    // enough to check every reference in a model on load.
    pub fn association_exists(&self, id: TimeSeriesId) -> Result<bool>;
    pub fn get_array_by_hash(&self, hash: &[u8; 32]) -> Result<TypedArray>;
    // (SingleTimeSeries, DeterministicSingleTimeSeries) associations on one array.
    pub fn count_array_references(&self, data_hash: &[u8; 32]) -> Result<(usize, usize)>;

    pub fn get_resolutions(
        &self,
        time_series_type: Option<TimeSeriesType>,
    ) -> Result<Vec<Period>>;
    pub fn get_time_series_counts(&self) -> Result<TimeSeriesCounts>;
    pub fn get_forecast_parameters(
        &self,
        resolution: Option<Period>,
        interval: Option<Period>,
    ) -> Result<ForecastParameters>;

    // Catalog introspection (each one catalog query; see "Introspection" below).
    pub fn check_static_consistency(
        &self,
        resolution: Option<Period>,
    ) -> Result<Vec<StaticConsistency>>;
    pub fn counts_by_type(&self) -> Result<Vec<(TimeSeriesType, i64)>>;
    pub fn num_distinct_arrays(&self) -> Result<i64>;
    pub fn time_series_counts_detailed(&self) -> Result<TimeSeriesCountsDetailed>;
    pub fn list_owner_ids(
        &self,
        category: OwnerCategory,
        time_series_type: Option<TimeSeriesType>,
        resolution: Option<Period>,
    ) -> Result<Vec<i64>>;
    pub fn static_summary(&self) -> Result<Vec<StaticSummaryRow>>;
    pub fn forecast_summary(&self) -> Result<Vec<ForecastSummaryRow>>;

    // Per-timestamp readers (see "Readers" below).
    pub fn build_static_reader(&self, filter: ListFilter) -> Result<StaticReader>;
    pub fn static_read(&self, reader: &mut StaticReader, at: DateTime<Utc>) -> Result<()>;
    pub fn build_forecast_reader(&self, filter: ListFilter) -> Result<ForecastReader>;
    pub fn forecast_read(&self, reader: &mut ForecastReader, at: DateTime<Utc>) -> Result<()>;

    // The supplemental-attribute catalog (see "Associations" below). Independent
    // of time series: none of these touch, or are touched by, a time-series call.
    pub fn add_supplemental_attribute_association(
        &mut self,
        assoc: SupplementalAttributeAssociation,
    ) -> Result<()>;
    pub fn add_supplemental_attribute_associations(
        &mut self,
        assocs: Vec<SupplementalAttributeAssociation>,
    ) -> Result<usize>;
    pub fn has_supplemental_attribute_association(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<bool>;
    pub fn list_supplemental_attribute_associations(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<Vec<SupplementalAttributeAssociation>>;
    pub fn list_supplemental_attribute_ids(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<Vec<i64>>;
    pub fn list_components_with_attributes(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<Vec<i64>>;
    pub fn remove_supplemental_attribute_associations(
        &mut self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<usize>;
    pub fn replace_supplemental_attribute_component_id(
        &mut self,
        old_id: i64,
        new_id: i64,
    ) -> Result<usize>;
    pub fn count_supplemental_attribute_associations(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<i64>;
    pub fn count_supplemental_attributes(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<i64>;
    pub fn count_components_with_attributes(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<i64>;
    pub fn supplemental_attribute_counts_by_type(&self) -> Result<Vec<(String, i64)>>;
    pub fn supplemental_attribute_summary(
        &self,
    ) -> Result<Vec<SupplementalAttributeSummaryRow>>;

    // The parent/child catalog (see "Associations" below). Same independence
    // from time series.
    pub fn add_parent_child_association(&mut self, assoc: ParentChildAssociation) -> Result<()>;
    pub fn add_parent_child_associations(
        &mut self,
        assocs: Vec<ParentChildAssociation>,
    ) -> Result<usize>;
    pub fn has_parent_child_association(&self, filter: &ParentChildFilter) -> Result<bool>;
    pub fn list_parent_child_associations(
        &self,
        filter: &ParentChildFilter,
    ) -> Result<Vec<ParentChildAssociation>>;
    pub fn list_children(&self, filter: &ParentChildFilter) -> Result<Vec<i64>>;
    pub fn list_parents(&self, filter: &ParentChildFilter) -> Result<Vec<i64>>;
    pub fn remove_parent_child_associations(
        &mut self,
        filter: &ParentChildFilter,
    ) -> Result<usize>;
    pub fn replace_parent_child_component_id(
        &mut self,
        old_id: i64,
        new_id: i64,
    ) -> Result<usize>;
    pub fn count_parent_child_associations(&self, filter: &ParentChildFilter) -> Result<i64>;

    // Reclaims both halves. On an on-disk store this rewrites the .h5 file from
    // the catalog's live set and replaces it, so a delete actually shrinks the
    // store; assumes this process is the file's only user.
    pub fn compact(&mut self) -> Result<CompactionReport>;
    pub fn verify_integrity(&self) -> Result<IntegrityReport>;
    pub fn flush(&mut self) -> Result<()>;

    // Cross-operation transactions: the operations between a begin and its
    // matching commit either all take effect or none do. Removals are reversible
    // only inside one -- outside, a freed array is gone. Calls nest (SQLite
    // savepoints); only the outermost commit is durable. Composes with
    // `bulk_add` rather than replacing it: batch each operation, and use a
    // transaction when several must be atomic together. Holds the SQLite write
    // lock until the outermost commit/rollback.
    pub fn begin_transaction(&mut self) -> Result<()>;
    pub fn commit_transaction(&mut self) -> Result<()>;
    pub fn rollback_transaction(&mut self) -> Result<()>;
    pub fn in_transaction(&self) -> bool;
    // Write the whole store (arrays + catalog) to `path` + `<path>.sqlite`,
    // overwriting them. Works for on-disk *and* in-memory stores.
    pub fn persist_to(&mut self, path: &Path) -> Result<()>;
}
```

`Store` is `Send` but **not** `Sync` (the SQLite catalog holds a `rusqlite::Connection`): a store
can be moved between threads, but sharing one requires external synchronization —
`Arc<Mutex<Store>>`, serializing reads as well as writes, which is what the gRPC server does.

### Method notes

- **`add_time_series`** — Accepts any [`TimeSeriesData`](#timeseriesdata) variant —
  `SingleTimeSeries`, `NonSequentialTimeSeries`, `PersistentTimeSeries`, or a dense forecast
  (`Deterministic`, `Probabilistic`, `Scenarios`). Hashes the array, stores it (deduplicating on the
  hash), inserts a metadata association, and returns its key. Errors with `DuplicateTimeSeries` if
  the key already exists or `ReadOnlyStore` on a read-only store. It is a convenience wrapper over
  `add_time_series_bulk`.
- **`transform_single_time_series`** — Derives a `DeterministicSingleTimeSeries` from every stored
  `SingleTimeSeries`, sharing the underlying array (with `count` derived from the series length),
  and returns the number of series transformed. This is the only way to create a
  `DeterministicSingleTimeSeries`; it is never added directly. The optional `owner_category` and
  `resolution` filters restrict the transform to a single owner category and/or resolution, leaving
  other series untouched.
- **`add_time_series_bulk`** — All-or-nothing: every array put and association insert in the call
  commits together or rolls back together.
- **`read_by_id` / `read_by_ids`** — Reconstruct the stored type as a
  [`TimeSeriesData`](#timeseriesdata) variant (static series and all forecast types). A read names
  only an id, so the row's own `time_series_type` decides what comes back — there is no requested
  type to disagree with it. A `ReadWindow` is _checked_: a start off the series' grid, or an extent
  past its end, is `InvalidParameter` rather than the smaller answer a range would clip to.
- **`read_by_ids_range`** — The bounds read. `start` is inclusive and `end` is exclusive, and it
  _clips_ to what is there — see [Reading a time range](#reading-a-time-range) for what each type
  applies that to. Both bounds must be spelled the way the series are, and a selection spanning both
  coherence groups is refused rather than resolved per series.
- **`read_projected` / `read_projected_by_ids`** — Evaluate a stored step function at instants the
  caller names, returning a `TypedArray` shaped `[at.len(), *E]` rather than a series. Only a
  `PersistentTimeSeries`: every other type would need a _resampling policy_ (interpolate?
  forward-fill?), and choosing one is the application's business, where hold-last needs no choice at
  all. The instants are query bounds like any other and must be spelled the way the series is —
  `Instants::zoned` for a native caller, `Instants::zoneless` for wall clocks — except that an empty
  vector names no bound and so answers with an empty array. The bulk form refuses a selection
  spanning both coherence groups, but each series keeps its **own** breakpoints, so a cohort of
  curves that do not line up is still one call. Deliberately no storage fast path: a persistent
  series is tiny, and the value here is that the semantics live in one place.
- **`clear_time_series`** — `Some((id, category))` removes one owner's series (the owner is the
  `(owner_id, owner_category)` pair); `None` removes all. Returns the count removed. Underlying
  arrays are freed only when their last reference is gone.
- **`replace_owner`** — Reassigns every series owned by `(old_owner_id, owner_category)` to
  `(new_owner_id, owner_category)`, returning the number of associations updated. The category is
  unchanged by the move and scopes which owner's series are reassigned.
- **`copy_time_series`** — Copies one association onto `(dst_owner_id, dst_owner_type)`, keeping the
  source's `owner_category` and every descriptive column — crucially `time_series_type`, so a
  `DeterministicSingleTimeSeries` stays one rather than being materialized into a dense
  `Deterministic` (what a read-then-write copy through the bindings would produce). Only a metadata
  row is written: the array is content-addressed and shared. `new_name = None` keeps the source
  name. Errors with `DuplicateTimeSeries` if the destination identity already exists.
- **`list_metadata`** — The identify half of the whole surface: which series exist, what type and
  grid each is, which array each resolves to (`data_hash`), and the `id` that addresses it. Its
  [`ListFilter`](#listfilter) reads `time_series_type` through
  [`TimeSeriesType::accepts`](#requested-types), so asking for `Deterministic` also selects a stored
  `DeterministicSingleTimeSeries`, and each row still reports the concrete type that matched. A
  caller wanting exactly one row poses the filter and checks that it got one — there is deliberately
  no separate attribute-to-id resolver.
- **`list_metadata_by_ids`** — The same listing addressed by id, for a consumer hydrating a model
  full of recorded references: one catalog query for the whole set rather than one call each.
- **`get_metadata_by_id` / `association_exists`** — One row by id, and the same question without
  fetching it. Both answer `None` / `false` for a stale reference, because a consumer validating
  what it persisted is asking a question; the reads and removals treat the same reference as a
  failure, because they are already committed to acting on it.
- **`get_array_by_hash`** — Read an array directly by content hash, given a `data_hash` off any
  catalog row.
- **`count_array_references`** — `(sts, dst)` association counts referencing one `data_hash`, so a
  caller can tell whether removing a `SingleTimeSeries` would orphan a
  `DeterministicSingleTimeSeries` derived from (and sharing) its array.
- **`verify_integrity`** — Reads back every array and timestamp vector the catalog references,
  recomputes its hash, and reports mismatches and dangling references. It checks the HDF5 half
  against the catalog, never the catalog against itself, so an empty report does not mean the store
  as a whole is sound. See
  [content addressing](../explanation/content-addressing.md#what-it-does-not-cover).
- **`flush`** — Issues `H5Fflush` so the files can be copied for persistence without closing.
- **`persist_to`** — Writes both halves of the artifact to `path` and `<path>.sqlite`, overwriting
  existing targets. An on-disk store is flushed and copied; an in-memory store is materialized
  (every distinct array by hash, plus the whole catalog). Because arrays are content-addressed, this
  reproduces every series — static, forecast, non-sequential — without per-type reconstruction.
- **`persist_catalog`** — Writes an in-memory catalog to this store's own `<path>.sqlite`, stamped
  to match the HDF5 file already beside it. Unlike `persist_to`, copies no arrays: they are already
  in place. A checkpoint, not a mode switch — the catalog stays in RAM afterwards. For a
  `CatalogMode::Attached` store this is `flush`.

### Reading a time range

`read_by_ids_range(ids, TimeRange::new(start, end))` selects on the time axis. The rule is the same
for all seven types — **`start` is inclusive, `end` is exclusive** — but what it is applied _to_
differs, because the types disagree about what a stored value is:

| Type                                                                              | Selected                                                                                         |
| --------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| `NonSequentialTimeSeries`                                                         | every timestamp `t` with `start <= t < end`                                                      |
| `PersistentTimeSeries`                                                            | the breakpoint **in force at** `start`, then every one with `start < b < end`                    |
| `SingleTimeSeries`                                                                | every step whose **covered interval** `[t, t + resolution)` overlaps the range                   |
| `Deterministic` / `Probabilistic` / `Scenarios` / `DeterministicSingleTimeSeries` | every window whose **start** `w` has `start <= w < end`, and `start` must _be_ a window boundary |

The two static rows differ only at the `start` bound, and only when `start` falls strictly inside a
step. An irregular series pairs a value with an _instant_, so a value at `t < start` is outside the
range. A regular series pairs a value with the _step it covers_, so the step containing `start` does
overlap the range and is returned — which means the sliced series' `initial_timestamp` can be
earlier than the `start` that was asked for. A `PersistentTimeSeries` goes one step further for the
same reason: a step function defines a value at `start` itself — the one carried by the breakpoint
in force there — so the slice begins at that breakpoint even though it precedes the window. A
`start` before the very first breakpoint is an `InvalidParameter` error, not a clamp: a step
function is undefined there. The bounds need not be grid-aligned; `start` is floored and `end` is
ceiled onto the grid (calendar-aware for a monthly resolution). The `end` bound behaves identically
under either reading: a step at or after `end` cannot overlap `[start, end)`.

A zero-width range (`end == start`) selects nothing, for every one of the seven types — `[t, t)`
contains no instant, so there is none for a value to be attached to. That is an answer, not a fault.
It holds for `PersistentTimeSeries` too, and takes precedence over the row above: an empty window
has no `start` to be in force at, so an empty window before the first breakpoint is empty rather
than an error.

Forecasts are stricter on purpose. A window is a whole array, not a point, so there is no partial
window to return: an off-grid `start` is rejected with `InvalidParameter` rather than snapped, at
any magnitude — including one finer than a millisecond, which [`Period::steps_between`](#period)
checks the exact landing for. A `start` that is aligned but at or past the last window is rejected
too, rather than returning an empty selection.

`end < start` is `InvalidParameter` for every type. Query bounds themselves are unconstrained: they
may be finer than the millisecond every _stored_ instant is held to (see
[timestamp precision](../explanation/data-model.md#timestamp-precision)).

A [reader](#readers) is exact rather than range-based: `index_at` maps a timestamp to its index and
errors if that instant is not on the timeline — for both the regular and the irregular case. It
never floors, ceils, or clamps.

### Introspection

Grouped catalog queries the bindings use instead of listing every association and aggregating in the
caller. All are read-only and hit SQLite once.

- **`check_static_consistency`** — one [`StaticConsistency`](#report-and-count-types)
  `{ resolution, initial_timestamp, length }` per resolution present (empty `Vec` when there are no
  `SingleTimeSeries`), ordered by resolution; each row is the grid shared by every
  `SingleTimeSeries` at that resolution. Consistency is only required _within_ a resolution — series
  at different resolutions legitimately have different grids — so pass `Some(resolution)` to scope
  the check to one grid. Returns `IntegrityError` when the series at a single resolution disagree.
- **`counts_by_type`** — Association count per [`TimeSeriesType`](#timeseriestype).
- **`num_distinct_arrays`** — Distinct stored content hashes; series sharing an array count once.
- **`time_series_counts_detailed`** — [`TimeSeriesCountsDetailed`](#report-and-count-types):
  distinct owners split by category, and distinct _arrays_ (not associations) split into static vs
  forecast.
- **`list_owner_ids`** — Distinct owner ids in one category that have a time series, optionally
  narrowed by type and/or resolution.
- **`static_summary` / `forecast_summary`** — One [`StaticSummaryRow`](#report-and-count-types) /
  [`ForecastSummaryRow`](#report-and-count-types) per distinct owner/name/shape (or window)
  combination, with the association count. The core groups; the binding formats the table.

### Forecasts

Dense forecasts (`Deterministic`, `Probabilistic`, `Scenarios`) are written through the generic
[`add_time_series`](#store) by wrapping the corresponding object in a
[`TimeSeriesData`](#timeseriesdata) variant. Build the object with its `new` constructor — each
holds a [`TypedArray`](#typedarray-and-dtype) in its native shape, and the constructor validates the
shape against the windowing parameters (`horizon`, `interval`, `count`, and for `Probabilistic` the
`percentiles`):

```rust
use infrastore_core::{Deterministic, TimeSeriesData};

let forecast = Deterministic::new(
    initial_timestamp, resolution, horizon, interval, count, data, name,
)?;
let key = store.add_time_series(
    owner_id, owner_type, OwnerCategory::Component,
    TimeSeriesData::Deterministic(forecast),
    features, units,
)?;
```

Dense forecast arrays (`Deterministic` / `Probabilistic` / `Scenarios`) are stored as standalone
HDF5 variables. A `DeterministicSingleTimeSeries` is **not** added directly: call
`transform_single_time_series(horizon, interval, owner_category, resolution)` to derive one from
every stored `SingleTimeSeries` (it shares the backing column-packed array, derives `count` from the
series length, and dedups against that series).

Conventional array shapes:

| Type                            | `data` shape                                  | extra metadata |
| ------------------------------- | --------------------------------------------- | -------------- |
| `Deterministic`                 | `[H, count, *E]`                              | —              |
| `DeterministicSingleTimeSeries` | the backing `SingleTimeSeries` array (dedups) | —              |
| `Probabilistic`                 | `[percentile_count, H, count, *E]`            | `percentiles`  |
| `Scenarios`                     | `[scenario_count, H, count, *E]`              | —              |

**Reading forecasts:** `read_by_id` reconstructs all forecast types, returning the matching
[`TimeSeriesData`](#timeseriesdata) variant — `Deterministic`, `Probabilistic`, or `Scenarios`. A
`DeterministicSingleTimeSeries` is synthesized into a `Deterministic` by gathering its windows from
the underlying packed array. The low-level pair still works for direct array access: fetch a
[`TimeSeriesMetadata`](#timeseriesmetadata) with `get_metadata_by_id` (it carries `horizon`,
`interval`, `count`, and `percentiles`), then read the array with
`get_array_by_hash(&meta.data_hash)`.

### Readers

`read_by_id` returns a whole series or forecast. To read **many whole series at once** (e.g.
exploration or plotting), `read_by_ids` takes a slice of ids and reads packed `SingleTimeSeries` in
one decompress-once pass per dataset — far cheaper than a `read_by_id` per series under the
timestamp-major chunking, where a single full-series read touches every chunk. Results follow the
order the ids are given, repeats included, and an id naming no row fails the read with `NotFound`
rather than being skipped. For the timestamp-oriented access pattern — _walk the timeline and read
every series' value at each instant_ — build a **reader** instead. A reader is built once over a
[`ListFilter`](#listfilter), pins one resolution, and holds reusable buffers that each read
overwrites in place, so a tight loop allocates nothing. The reader is a passive plan: it does not
borrow the `Store`, so reads go through `Store::static_read` / `Store::forecast_read`, which fill
the buffers; the caller then walks the groups/entries. There are two:
[`StaticReader`](#staticreader-and-staticgroup) for the static types and
[`ForecastReader`](#forecastreader-windowslot-and-forecastentry) for forecasts.

```rust
// Static: value of every SingleTimeSeries at one timestamp, columnar.
let mut reader = store.build_static_reader(ListFilter::new().resolution(res))?;
// `timestamps()` walks the timeline whichever kind it is, so the loop below is
// identical for an irregular reader.
for at in reader.timestamps().collect::<Vec<_>>() {
    store.static_read(&mut reader, at)?;
    for group in reader.groups() {
        let bytes = group.values();        // [num_columns, *element_shape], row-major LE
        // group.keys()[j] identifies column j; group.dtype(), group.element_shape()
    }
}

// Forecast: the window at one timestamp for every matching forecast of one type.
let mut reader = store.build_forecast_reader(
    ListFilter::new().time_series_type(TimeSeriesType::Deterministic).resolution(res),
)?;
for k in 0..reader.count() {
    // Window k: initial + k·interval.
    let at = reader.interval().add_to(reader.initial_timestamp(), k as i64).unwrap();
    store.forecast_read(&mut reader, at)?;
    // `entry_slot` takes the *entry* index, not `entry.slot()` (which indexes `slots()`).
    for (i, entry) in reader.entries().iter().enumerate() {
        let slot = reader.entry_slot(i);
        let bytes = slot.window();         // window of slot.window_shape(), row-major LE
        // entry.id() names the forecast; get_metadata_by_id resolves its owner
    }
}
```

`build_static_reader` covers all three static types, and which one the filter names decides what
must hold. For `SingleTimeSeries` (the default) the filter must pin a resolution and all matched
series must share one grid (`initial_timestamp` + `length`). For `NonSequentialTimeSeries` it must
pin _no_ resolution — an irregular series has none — and all matched series must instead lie on one
timestamp vector, the same cohort that pools their arrays on disk:

```rust
let mut reader = store.build_static_reader(
    ListFilter::new().time_series_type(TimeSeriesType::NonSequentialTimeSeries),
)?;
assert!(reader.resolution().is_none());     // no constant step to report
```

For `PersistentTimeSeries` the filter must likewise pin no resolution, but this is the one case
whose columns need **not** share a timeline: a step function has a value at every instant from its
first breakpoint onward, so each column resolves hold-last on breakpoints of its own. The reader's
timeline is then the sorted **union** of every column's breakpoints — every instant at which some
column changes value — and `index_at` reports a position on that union axis, never a storage row
index. Reading at an instant before some column's first breakpoint is an error naming that column.

```rust
let mut reader = store.build_static_reader(
    ListFilter::new().time_series_type(TimeSeriesType::PersistentTimeSeries),
)?;
// Columns may sit on different breakpoint vectors; the timeline merges them.
for t in reader.timestamps().collect::<Vec<_>>() {
    store.static_read(&mut reader, t)?;
}
```

Uniformity — where it is required — is validated at build, so there is no presence mask in any of
the three cases. `build_forecast_reader` requires a forecast type and a resolution; a
`Deterministic` reader is abstract (also matches `DeterministicSingleTimeSeries`), and all matched
forecasts must share one window timeline (`initial_timestamp` + `interval` + `count`). `static_read`
/ `forecast_read` error (never clamp) if `at` is off the grid/timeline.

**Window-read deduplication.** A `ForecastReader` groups its entries into `WindowSlot`s keyed by
`(array hash, read plan)`: forecasts that reference the same array and slice it the same way —
deduplicated identical data, or several `DeterministicSingleTimeSeries` over one `SingleTimeSeries`
— share one slot. `forecast_read` performs one backend read per **slot**, not per entry, so a
forecast shared by N owners is read once per timestamp (the forecast analog of `StaticReader`
reading a packed column once and gathering it to many columns). `reader.slots()` /
`reader.entry_slot(i)` expose the slots; note that `entry_slot` takes the **entry** index `i` and
returns the slot backing that entry, while `entry.slot()` is that slot's index into `slots()` (equal
for entries that share data).

### Associations

Two catalogs of relationships between entities the store does not otherwise model. They live here so
consumers do not each carry their own SQLite database for them, and they are **wholly independent of
time series**: there are no foreign keys and no cascade (both endpoints live in the caller's object
graph, so a cascade could never fire), so removing a time series never removes an association and
removing an association never removes a time series. A caller that wants both composes the two
calls.

Both families share the same filter conventions: every field of the filter is optional, set fields
are ANDed, and the default filter matches every row — which is what makes a bulk export/import pair
a round trip. The `*_types` fields are lists of **concrete** type names rendered as SQL `IN (…)`;
expanding an abstract type into its subtypes stays with the caller, where the type hierarchy lives,
and an empty list matches nothing. Every `remove_*` returns the number of rows removed, and removing
zero rows is `Ok(0)` rather than an error: the store has no view of whether the caller expected a
hit.

#### Supplemental-attribute associations

Which supplemental attributes are attached to which components. Identity is the
`(component_id, attribute_id)` pair — the type names are denormalized labels carried for filtering
and reporting, not part of identity — so re-attaching the same pair under different type names is
still a duplicate. One attribute may be attached to many components.

- **`add_supplemental_attribute_association`** — Attaches one
  [`SupplementalAttributeAssociation`](#association-types). Errors with `DuplicateAssociation` if
  that component already carries that attribute, whatever type names are supplied.
- **`add_supplemental_attribute_associations`** — All-or-nothing: a duplicate anywhere in the batch
  rolls the whole batch back. Returns the number inserted. It is the import half of the round trip
  whose export is `list_supplemental_attribute_associations` with a default filter.
- **`list_supplemental_attribute_associations` / `has_supplemental_attribute_association` /
  `count_supplemental_attribute_associations`** — The
  [`SupplementalAttributeFilter`](#association-types) predicate over the table. The list returns
  rows in insertion order, so a default-filter export/import pair round-trips.
- **`list_supplemental_attribute_ids` / `list_components_with_attributes`** — Distinct ids on one
  end of the matching rows, ascending: the attributes attached to a component when `component_id` is
  set, and the components carrying an attribute when `attribute_id` is set.
- **`count_supplemental_attributes` / `count_components_with_attributes`** — The same two queries
  counted rather than listed.
- **`remove_supplemental_attribute_associations`** — Removes every matching row and returns the
  count.
- **`replace_supplemental_attribute_component_id`** — Moves every attachment from component `old_id`
  to `new_id`, returning the rows updated. Errors with `DuplicateAssociation` if `new_id` already
  carries one of the attributes being moved.
- **`supplemental_attribute_counts_by_type` / `supplemental_attribute_summary`** — Grouped counts,
  by attribute type or by both type names ([`SupplementalAttributeSummaryRow`](#association-types),
  ordered by attribute type then component type). The core groups; the caller formats.

```rust
use infrastore_core::{SupplementalAttributeAssociation, SupplementalAttributeFilter};

store.add_supplemental_attribute_association(SupplementalAttributeAssociation {
    component_id: 1,
    component_type: "Generator".into(),
    attribute_id: 100,
    attribute_type: "GeographicInfo".into(),
})?;

// The attributes attached to component 1, then the components carrying attribute 100.
let attributes =
    store.list_supplemental_attribute_ids(&SupplementalAttributeFilter::new().component_id(1))?;
let components =
    store.list_components_with_attributes(&SupplementalAttributeFilter::new().attribute_id(100))?;

// Detach them: removing the attachments leaves any time series untouched.
let removed = store.remove_supplemental_attribute_associations(
    &SupplementalAttributeFilter::new().component_id(1),
)?;

// Bulk round trip — the default filter matches every row.
let exported = store.list_supplemental_attribute_associations(&Default::default())?;
target.add_supplemental_attribute_associations(exported)?;
```

#### Parent/child associations

Directed edges between components — a generator (parent) connected to a bus (child), say. Both
endpoints are always components; an attribute cannot appear here. Identity is the **ordered**
`(parent_id, child_id)` pair, so the reversed pair is a different edge. There is no
relationship-kind column, so one ordered pair may be related at most once.

This family is deliberately narrower than the supplemental one: it has no counts-by-type and no
grouped summary, because there is no consumer for them yet. Both are additive if one appears.

- **`add_parent_child_association`** — Records one [`ParentChildAssociation`](#association-types).
  Errors with `DuplicateAssociation` if that ordered pair is already related.
- **`add_parent_child_associations`** — All-or-nothing bulk insert, returning the number inserted;
  the import half of the round trip whose export is `list_parent_child_associations` with a default
  filter.
- **`list_parent_child_associations` / `has_parent_child_association` /
  `count_parent_child_associations`** — The [`ParentChildFilter`](#association-types) predicate over
  the table. The list returns rows in insertion order.
- **`list_children` / `list_parents`** — Distinct ids on one end of the matching edges, ascending:
  the children of a component when `parent_id` is set, and its parents when `child_id` is set.
- **`remove_parent_child_associations`** — Removes every matching edge and returns the count.
- **`replace_parent_child_component_id`** — Rewrites component `old_id` to `new_id` on **both** ends
  of every edge, returning the rows updated. Errors with `DuplicateAssociation` if the rewrite would
  duplicate an edge `new_id` already has.

```rust
use infrastore_core::{ParentChildAssociation, ParentChildFilter};

store.add_parent_child_association(ParentChildAssociation {
    parent_id: 1,
    parent_type: "Generator".into(),
    child_id: 7,
    child_type: "Bus".into(),
})?;

// The reversed pair is a different edge, not a duplicate.
store.add_parent_child_association(ParentChildAssociation {
    parent_id: 7,
    parent_type: "Bus".into(),
    child_id: 1,
    child_type: "Generator".into(),
})?;

let children = store.list_children(&ParentChildFilter::new().parent_id(1))?;   // [7]
let parents = store.list_parents(&ParentChildFilter::new().child_id(7))?;      // [1]

// Bulk round trip — the default filter matches every row.
let exported = store.list_parent_child_associations(&ParentChildFilter::default())?;
target.add_parent_child_associations(exported)?;
```

Neither association catalog is exposed over the [gRPC server](./grpc-api.md) or the
[`infrastore` CLI](./cli.md).

## Types

### `TimeSeriesId` and `KeyIdentity`

`TimeSeriesId` is the catalog id of one association — the only way to address a stored series. It is
a newtype over `i64` rather than a bare integer because the store hands out several unrelated
integer id streams (this one, `owner_id`, and the two association catalogs' own ids), and every
read, removal and rename takes one of them; passing an `owner_id` where a series id belongs is a
type error here rather than a lookup that silently finds the wrong row.

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct TimeSeriesId(pub i64);

impl TimeSeriesId {
    pub const fn get(self) -> i64;   // for a boundary that speaks in scalars
}
```

`#[serde(transparent)]`, so the SQLite catalog, the gRPC wire and the OpenAPI document (which spells
it `association_id`) are unchanged by the wrapper, and every binding exchanges a plain integer.

`KeyIdentity` is the tuple the catalog files a row under, matching its uniqueness constraint. It is
**not an address**: it stays internal to the write path, and nothing takes one. `interval` is part
of the identity (`Some` for every forecast type, `None` for the static types); `resolution` is
`Option` because neither `NonSequentialTimeSeries` nor `PersistentTimeSeries` has one.

```rust
pub struct KeyIdentity {
    pub owner_id: i64,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub resolution: Option<Period>,
    pub interval: Option<Period>,
    pub features: Features,
}
```

A caller that knows a series by its attributes recovers its id from a `list_metadata` row. See
[Data Model](../explanation/data-model.md).

### `SingleTimeSeries`

```rust
pub struct SingleTimeSeries {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Period,
    pub length: usize,
    pub data: TypedArray,
    pub name: String,
    pub element_type: ElementType,   // never optional; see below
    pub units: Option<String>,
    pub quantity_kind: Option<String>,
    pub unit_system: Option<UnitSystem>,
    pub time_reference: Option<TimeReference>,
    pub component_field: Option<String>,
    pub application_data: Option<String>,
}

impl SingleTimeSeries {
    pub fn new(
        initial_timestamp: DateTime<Utc>, resolution: impl Into<Period>, data: TypedArray,
        name: impl Into<String>,
    ) -> Self;
    pub fn with_element_type(self, element_type: ElementType) -> Self;
    pub fn with_units(self, units: impl Into<String>) -> Self;
    pub fn with_quantity_kind(self, quantity_kind: impl Into<String>) -> Self;
    pub fn with_unit_system(self, unit_system: UnitSystem) -> Self;
    pub fn with_time_reference(self, time_reference: TimeReference) -> Self;
    pub fn with_component_field(self, component_field: impl Into<String>) -> Self;
    pub fn with_application_data(self, application_data: impl Into<String>) -> Self;
}
```

`length` is derived from the array's first axis (`data.length()`) by `new`.

The descriptors travel on the series rather than on the write request, so a read returns what a
write declared. `element_type` is **not** an `Option`: `new` resolves it to `Scalar(data.dtype)` —
what an ordinary numeric series is — and `with_element_type` replaces it. There is deliberately no
"undeclared" spelling, because it would be a second way to say `Scalar(dtype)` and a series written
that way would not compare equal to the same series read back. The consequence to know: replacing
`data` on an already-built series without updating `element_type` is a mismatch the store rejects on
write (`InvalidParameter`) rather than silently re-deriving one — build the series again instead.
The other four series types follow the same pattern.

### `TypedArray` and `Dtype`

The storage array type: an element `dtype`, an N-dimensional `shape` `[length, k1, k2, …]` (first
axis time, trailing axes the per-step element shape), and raw row-major, little-endian bytes.

```rust
pub enum Dtype { F64, F32, I64, I32, U64, Bool }   // codes 0..=5; size() = 8/4/8/4/8/1

pub struct TypedArray {
    pub dtype: Dtype,
    pub shape: Vec<usize>,
    pub bytes: Vec<u8>,
}

impl TypedArray {
    pub fn new(dtype: Dtype, shape: Vec<usize>, bytes: Vec<u8>) -> Result<Self, String>; // validates len
    pub fn from_f64(shape: Vec<usize>, values: &[f64]) -> Self;
    pub fn to_f64_vec(&self) -> Result<Vec<f64>, String>;
    pub fn length(&self) -> usize;          // shape[0]
    pub fn element_shape(&self) -> &[usize]; // shape[1..]
}
```

`Dtype::code()` / `Dtype::from_code(i32)` and `Dtype::as_str()` / `Dtype::parse(&str)` convert to
and from the stable integer codes and string names used by the bindings and the on-disk format.

### `Period`

The calendar-aware time span used for every resolution, horizon, and interval. A `Period` is either
a **fixed** span (a `chrono::Duration` — hours, minutes, days, weeks) or a **calendar** span (a
count of months, so `Quarter = 3`, `Year = 12`), letting the store represent monthly/annual grids a
fixed `Duration` cannot.

```rust
pub enum Period {
    Fixed(Duration),   // a fixed chrono::Duration
    Months(i32),       // n calendar months
}

impl Period {
    pub fn fixed(d: Duration) -> Self;     // also: From<Duration> for Period
    pub fn months(n: i32) -> Self;
    pub fn is_irregular(&self) -> bool;    // true for Months
    pub fn is_positive(&self) -> bool;
    pub fn same_kind(&self, other: &Period) -> bool;  // both Fixed, or both Months

    // Grid arithmetic (calendar-aware for `Months`).
    pub fn add_to(&self, dt: DateTime<Utc>, k: i64) -> Option<DateTime<Utc>>;
    // Whole steps from `start` to `at`; errors if `at` is before `start` or off-grid.
    pub fn steps_between(&self, start: DateTime<Utc>, at: DateTime<Utc>) -> Result<usize>;
    // Nearest grid step at or below / at or above `at`; clamps to 0, never errors
    // (used for time-range slicing, where the bounds are arbitrary).
    pub fn floor_steps(&self, start: DateTime<Utc>, at: DateTime<Utc>) -> usize;
    pub fn ceil_steps(&self, start: DateTime<Utc>, at: DateTime<Utc>) -> usize;
    // `other / self` as an exact positive integer (H = horizon / resolution).
    // Mixing a Fixed and a Months period is an error.
    pub fn divide_into(&self, other: &Period) -> Result<usize>;

    // The on-disk / on-the-wire encoding: an ISO-8601 duration ("PT1H", "P1M", "P1Y").
    pub fn to_iso8601(&self) -> String;                 // also the `Display` impl
    pub fn from_iso8601(s: &str) -> Result<Period>;
}
```

`to_iso8601` / `from_iso8601` are the persistence contract: every resolution, horizon, and interval
is stored and transmitted as that string. The encoding is a pure function of the value, so equal
periods always encode identically (which is what the catalog's uniqueness key relies on), and it
round-trips. Calendar units (`Y`, `M` before the `T`) decode to `Months`; fixed units (`W`, `D`, and
`H`/`M`/`S` after the `T`) decode to `Fixed`; a string mixing the two is rejected.

Because `Period: From<Duration>`, anywhere the API takes `impl Into<Period>` you may pass a
`chrono::Duration` directly (e.g. `Duration::hours(1)`); use `Period::months(n)` for calendar spans.
Two periods of different kinds (one `Fixed`, one `Months`) are never equal, even if a particular
month happens to span the same wall-clock time. See the [data model](../explanation/data-model.md)
for how resolution drives the storage grid.

### `NonSequentialTimeSeries`

```rust
pub struct NonSequentialTimeSeries {
    pub timestamps: Vec<DateTime<Utc>>,
    pub length: usize,
    pub data: TypedArray,
    pub name: String,
}

impl NonSequentialTimeSeries {
    pub fn new(
        timestamps: Vec<DateTime<Utc>>, data: TypedArray, name: impl Into<String>,
    ) -> Result<Self, String>;
}
```

`new` validates that timestamps are strictly increasing and match the data length.

### `PersistentTimeSeries`

```rust
pub struct PersistentTimeSeries {
    pub timestamps: Vec<DateTime<Utc>>,   // breakpoints, strictly increasing
    pub length: usize,
    pub data: TypedArray,
    pub name: String,
}

impl PersistentTimeSeries {
    pub fn new(
        timestamps: Vec<DateTime<Utc>>, data: TypedArray, name: impl Into<String>,
    ) -> Result<Self, String>;

    /// The index of the breakpoint in force at `at` — the greatest one `<= at`.
    /// `Err` if `at` precedes the first breakpoint.
    pub fn index_in_force_at(&self, at: DateTime<Utc>) -> Result<usize, String>;

    /// Evaluate the step function at each instant in `at`, in the order given.
    /// `[at.len(), *E]` with the dtype and element shape unchanged.
    pub fn project_onto(&self, at: &[DateTime<Utc>]) -> Result<TypedArray, String>;
}
```

A sparse **step function**: the value at breakpoint `i` is in force until breakpoint `i + 1`, and
past the last one forever; before the first breakpoint it is undefined and asking for it is an
error. `new` validates exactly what `NonSequentialTimeSeries::new` does. `index_in_force_at` is the
single definition of the lookup — nothing else re-derives it. See the
[data model](../explanation/data-model.md#persistenttimeseries) for the full contract and the
contrast with `NonSequentialTimeSeries`.

`project_onto` is that lookup applied `at.len()` times and gathered, and it makes **no policy
choice**: the caller names the instants, and each one resolves by the documented rule or the call
fails. It is a **gather, not a slice** — `at` may be unsorted and may repeat, and the caller's order
is the output order. An empty `at` yields an empty array of the right element shape rather than an
error. A composite `element_type` has its rows copied whole, padding included, so the result decodes
exactly as `data` does. If _any_ instant precedes the first breakpoint the whole call fails, and
every index is resolved before a byte is copied, so no partial answer is produced.

Deciding _which_ instants to ask for, or how to collapse the answer for a downstream solver, stays
with the caller — see [where policy lives](../explanation/data-model.md#persistenttimeseries).

### `Deterministic`

```rust
pub struct Deterministic {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Period,
    pub horizon: Period,
    pub interval: Period,
    pub count: usize,
    pub data: TypedArray,   // shape [H, count, *E]
    pub name: String,
}

impl Deterministic {
    pub fn new(
        initial_timestamp: DateTime<Utc>, resolution: impl Into<Period>,
        horizon: impl Into<Period>, interval: impl Into<Period>, count: usize, data: TypedArray,
        name: impl Into<String>,
    ) -> Result<Self, String>;
}
```

`new` validates `data.shape` against `[H, count, *E]` where `H = horizon / resolution`.

`validate` re-checks those same invariants against the values the struct currently holds, and
returns the same `Err(String)`. Every field is `pub` and the type derives `Deserialize`, so a struct
literal, a field assignment, or `serde_json::from_str` all produce a value that never met `new` —
which is why `add_time_series` calls `validate` on the write path rather than trusting the
constructor. `Probabilistic` and `Scenarios` carry the same method.

### `Probabilistic`

```rust
pub struct Probabilistic {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Period,
    pub horizon: Period,
    pub interval: Period,
    pub count: usize,
    pub percentiles: Vec<f64>,
    pub data: TypedArray,   // shape [num_percentiles, H, count, *E]
    pub name: String,
}

impl Probabilistic {
    pub fn new(
        initial_timestamp: DateTime<Utc>, resolution: impl Into<Period>,
        horizon: impl Into<Period>, interval: impl Into<Period>, count: usize,
        percentiles: Vec<f64>, data: TypedArray, name: impl Into<String>,
    ) -> Result<Self, String>;
}
```

`new` also requires `percentiles` to be non-empty and strictly increasing.

### `Scenarios`

```rust
pub struct Scenarios {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Period,
    pub horizon: Period,
    pub interval: Period,
    pub count: usize,
    pub scenario_count: usize,
    pub data: TypedArray,   // shape [scenario_count, H, count, *E]
    pub name: String,
}

impl Scenarios {
    pub fn new(
        initial_timestamp: DateTime<Utc>, resolution: impl Into<Period>,
        horizon: impl Into<Period>, interval: impl Into<Period>, count: usize,
        scenario_count: usize, data: TypedArray, name: impl Into<String>,
    ) -> Result<Self, String>;
}
```

### `StaticReader` and `StaticGroup`

The columnar static-series reader (see [Readers](#readers)). `Period` is the crate's resolution
type. `values()` is empty until the first `Store::static_read`.

```rust
impl StaticReader {
    pub fn time_series_type(&self) -> TimeSeriesType;  // which static type, hence which timeline
    pub fn initial_timestamp(&self) -> DateTime<Utc>;
    pub fn resolution(&self) -> Option<Period>;        // None for the explicit-axis types
    pub fn length(&self) -> usize;                     // timeline points
    pub fn groups(&self) -> &[StaticGroup];
    pub fn index_at(&self, at: DateTime<Utc>) -> Result<usize>;
    pub fn timestamp_at(&self, index: usize) -> Result<DateTime<Utc>>;
    pub fn timestamps(&self) -> impl Iterator<Item = DateTime<Utc>> + '_;
}

impl StaticGroup {
    pub fn dtype(&self) -> Dtype;
    pub fn element_shape(&self) -> &[usize];  // trailing per-step dims; empty == scalar
    pub fn ids(&self) -> &[TimeSeriesId];     // column j's catalog id
    pub fn num_columns(&self) -> usize;
    pub fn values(&self) -> &[u8];            // [num_columns, *element_shape], row-major LE
}
```

### `ForecastReader`, `WindowSlot`, and `ForecastEntry`

The forecast-window reader (see [Readers](#readers)). Entries are the per-series forecasts; slots
are the deduplicated physical reads. `WindowSlot::window()` is empty until the first
`Store::forecast_read`.

```rust
impl ForecastReader {
    pub fn time_series_type(&self) -> TimeSeriesType;
    pub fn initial_timestamp(&self) -> DateTime<Utc>;
    pub fn resolution(&self) -> Period;
    pub fn interval(&self) -> Period;
    pub fn count(&self) -> usize;             // windows; valid timestamps initial + k·interval
    pub fn entries(&self) -> &[ForecastEntry];
    pub fn slots(&self) -> &[WindowSlot];     // one backend read each per forecast_read
    pub fn entry_slot(&self, i: usize) -> &WindowSlot;  // slot backing entry i
    pub fn window_index(&self, at: DateTime<Utc>) -> Result<usize>;
}

impl ForecastEntry {
    pub fn id(&self) -> TimeSeriesId;
    pub fn slot(&self) -> usize;              // index into slots(); equal for entries sharing data
}

impl WindowSlot {
    pub fn dtype(&self) -> Dtype;
    pub fn window_shape(&self) -> &[usize];   // [H,*E] / [P,H,*E] / [scenarios,H,*E]
    pub fn window(&self) -> &[u8];            // most recent window, row-major LE
}
```

### `TimeSeriesData`

```rust
pub enum TimeSeriesData {
    SingleTimeSeries(SingleTimeSeries),
    NonSequentialTimeSeries(NonSequentialTimeSeries),
    PersistentTimeSeries(PersistentTimeSeries),
    Deterministic(Deterministic),
    Probabilistic(Probabilistic),
    Scenarios(Scenarios),
}

impl TimeSeriesData {
    pub fn time_series_type(&self) -> TimeSeriesType;
    pub fn as_single(&self) -> Option<&SingleTimeSeries>;
    pub fn as_non_sequential(&self) -> Option<&NonSequentialTimeSeries>;
    pub fn as_persistent(&self) -> Option<&PersistentTimeSeries>;
    pub fn as_deterministic(&self) -> Option<&Deterministic>;
    pub fn as_probabilistic(&self) -> Option<&Probabilistic>;
    pub fn as_scenarios(&self) -> Option<&Scenarios>;
}
```

There is no `DeterministicSingleTimeSeries` variant: a stored `DeterministicSingleTimeSeries` is
read back as a `Deterministic` (so `as_deterministic` returns `Some` for it).

### `TimeSeriesType`

```rust
pub enum TimeSeriesType {
    SingleTimeSeries,
    NonSequentialTimeSeries,
    Deterministic,
    DeterministicSingleTimeSeries,
    Probabilistic,
    Scenarios,
    PersistentTimeSeries,
}
```

`as_str()` / `parse(&str)` convert to and from the canonical string names used on disk.
`PersistentTimeSeries` is **appended** rather than inserted: the storage codes are an on-disk
contract, and the `Deterministic`/`DeterministicSingleTimeSeries` adjacency that `code_span` relies
on must not be disturbed. That makes the static group non-contiguous in the code space, which is why
`static_codes()` / `forecast_codes()` return lists rather than ranges.

### `OwnerCategory`

```rust
pub enum OwnerCategory { Component, SupplementalAttribute }
```

### `FeatureValue` and `Features`

```rust
pub enum FeatureValue { Int(i64), Float(f64), Bool(bool), Str(String) }
pub type Features = BTreeMap<String, FeatureValue>;
```

`Features` is sorted by key, which fixes hash order and the uniqueness constraint. `FeatureValue`
canonicalizes `NaN` for hashing and equality.

Feature names that would shadow a time-series or key field are rejected on the write path with
`InvalidParameter` — see
[reserved feature names](../explanation/data-model.md#reserved-feature-names). The list and the
check are public:

```rust
pub const RESERVED_FEATURE_NAMES: &[&str];        // sorted, exact, case-sensitive
pub fn is_reserved_feature_name(name: &str) -> bool;
pub fn validate_features(features: &Features) -> Result<()>;
```

### `TimeSeriesMetadata`

The full record returned by `list_metadata` and `get_metadata_by_id`: owner fields,
`time_series_type`, `name`, `data_hash: [u8; 32]`, the optional temporal fields
(`initial_timestamp`, `resolution`, `length`, `horizon`, `interval`, `count`, `timestamps`),
`features`, the descriptors (`units`, `quantity_kind: Option<String>`,
`unit_system: Option<UnitSystem>`, `time_reference: Option<TimeReference>`,
`component_field: Option<String>`, `application_data: Option<String>`),
`percentiles: Option<Vec<f64>>` (set for `Probabilistic`), and the array typing: `dtype: Dtype`,
`element_shape: Vec<usize>`. The span fields (`resolution`, `horizon`, `interval`) are
`Option<Period>`.

### `UnitSystem`

```rust
pub enum UnitSystem { NaturalUnits, ComponentBase }

impl UnitSystem {
    pub fn as_str(&self) -> &'static str;      // "natural_units" / "component_base"
    pub fn parse(s: &str) -> Option<Self>;
}
```

`None` on a metadata row means _unspecified_, not `NaturalUnits`. See
[Optional descriptors](../explanation/data-model.md#optional-descriptors).

### `TimeReference` and `TimeRange`

```rust
pub enum TimeReference {
    Utc,                  // an instant, written as UTC
    FixedOffset(i32),     // an instant, written at a fixed offset — minutes east
    Zone(String),         // an instant, written in a named IANA zone; held opaquely
    Zoneless,             // a wall clock; names no instant
}

impl TimeReference {
    pub fn is_zoned(&self) -> bool;
    pub fn is_zoneless(&self) -> bool;
    pub fn accepts_zoned_bound(reference: Option<&TimeReference>) -> bool;
    pub fn as_storage_string(&self) -> String;   // "utc" / "-07:00" / "America/Denver" / "zoneless"
    pub fn parse(s: &str) -> Result<Self>;
    pub fn validate(&self) -> Result<()>;        // shape only; no tz database
}
```

How a series' timestamps were **spelled**. `None` on a metadata row means _unspecified_, and groups
with the three zoned variants for query bounds — it is not a claim the timestamps were written as
UTC. Rust has no naive datetime type, so a native caller **declares** the spelling; the bindings
infer it from theirs.

`validate` checks shape only: a zone name must be non-empty, bounded, IANA-shaped, and unreadable as
an offset or as either literal — which is what lets one catalog column hold all four spellings.
Existence is deliberately not checked; see
[Time references](../explanation/data-model.md#time-references).

```rust
pub struct TimeRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub zoneless: bool,
}

impl TimeRange {
    pub fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Self;        // zoned
    pub fn zoneless(start: DateTime<Utc>, end: DateTime<Utc>) -> Self;
    pub fn spelled(start: DateTime<Utc>, end: DateTime<Utc>, zoneless: bool) -> Self;
    pub fn bounds(&self) -> (DateTime<Utc>, DateTime<Utc>);
}
impl From<(DateTime<Utc>, DateTime<Utc>)> for TimeRange;   // zoned
```

The `time_range` argument of `read_by_ids_range`. The `zoneless` flag is what lets the core refuse a
bound whose spelling the series cannot answer rather than coercing it; a `DateTime<Utc>` is zoned by
construction, so `(start, end).into()` is the native spelling.

### `Descriptors`

The descriptive attributes a series carries alongside its array, applied to a reconstructed series
by `TimeSeriesData::set_descriptors`:

```rust
pub struct Descriptors {
    pub element_type: ElementType,
    pub units: Option<String>,
    pub quantity_kind: Option<String>,
    pub unit_system: Option<UnitSystem>,
    pub time_reference: Option<TimeReference>,
    pub component_field: Option<String>,
    pub application_data: Option<String>,
}

impl Descriptors {
    pub fn new(element_type: ElementType) -> Self;   // everything else unset
}
```

It is a struct rather than a positional argument list because four of the seven fields are
`Option<String>`: as bare parameters, `units`, `quantity_kind`, `component_field`, and
`application_data` would be silently interchangeable at every call site.

### `ListFilter`

A builder; every field is an optional filter, combined with AND. `ListFilter::new()` and
`ListFilter::default()` are the same empty filter (matches everything).

```rust
ListFilter::new()
    .owner_id(42)
    .owner_type("Generator")
    .owner_category(OwnerCategory::Component)
    .time_series_type(TimeSeriesType::SingleTimeSeries)
    .name("load")
    .name_glob("load_*")  // SQLite GLOB (case-sensitive, `*`/`?`); ANDed with .name
    .component_field("max_active_power")  // exact, case-sensitive; see below
    .zoneless(false)      // coherence predicate on the timestamp spelling; see below
    .resolution(Duration::hours(1))   // impl Into<Period>
    .interval(Duration::hours(24))    // impl Into<Period>; forecasts only
    .features(features)   // subset match: rows must contain at least these pairs
```

`component_field` answers "every series that varies this field", alone or scoped to one owner. It is
a descriptor, not part of a series' identity, so it narrows a listing but never addresses a single
row on its own — one component may carry several series for one field, distinguished by name or
features. A row that declares no `component_field` matches no value (SQL equality is never true
against NULL), so the filter cannot select the rows that left it unset. It is served by the partial
index `idx_component_field`, which costs a store that never sets the field nothing.

`zoneless` is a **binary predicate**, not a match on a specific `TimeReference`: `Some(true)` keeps
the wall-clock series, `Some(false)` keeps everything that accepts an instant bound — the three
zoned spellings _and_ the rows that left the reference unset. An exact match could not name that
second group at all (the trap `component_field` documents), and here those rows are a coherence
group rather than an oversight. It is the constructive half of the rules that make
`read_by_ids_range` and `build_static_reader` refuse a selection spanning both groups; see
[Time references](../explanation/data-model.md#time-references).

### `AddRequest`

The element type of `add_time_series_bulk` (and of `BulkAdd::push`), mirroring the `add_time_series`
arguments plus an optional `application_data` — an opaque, package-owned payload (typically JSON)
stored verbatim. The series name lives on the `TimeSeriesData` object, not here.

```rust
pub struct AddRequest {
    pub owner_id: i64,
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub data: TimeSeriesData,
    pub features: Features,
    pub units: Option<String>,
    pub application_data: Option<String>,
    // …plus the other descriptors (`quantity_kind`, `unit_system`,
    // `time_reference`, `component_field`), all `Option` and defaulting to unset
}

impl AddRequest {
    pub fn new(owner_id: i64, owner_type: &str, owner_category: OwnerCategory,
               data: TimeSeriesData) -> Self;              // everything else unset
    pub fn with_features(self, features: Features) -> Self;
}
```

A request names no catalog id. Every add — this one, `add_time_series`, and both association
catalogs' — lets the catalog assign, and returns the [`TimeSeriesId`](#timeseriesid-and-keyidentity)
it chose. The one writer that files rows under ids a caller supplies is `import_association_rows`,
replaying a document that already recorded them; see
[Association ids](../explanation/data-model.md#association-ids).

### `BulkAdd`

The buffered bulk-add session returned by [`Store::bulk_add`](#store). Requests accumulate in memory
— no validation and no I/O until `commit`, which writes every array as a batch-sized block and
inserts every association in one transaction, all-or-nothing. Dropping the session without
committing discards the buffer and writes nothing.

```rust
impl BulkAdd<'_> {
    pub fn push(&mut self, request: AddRequest) -> &mut Self;   // prebuilt request
    pub fn add(                                                 // …or from its parts
        &mut self,
        owner_id: i64,
        owner_type: &str,
        owner_category: OwnerCategory,
        data: TimeSeriesData,
        features: Features,
    ) -> &mut Self;
    pub fn len(&self) -> usize;          // requests buffered so far
    pub fn is_empty(&self) -> bool;
    pub fn commit(self) -> Result<Vec<TimeSeriesId>>;           // in push order
}
```

### Requested types

What a query — a `ListFilter`, whether on `list_metadata`, an existence probe, or a reader build —
is asked to match. Every type matches only itself, with one exception: **`Deterministic` also
matches a stored `DeterministicSingleTimeSeries`**, since a DST is a synthetic view that reads back
as a `Deterministic` and callers should not have to know which form a store holds. (The two never
coexist for one identity, so this never creates ambiguity.) Requesting
`DeterministicSingleTimeSeries` narrows to the derived form.

```rust
impl TimeSeriesType {
    /// Does a stored series of type `stored` satisfy a request for `self`?
    pub fn accepts(self, stored: TimeSeriesType) -> bool;
    /// The same rule as catalog type names, for the SQL predicates.
    pub fn stored_names(self) -> &'static [&'static str];
}
```

### Association types

The row, predicate, and grouped-row types of the two [association catalogs](#associations). All
derive `Serialize`/`Deserialize`, so a binding can hand a whole filter or a whole batch across a
language boundary as one JSON value. The rows also derive `PartialEq`/`Eq`/`Hash`, so they work in
sets and as map keys.

```rust
// One attachment: a supplemental attribute carried by a component. Identity is
// the (component_id, attribute_id) pair; the type names are denormalized labels.
pub struct SupplementalAttributeAssociation {
    pub component_id: i64,
    pub component_type: String,
    pub attribute_id: i64,
    pub attribute_type: String,
}

// One directed edge between two components. Identity is the *ordered*
// (parent_id, child_id) pair, so the reversed pair is a different edge.
pub struct ParentChildAssociation {
    pub parent_id: i64,
    pub parent_type: String,
    pub child_id: i64,
    pub child_type: String,
}

// One grouped row of `supplemental_attribute_summary`; `count` is how many
// attachments share the (component_type, attribute_type) pair.
pub struct SupplementalAttributeSummaryRow {
    pub component_type: String,
    pub attribute_type: String,
    pub count: i64,
}
```

The two filters are builders, like [`ListFilter`](#listfilter): every field is optional and the set
ones are combined with AND, so `::new()` / `::default()` matches every row.

```rust
pub struct SupplementalAttributeFilter {
    pub component_id: Option<i64>,
    pub component_types: Option<Vec<String>>,
    pub attribute_id: Option<i64>,
    pub attribute_types: Option<Vec<String>>,
}

pub struct ParentChildFilter {
    pub parent_id: Option<i64>,
    pub parent_types: Option<Vec<String>>,
    pub child_id: Option<i64>,
    pub child_types: Option<Vec<String>>,
}
```

```rust
SupplementalAttributeFilter::new()
    .component_id(1)
    .component_types(["Generator", "Load"])   // concrete type names, rendered as SQL `IN (…)`
    .attribute_id(100)
    .attribute_types(["GeographicInfo"])

ParentChildFilter::new()
    .parent_id(1)
    .parent_types(["Generator"])
    .child_id(7)
    .child_types(["Bus"])
```

The `*_types` lists take **concrete** type names only; expanding an abstract type into its subtypes
stays with the caller, where the type hierarchy lives. An empty list is an empty allow-list and
matches nothing (as opposed to leaving the field unset, which matches everything).

### Report and count types

```rust
pub struct TimeSeriesCounts {
    pub components_with_time_series: i64,
    pub static_time_series: i64,
    pub forecasts: i64,
}
// Owner- and array-oriented counts (`time_series_counts_detailed`). Unlike
// `TimeSeriesCounts`, the series counts here are deduplicated by array content
// and owners are split by category.
pub struct TimeSeriesCountsDetailed {
    pub components_with_time_series: i64,
    pub supplemental_attributes_with_time_series: i64,
    pub static_time_series_count: i64,
    pub forecast_count: i64,
}
// One grouped row of `static_summary` / `forecast_summary`; `count` is the
// number of associations in the group.
pub struct StaticSummaryRow {
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub initial_timestamp: Option<DateTime<Utc>>,
    pub resolution: Option<Period>,
    pub time_step_count: Option<i64>,
    pub count: i64,
}
pub struct ForecastSummaryRow {
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub initial_timestamp: Option<DateTime<Utc>>,
    pub resolution: Option<Period>,
    pub horizon: Option<Period>,
    pub interval: Option<Period>,
    pub window_count: Option<i64>,
    pub count: i64,
}
pub struct CompactionReport {   // on-disk compaction rewrites the .h5; see the file-format reference
    pub slots_reclaimed: usize,
    pub datasets_dropped: usize,
    pub feature_sets_reclaimed: usize,
    pub timestamp_sets_reclaimed: usize,
    pub bytes_reclaimed: u64,   // how much smaller the file got; 0 for an in-memory store
}
pub struct IntegrityReport { pub errors: Vec<String> }  // .ok() == errors.is_empty()
pub struct ForecastParameters {
    pub horizon: Option<Period>, pub interval: Option<Period>,
    pub count: Option<usize>, pub resolution: Option<Period>,
    pub initial_timestamp: Option<DateTime<Utc>>,
}
pub struct StaticConsistency {  // one row per resolution from check_static_consistency
    pub resolution: Period,
    pub initial_timestamp: DateTime<Utc>,
    pub length: usize,
}
```

## Errors

```rust
pub type Result<T> = std::result::Result<T, TimeSeriesError>;

#[non_exhaustive] // match with a wildcard arm; new variants are not semver breaks
pub enum TimeSeriesError {
    NotFound,
    DuplicateTimeSeries,
    /// An association with the same identity already exists — the
    /// `(component_id, attribute_id)` pair of an attachment, or the ordered
    /// `(parent_id, child_id)` pair of an edge. The payload names the offending
    /// pair; it is a human-readable message, not a parseable encoding.
    DuplicateAssociation(String),
    InvalidParameter(String),
    IntegrityError(String),
    ReadOnlyStore,
    ConnectionError(String),
    IncompatibleForecast,
    /// The store on disk was written in a different, incompatible on-disk
    /// format. There is no in-place upgrade; see the file-format reference.
    IncompatibleFormat { found: String, expected: &'static str },
    /// The two halves do not carry the same generation stamp, so they came from
    /// different saves. Both unstamped (an artifact predating the stamp) is
    /// legal; exactly one stamped is not. `"none"` renders a missing stamp.
    MismatchedArtifact { h5: String, sqlite: String },
    /// A store already exists where one was about to be created. See
    /// [`create_store`](#constructors).
    StoreExists { path: String },
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Serde(serde_json::Error),
}
```

## `StorageBackend` Trait

The seam between `Store` and array storage. Implemented by `MemoryBackend` and `Hdf5Backend`. You
rarely call it directly, but it documents the backend contract. It is **not** re-exported at the
crate root — import it (and the backends) from the `storage` module:

```rust
use infrastore_core::storage::{MemoryBackend, Hdf5Backend, StorageBackend};
```

Every method below with a default is a performance override: the default is correct but naive, and
`Hdf5Backend` implements a faster path (single hyperslab reads, whole-chunk block writes).

```rust
pub trait StorageBackend: Send + Sync {
    // --- required ---

    // `packed = true` column-packs same-shaped arrays (SingleTimeSeries / DST);
    // `packed = false` stores a standalone multi-dim variable (NonSequential, dense forecasts).
    // Idempotent on hash: returns `true` only if this call physically wrote new content.
    fn put_array(
        &mut self,
        hash: &[u8; 32],
        data: &TypedArray,
        resolution: Period,
        packed: bool,
    ) -> Result<bool>;
    fn get_array(&self, hash: &[u8; 32]) -> Result<TypedArray>;
    // Slice along axis 0 (the time axis); `range` end is exclusive.
    fn get_slice(&self, hash: &[u8; 32], range: Range<usize>) -> Result<TypedArray>;
    fn remove_array(&mut self, hash: &[u8; 32]) -> Result<()>;   // no-op if absent
    fn contains(&self, hash: &[u8; 32]) -> Result<bool>;
    fn compact(&mut self) -> Result<CompactionReport>;   // in-memory path only; `Store::compact`
                                                        // rewrites the file for an on-disk store
    fn verify(&self) -> Result<IntegrityReport>;
    fn flush(&mut self) -> Result<()>;

    // --- provided (overridden by Hdf5Backend) ---

    // Write a block of same-shaped packed arrays at once (the bulk-add write path).
    // The returned Vec is aligned to `hashes`: `true` where this call wrote new content.
    fn put_packed_block(
        &mut self,
        hashes: &[[u8; 32]],
        arrays: &[&TypedArray],
        resolution: Period,
    ) -> Result<Vec<bool>>;
    // Read many whole arrays at once (`Store::read_by_ids`): one decompress pass per dataset.
    fn read_arrays(&self, hashes: &[[u8; 32]]) -> Result<Vec<TypedArray>>;
    // One time step across co-located arrays (`StaticReader`); `out` is cleared, then
    // filled row-major as [column, *element_shape]. Reusing the buffer keeps the loop
    // allocation-free.
    fn read_index_into(&self, hashes: &[[u8; 32]], index: usize, out: &mut Vec<u8>) -> Result<()>;
    // Stored (dtype, shape), ideally without reading the data.
    fn array_shape(&self, hash: &[u8; 32]) -> Result<(Dtype, Vec<usize>)>;
    // One forecast window: the `window_index` slice along `count_axis`, that axis dropped.
    fn read_window_into(
        &self,
        hash: &[u8; 32],
        count_axis: usize,
        window_index: usize,
        out: &mut Vec<u8>,
    ) -> Result<()>;
    // `len` consecutive steps from `start` along axis 0 (backs DST window reads).
    fn read_range_into(
        &self,
        hash: &[u8; 32],
        start: usize,
        len: usize,
        out: &mut Vec<u8>,
    ) -> Result<()>;
    // The compression policy applied to writes; defaults to `Compression::None`
    // (in-memory backends never compress).
    fn compression(&self) -> Compression;
}
```

## Hashing

In the `hash` module (`infrastore_core::hash`). `array_hash` and `hash_hex` are also re-exported at
the crate root; `features_hash` is only reachable through the module.

```rust
pub fn array_hash(data: &TypedArray) -> [u8; 32];   // domain: dtype tag + shape + typed bytes
pub fn features_hash(features: &Features) -> [u8; 32];
pub fn hash_hex(hash: &[u8; 32]) -> String;
```

These define the cross-language content-addressing contract; see
[Content Addressing](../explanation/content-addressing.md).

## Constants

```rust
pub const DATA_FORMAT_VERSION: &str = "0.11.0";
```
