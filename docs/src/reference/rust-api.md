# Rust API

The public surface of `infrastore-core`. Import paths below are relative to the crate root.

```rust
use infrastore_core::{
    create_store, open_store, Store, BulkAdd, TimeSeriesKey, KeyIdentity,
    SingleTimeSeries, NonSequentialTimeSeries, Deterministic, Probabilistic, Scenarios,
    TimeSeriesData, TimeSeriesType, RequestedType, Period,
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
pub fn open_store(path: &Path, read_only: bool) -> Result<Store>
```

- `create_store(None, true)` — in-memory store, no filesystem I/O.
- `create_store(Some(path), false)` — creates `path` (NetCDF) and `path.sqlite` (metadata).
- `create_store_with_compression(...)` — as above but with an explicit NetCDF compression policy.
- `open_store(path, read_only)` — opens an existing pair. `read_only = true` rejects all writes.

`Store::create` / `Store::create_with_compression` / `Store::open` are the inherent-method
equivalents.

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
        units: Option<String>,
    ) -> Result<TimeSeriesKey>;

    // A managed batch: packed series are written into batch-sized datasets that
    // fill whole HDF5 chunks (the optimized bulk-write path).
    pub fn add_time_series_bulk(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesKey>>;

    // Begin a buffered bulk add. Requests pushed onto the returned guard are
    // accumulated in memory and written together by `BulkAdd::commit` (same
    // block-write path as `add_time_series_bulk`); dropping without committing
    // discards the buffer.
    pub fn bulk_add(&mut self) -> BulkAdd<'_>;

    // Copy one association onto another owner (metadata only; the array is shared).
    pub fn copy_time_series(
        &mut self,
        src: &KeyIdentity,
        dst_owner_id: i64,
        dst_owner_type: &str,
        new_name: Option<&str>,   // None keeps the source name
    ) -> Result<TimeSeriesKey>;

    pub fn get_time_series(
        &self,
        key: &KeyIdentity,
        time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<TimeSeriesData>;

    // Read many full series at once (no time-range slicing). Packed
    // `SingleTimeSeries` are read in one decompress-once pass per dataset; other
    // types reuse the per-key path. Returns a `TimeSeriesData` per key, in order.
    pub fn bulk_read(&self, keys: &[&KeyIdentity]) -> Result<Vec<TimeSeriesData>>;

    pub fn transform_single_time_series(
        &mut self,
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        owner_category: Option<OwnerCategory>,
        resolution: Option<Period>,
    ) -> Result<usize>;

    pub fn remove_time_series(&mut self, key: &KeyIdentity) -> Result<()>;
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

    pub fn list_time_series(&self, filter: ListFilter) -> Result<Vec<TimeSeriesMetadata>>;
    // Key-centric listing: the same rows reduced to their keys, dropping storage
    // detail (`data_hash`, `dtype`, `ext`, `percentiles`).
    pub fn list_keys(&self, filter: ListFilter) -> Result<Vec<TimeSeriesKey>>;
    // …and with each key's array content hash, so callers can group series that
    // share stored data (dedup'd arrays; an STS and any DST derived from it).
    pub fn list_keys_with_hash(
        &self,
        filter: ListFilter,
    ) -> Result<Vec<(TimeSeriesKey, [u8; 32])>>;
    pub fn get_time_series_keys(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
    ) -> Result<Vec<TimeSeriesKey>>;
    pub fn has_time_series(&self, key: &KeyIdentity) -> Result<bool>;
    // Existence over a filter without listing: "does this owner have any time
    // series (of type T)?". Both probes answer from a covering index and are
    // safe for hot loops.
    pub fn has_any_time_series(&self, filter: ListFilter) -> Result<bool>;

    // Resolve a forecast addressed by attributes + a `RequestedType` to the one
    // matching key. `NotFound` if nothing matches; `InvalidParameter` if ambiguous.
    pub fn resolve_forecast_key(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
        name: &str,
        resolution: Option<Period>,
        interval: Option<Period>,
        features: Features,
        requested: RequestedType,
    ) -> Result<TimeSeriesKey>;

    pub fn get_metadata(&self, key: &KeyIdentity) -> Result<TimeSeriesMetadata>;
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
  `SingleTimeSeries`, `NonSequentialTimeSeries`, or a dense forecast (`Deterministic`,
  `Probabilistic`, `Scenarios`). Hashes the array, stores it (deduplicating on the hash), inserts a
  metadata association, and returns its key. Errors with `DuplicateTimeSeries` if the key already
  exists or `ReadOnlyStore` on a read-only store. It is a convenience wrapper over
  `add_time_series_bulk`.
- **`transform_single_time_series`** — Derives a `DeterministicSingleTimeSeries` from every stored
  `SingleTimeSeries`, sharing the underlying array (with `count` derived from the series length),
  and returns the number of series transformed. This is the only way to create a
  `DeterministicSingleTimeSeries`; it is never added directly. The optional `owner_category` and
  `resolution` filters restrict the transform to a single owner category and/or resolution, leaving
  other series untouched.
- **`add_time_series_bulk`** — All-or-nothing: every array put and association insert in the call
  commits together or rolls back together.
- **`get_time_series`** — Reconstructs the stored type as a [`TimeSeriesData`](#timeseriesdata)
  variant (static series and all forecast types). With `time_range = Some((start, end))`, slices on
  the time axis; the returned series's `initial_timestamp` and `length` reflect the slice. For
  forecasts the window is resolved over the `count` axis (`resolve_windows`). `end` is exclusive.
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
- **`get_time_series_keys`** — Lists every key for the owner identified by the
  `(owner_id, owner_category)` pair.
- **`list_keys` / `list_keys_with_hash`** — The key-centric listing path (what the bindings use).
  `list_keys_with_hash` pairs each key with its array's content hash in the same single catalog
  query, so callers can group keys by the data behind them.
- **`resolve_forecast_key`** — Resolves a forecast addressed by attributes plus a
  [`RequestedType`](#requestedtype) to the single matching key, whose `time_series_type` is the
  concrete type that matched. `resolution` and `interval` are optional filters; leave them `None` to
  match across them. `NotFound` if nothing matches, `InvalidParameter` if more than one does.
- **`get_metadata` / `get_array_by_hash`** — The low-level pair used by external bindings: resolve a
  key to metadata (including `data_hash`), then read the array directly.
- **`count_array_references`** — `(sts, dst)` association counts referencing one `data_hash`, so a
  caller can tell whether removing a `SingleTimeSeries` would orphan a
  `DeterministicSingleTimeSeries` derived from (and sharing) its array.
- **`verify_integrity`** — Recomputes each stored array's hash and reports mismatches. Covers the
  NetCDF half only: the SQLite catalog is not inspected, so an empty report does not mean the store
  as a whole is sound. See
  [content addressing](../explanation/content-addressing.md#what-it-does-not-cover).
- **`flush`** — Issues `nc_sync` so the files can be copied for persistence without closing.
- **`persist_to`** — Writes both halves of the artifact to `path` and `<path>.sqlite`, overwriting
  existing targets. An on-disk store is flushed and copied; an in-memory store is materialized
  (every distinct array by hash, plus the whole catalog). Because arrays are content-addressed, this
  reproduces every series — static, forecast, non-sequential — without per-type reconstruction.

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
NetCDF variables. A `DeterministicSingleTimeSeries` is **not** added directly: call
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

**Reading forecasts:** `get_time_series` reconstructs all forecast types, returning the matching
[`TimeSeriesData`](#timeseriesdata) variant — `Deterministic`, `Probabilistic`, or `Scenarios`. A
`DeterministicSingleTimeSeries` is synthesized into a `Deterministic` by gathering its windows from
the underlying packed array. The low-level pair still works for direct array access: resolve a
[`TimeSeriesMetadata`](#timeseriesmetadata) with `get_metadata` (it carries `horizon`, `interval`,
`count`, and `percentiles`), then fetch the array with `get_array_by_hash(&meta.data_hash)`.

### Readers

`get_time_series` returns a whole series or forecast. To read **many whole series at once** (e.g.
exploration or plotting), `bulk_read` takes a slice of keys and reads packed `SingleTimeSeries` in
one decompress-once pass per dataset — far cheaper than a `get_time_series` per key under the
timestamp-major chunking, where a single full-series read touches every chunk. For the
timestamp-oriented access pattern — _walk the timeline and read every series' value at each instant_
— build a **reader** instead. A reader is built once over a [`ListFilter`](#listfilter), pins one
resolution, and holds reusable buffers that each read overwrites in place, so a tight loop allocates
nothing. The reader is a passive plan: it does not borrow the `Store`, so reads go through
`Store::static_read` / `Store::forecast_read`, which fill the buffers; the caller then walks the
groups/entries. There are two: [`StaticReader`](#staticreader-and-staticgroup) for
`SingleTimeSeries` and [`ForecastReader`](#forecastreader-windowslot-and-forecastentry) for
forecasts.

```rust
// Static: value of every SingleTimeSeries at one timestamp, columnar.
let mut reader = store.build_static_reader(ListFilter::new().resolution(res))?;
for k in 0..reader.length() {
    // Grid point k: initial + k·resolution (calendar-aware, hence `Period::add_to`).
    let at = reader.resolution().add_to(reader.initial_timestamp(), k as i64).unwrap();
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
        // entry.key() identifies the forecast/owner
    }
}
```

`build_static_reader` requires the filter to pin a resolution and that all matched series share one
grid (`initial_timestamp` + `length`) — validated at build, so there is no presence mask.
`build_forecast_reader` requires a forecast type and a resolution; a `Deterministic` reader is
abstract (also matches `DeterministicSingleTimeSeries`), and all matched forecasts must share one
window timeline (`initial_timestamp` + `interval` + `count`). `static_read` / `forecast_read` error
(never clamp) if `at` is off the grid/timeline.

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

### `TimeSeriesKey` and `KeyIdentity`

`TimeSeriesKey` is an enum: one variant per series family, each pairing the shared identity with a
per-variant descriptive snapshot (window/shape parameters). Equality is **identity-only** — two keys
with the same `KeyIdentity` are equal even if their descriptive snapshots differ, so a key stays a
reliable handle.

```rust
pub enum TimeSeriesKey {
    Single(SingleTimeSeriesKey),
    NonSequential(NonSequentialTimeSeriesKey),
    Forecast(ForecastTimeSeriesKey),
}

pub struct SingleTimeSeriesKey {
    pub identity: KeyIdentity,
    pub initial_timestamp: DateTime<Utc>,
    pub length: usize,
}

pub struct NonSequentialTimeSeriesKey {
    pub identity: KeyIdentity,
    pub length: usize,
}

pub struct ForecastTimeSeriesKey {
    pub identity: KeyIdentity,
    pub initial_timestamp: DateTime<Utc>,
    pub horizon: Period,
    pub count: usize,
}
```

`KeyIdentity` is the identifying tuple shared by every variant — and the only thing that determines
equality and is looked up in the catalog. `interval` is part of the identity (`Some` for every
forecast type, `None` for the static types); `resolution` is `Option` because
`NonSequentialTimeSeries` has none.

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

`TimeSeriesKey::identity()` returns `&KeyIdentity`; the lookup methods (`get_time_series`,
`get_metadata`, `has_time_series`, `remove_time_series`, `bulk_read`) take `&KeyIdentity`. See
[Data Model](../explanation/data-model.md#keys).

### `SingleTimeSeries`

```rust
pub struct SingleTimeSeries {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Period,
    pub length: usize,
    pub data: TypedArray,
    pub name: String,
}

impl SingleTimeSeries {
    pub fn new(
        initial_timestamp: DateTime<Utc>, resolution: impl Into<Period>, data: TypedArray,
        name: impl Into<String>,
    ) -> Self;
}
```

`length` is derived from the array's first axis (`data.length()`) by `new`.

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

The columnar `SingleTimeSeries` reader (see [Readers](#readers)). `Period` is the crate's resolution
type. `values()` is empty until the first `Store::static_read`.

```rust
impl StaticReader {
    pub fn initial_timestamp(&self) -> DateTime<Utc>;
    pub fn resolution(&self) -> Period;
    pub fn length(&self) -> usize;            // grid points; valid timestamps initial + k·resolution
    pub fn groups(&self) -> &[StaticGroup];
    pub fn index_at(&self, at: DateTime<Utc>) -> Result<usize>;
}

impl StaticGroup {
    pub fn dtype(&self) -> Dtype;
    pub fn element_shape(&self) -> &[usize];  // trailing per-step dims; empty == scalar
    pub fn keys(&self) -> &[TimeSeriesKey];   // column j identity
    pub fn num_columns(&self) -> usize;
    pub fn values(&self) -> &[u8];            // [num_columns, *element_shape], row-major LE
}
```

### `ForecastReader`, `WindowSlot`, and `ForecastEntry`

The forecast-window reader (see [Readers](#readers)). Entries are the per-key forecasts; slots are
the deduplicated physical reads. `WindowSlot::window()` is empty until the first
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
    pub fn key(&self) -> &TimeSeriesKey;
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
    Deterministic(Deterministic),
    Probabilistic(Probabilistic),
    Scenarios(Scenarios),
}

impl TimeSeriesData {
    pub fn time_series_type(&self) -> TimeSeriesType;
    pub fn as_single(&self) -> Option<&SingleTimeSeries>;
    pub fn as_non_sequential(&self) -> Option<&NonSequentialTimeSeries>;
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
}
```

`as_str()` / `parse(&str)` convert to and from the canonical string names used on disk.

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

The full record returned by `list_time_series` and `get_metadata`: owner fields, `time_series_type`,
`name`, `data_hash: [u8; 32]`, the optional temporal fields (`initial_timestamp`, `resolution`,
`length`, `horizon`, `interval`, `count`, `timestamps`), `features`, `units`,
`percentiles: Option<Vec<f64>>` (set for `Probabilistic`), and the array typing: `dtype: Dtype`,
`element_shape: Vec<usize>`, and `ext: Option<String>`. The span fields (`resolution`, `horizon`,
`interval`) are `Option<Period>`.

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
    .resolution(Duration::hours(1))   // impl Into<Period>
    .interval(Duration::hours(24))    // impl Into<Period>; forecasts only
    .features(features)   // subset match: rows must contain at least these pairs
```

### `AddRequest`

The element type of `add_time_series_bulk` (and of `BulkAdd::push`), mirroring the `add_time_series`
arguments plus an optional `ext` — an opaque, package-owned payload (typically JSON) stored
verbatim. The series name lives on the `TimeSeriesData` object, not here.

```rust
pub struct AddRequest {
    pub owner_id: i64,
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub data: TimeSeriesData,
    pub features: Features,
    pub units: Option<String>,
    pub ext: Option<String>,
}
```

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
        units: Option<String>,
    ) -> &mut Self;                                             // ext = None
    pub fn len(&self) -> usize;          // requests buffered so far
    pub fn is_empty(&self) -> bool;
    pub fn commit(self) -> Result<Vec<TimeSeriesKey>>;          // keys in push order
}
```

### `RequestedType`

What [`Store::resolve_forecast_key`](#store) is asked to match: one concrete stored type, or the
abstract "deterministic" family, which a stored `Deterministic` **or**
`DeterministicSingleTimeSeries` satisfies (the two never coexist for one series, so at most one can
match).

```rust
pub enum RequestedType {
    Concrete(TimeSeriesType),
    AbstractDeterministic,
}

impl RequestedType {
    pub fn matches(self, concrete: TimeSeriesType) -> bool;
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
pub struct CompactionReport {
    pub slots_reclaimed: usize,
    pub datasets_dropped: usize,
    pub feature_sets_reclaimed: usize,
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
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Serde(serde_json::Error),
}
```

## `StorageBackend` Trait

The seam between `Store` and array storage. Implemented by `MemoryBackend` and `NetCdfBackend`. You
rarely call it directly, but it documents the backend contract. It is **not** re-exported at the
crate root — import it (and the backends) from the `storage` module:

```rust
use infrastore_core::storage::{MemoryBackend, NetCdfBackend, StorageBackend};
```

Every method below with a default is a performance override: the default is correct but naive, and
`NetCdfBackend` implements a faster path (single hyperslab reads, whole-chunk block writes).

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
    fn compact(&mut self) -> Result<CompactionReport>;
    fn verify(&self) -> Result<IntegrityReport>;
    fn flush(&mut self) -> Result<()>;

    // --- provided (overridden by NetCdfBackend) ---

    // Write a block of same-shaped packed arrays at once (the bulk-add write path).
    // The returned Vec is aligned to `hashes`: `true` where this call wrote new content.
    fn put_packed_block(
        &mut self,
        hashes: &[[u8; 32]],
        arrays: &[&TypedArray],
        resolution: Period,
    ) -> Result<Vec<bool>>;
    // Read many whole arrays at once (`Store::bulk_read`): one decompress pass per dataset.
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
