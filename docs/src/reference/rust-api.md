# Rust API

The public surface of `time-series-store-core`. Import paths below are relative to the crate root.

```rust
use time_series_store_core::{
    create_store, open_store, Store, TimeSeriesKey,
    SingleTimeSeries, NonSequentialTimeSeries, Deterministic, Probabilistic, Scenarios,
    TimeSeriesData, TimeSeriesType, Period,
    TypedArray, Dtype, OwnerCategory, FeatureValue, Features, ListFilter, AddRequest,
    TimeSeriesCounts, ForecastParameters, CompactionReport, IntegrityReport,
    TimeSeriesError, Result, DATA_FORMAT_VERSION,
};
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
        name: &str,
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

    pub fn get_time_series(
        &self,
        key: &TimeSeriesKey,
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
    ) -> Result<usize>;

    pub fn remove_time_series(&mut self, key: &TimeSeriesKey) -> Result<()>;
    pub fn clear_time_series(
        &mut self,
        owner: Option<(i64, OwnerCategory)>,
    ) -> Result<usize>;
    pub fn replace_owner(
        &mut self,
        old_owner_id: i64,
        new_owner_id: i64,
        owner_category: OwnerCategory,
    ) -> Result<usize>;

    pub fn list_time_series(&self, filter: ListFilter) -> Result<Vec<TimeSeriesMetadata>>;
    pub fn get_time_series_keys(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
    ) -> Result<Vec<TimeSeriesKey>>;
    pub fn has_time_series(&self, key: &TimeSeriesKey) -> Result<bool>;

    pub fn get_metadata(&self, key: &TimeSeriesKey) -> Result<TimeSeriesMetadata>;
    pub fn get_array_by_hash(&self, hash: &[u8; 32]) -> Result<TypedArray>;

    pub fn get_resolutions(&self, ts_type: Option<TimeSeriesType>) -> Result<Vec<Period>>;
    pub fn get_time_series_counts(&self) -> Result<TimeSeriesCounts>;
    pub fn get_forecast_parameters(&self) -> Result<ForecastParameters>;

    // Per-timestamp readers (see "Readers" below).
    pub fn build_static_reader(&self, filter: ListFilter) -> Result<StaticReader>;
    pub fn static_read(&self, reader: &mut StaticReader, at: DateTime<Utc>) -> Result<()>;
    pub fn build_forecast_reader(&self, filter: ListFilter) -> Result<ForecastReader>;
    pub fn forecast_read(&self, reader: &mut ForecastReader, at: DateTime<Utc>) -> Result<()>;

    pub fn compact(&mut self) -> Result<CompactionReport>;
    pub fn verify_integrity(&self) -> Result<IntegrityReport>;
    pub fn flush(&mut self) -> Result<()>;
}
```

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
  `DeterministicSingleTimeSeries`; it is never added directly.
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
- **`get_time_series_keys`** — Lists every key for the owner identified by the
  `(owner_id, owner_category)` pair.
- **`get_metadata` / `get_array_by_hash`** — The low-level pair used by external bindings: resolve a
  key to metadata (including `data_hash`), then read the array directly.
- **`verify_integrity`** — Recomputes each stored array's hash and reports mismatches.
- **`flush`** — Issues `nc_sync` so the files can be copied for persistence without closing.

### Forecasts

Dense forecasts (`Deterministic`, `Probabilistic`, `Scenarios`) are written through the generic
[`add_time_series`](#store) by wrapping the corresponding object in a
[`TimeSeriesData`](#timeseriesdata) variant. Build the object with its `new` constructor — each
holds a [`TypedArray`](#typedarray-and-dtype) in its native shape, and the constructor validates the
shape against the windowing parameters (`horizon`, `interval`, `count`, and for `Probabilistic` the
`percentiles`):

```rust
use time_series_store_core::{Deterministic, TimeSeriesData};

let forecast = Deterministic::new(
    initial_timestamp, resolution, horizon, interval, count, data,
)?;
let key = store.add_time_series(
    owner_id, owner_type, OwnerCategory::Component, name,
    TimeSeriesData::Deterministic(forecast),
    features, units,
)?;
```

Dense forecast arrays (`Deterministic` / `Probabilistic` / `Scenarios`) are stored as standalone
NetCDF variables. A `DeterministicSingleTimeSeries` is **not** added directly: call
`transform_single_time_series(horizon, interval)` to derive one from every stored `SingleTimeSeries`
(it shares the backing column-packed array, derives `count` from the series length, and dedups
against that series).

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
    let at = reader.initial_timestamp() + /* k · resolution */;
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
    let at = reader.initial_timestamp() + /* k · interval */;
    store.forecast_read(&mut reader, at)?;
    for entry in reader.entries() {
        let slot = reader.entry_slot(entry.slot());
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
`reader.entry_slot(i)` expose the slots; `entry.slot()` is the slot index backing each entry, equal
for entries that share data.

## Types

### `TimeSeriesKey`

```rust
pub struct TimeSeriesKey {
    pub owner_id: i64,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub resolution: Option<Period>,
    pub features: Features,
}
```

The unique handle for a series; see [Data Model](../explanation/data-model.md#keys).

### `SingleTimeSeries`

```rust
pub struct SingleTimeSeries {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Period,
    pub length: usize,
    pub data: TypedArray,
}

impl SingleTimeSeries {
    pub fn new(
        initial_timestamp: DateTime<Utc>, resolution: impl Into<Period>, data: TypedArray,
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
    pub fn add_to(&self, dt: DateTime<Utc>, k: i64) -> Option<DateTime<Utc>>;  // calendar-aware
}
```

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
}

impl Deterministic {
    pub fn new(
        initial_timestamp: DateTime<Utc>, resolution: impl Into<Period>,
        horizon: impl Into<Period>, interval: impl Into<Period>, count: usize, data: TypedArray,
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
}

impl Probabilistic {
    pub fn new(
        initial_timestamp: DateTime<Utc>, resolution: impl Into<Period>,
        horizon: impl Into<Period>, interval: impl Into<Period>, count: usize,
        percentiles: Vec<f64>, data: TypedArray,
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
}

impl Scenarios {
    pub fn new(
        initial_timestamp: DateTime<Utc>, resolution: impl Into<Period>,
        horizon: impl Into<Period>, interval: impl Into<Period>, count: usize,
        scenario_count: usize, data: TypedArray,
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

### `TimeSeriesMetadata`

The full record returned by `list_time_series` and `get_metadata`: owner fields, `time_series_type`,
`name`, `data_hash: [u8; 32]`, the optional temporal fields (`initial_timestamp`, `resolution`,
`length`, `horizon`, `interval`, `count`, `timestamps`), `features`, `units`,
`percentiles: Option<Vec<f64>>` (set for `Probabilistic`), and the array typing: `dtype: Dtype`,
`element_shape: Vec<usize>`, and `logical_type: Option<String>`. The span fields (`resolution`,
`horizon`, `interval`) are `Option<Period>`.

### `ListFilter`

A builder; every field is an optional filter, combined with AND.

```rust
ListFilter::new()
    .owner_id(42)
    .owner_category(OwnerCategory::Component)
    .time_series_type(TimeSeriesType::SingleTimeSeries)
    .name("load")
    .resolution(Duration::hours(1))
    .features(features)   // subset match: rows must contain at least these pairs
```

### `AddRequest`

The element type of `add_time_series_bulk`, mirroring the `add_time_series` arguments plus an
optional `logical_type: Option<String>` (an opaque, binding-owned domain label).

### Report and count types

```rust
pub struct TimeSeriesCounts {
    pub components_with_time_series: i64,
    pub static_time_series: i64,
    pub forecasts: i64,
}
pub struct CompactionReport { pub slots_reclaimed: usize, pub datasets_dropped: usize }
pub struct IntegrityReport { pub errors: Vec<String> }  // .ok() == errors.is_empty()
pub struct ForecastParameters {
    pub horizon: Option<Period>, pub interval: Option<Period>,
    pub count: Option<usize>, pub resolution: Option<Period>,
    pub initial_timestamp: Option<DateTime<Utc>>,
}
```

## Errors

```rust
pub type Result<T> = std::result::Result<T, TimeSeriesError>;

pub enum TimeSeriesError {
    NotFound,
    DuplicateTimeSeries,
    InvalidParameter(String),
    IntegrityError(String),
    ReadOnlyStore,
    ConnectionError(String),
    IncompatibleForecast,
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Serde(serde_json::Error),
}
```

## `StorageBackend` Trait

The seam between `Store` and array storage. Implemented by `MemoryBackend` and `NetCdfBackend`. You
rarely call it directly, but it documents the backend contract.

```rust
pub trait StorageBackend: Send + Sync {
    // `packed = true` column-packs same-shaped arrays (SingleTimeSeries / DST);
    // `packed = false` stores a standalone multi-dim variable (NonSequential, dense forecasts).
    // idempotent on hash
    fn put_array(
        &mut self,
        hash: &[u8; 32],
        data: &TypedArray,
        resolution: Period,
        packed: bool,
    ) -> Result<bool>;
    fn get_array(&self, hash: &[u8; 32]) -> Result<TypedArray>;
    fn get_slice(&self, hash: &[u8; 32], range: Range<usize>) -> Result<TypedArray>;
    fn remove_array(&mut self, hash: &[u8; 32]) -> Result<()>;
    fn contains(&self, hash: &[u8; 32]) -> Result<bool>;
    fn compact(&mut self) -> Result<CompactionReport>;
    fn verify(&self) -> Result<IntegrityReport>;
    fn flush(&mut self) -> Result<()>;
}
```

## Hashing

```rust
pub fn array_hash(data: &TypedArray) -> [u8; 32];   // domain: dtype tag + shape + typed bytes
pub fn features_hash(features: &Features) -> [u8; 32];
pub fn hash_hex(hash: &[u8; 32]) -> String;
```

These define the cross-language content-addressing contract; see
[Content Addressing](../explanation/content-addressing.md).

## Constants

```rust
pub const DATA_FORMAT_VERSION: &str = "0.6.0";
```
