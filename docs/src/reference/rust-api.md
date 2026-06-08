# Rust API

The public surface of `time-series-store-core`. Import paths below are relative to the crate root.

```rust
use time_series_store_core::{
    create_store, open_store, Store, TimeSeriesKey,
    SingleTimeSeries, NonSequentialTimeSeries, TimeSeriesData, TimeSeriesType,
    TypedArray, Dtype, OwnerCategory, FeatureValue, Features, ListFilter, AddRequest,
    TimeSeriesCounts, ForecastParameters, CompactionReport, IntegrityReport,
    TimeSeriesError, Result, DATA_FORMAT_VERSION,
};
```

## Constructors

```rust
pub fn create_store(path: Option<&Path>, in_memory: bool) -> Result<Store>
pub fn open_store(path: &Path, read_only: bool) -> Result<Store>
```

- `create_store(None, true)` — in-memory store, no filesystem I/O.
- `create_store(Some(path), false)` — creates `path` (NetCDF) and `path.sqlite` (metadata).
- `open_store(path, read_only)` — opens an existing pair. `read_only = true` rejects all writes.

`Store::create` / `Store::open` are the inherent-method equivalents.

## `Store`

```rust
impl Store {
    pub fn read_only(&self) -> bool;

    pub fn add_time_series(
        &mut self,
        owner_uuid: &str,
        owner_type: &str,
        owner_category: OwnerCategory,
        name: &str,
        data: TimeSeriesData,
        features: Features,
        units: Option<String>,
        scaling_factor_multiplier: Option<String>,
    ) -> Result<TimeSeriesKey>;

    pub fn add_time_series_bulk(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesKey>>;

    pub fn get_time_series(
        &self,
        key: &TimeSeriesKey,
        time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<TimeSeriesData>;

    pub fn remove_time_series(&mut self, key: &TimeSeriesKey) -> Result<()>;
    pub fn clear_time_series(&mut self, owner_uuid: Option<&str>) -> Result<usize>;

    pub fn list_time_series(&self, filter: ListFilter) -> Result<Vec<TimeSeriesMetadata>>;
    pub fn get_time_series_keys(&self, owner_uuid: &str) -> Result<Vec<TimeSeriesKey>>;
    pub fn has_time_series(&self, key: &TimeSeriesKey) -> Result<bool>;

    pub fn get_metadata(&self, key: &TimeSeriesKey) -> Result<TimeSeriesMetadata>;
    pub fn get_array_by_hash(&self, hash: &[u8; 32]) -> Result<TypedArray>;

    pub fn get_resolutions(&self, ts_type: Option<TimeSeriesType>) -> Result<Vec<Duration>>;
    pub fn get_time_series_counts(&self) -> Result<TimeSeriesCounts>;
    pub fn get_forecast_parameters(&self) -> Result<ForecastParameters>;

    pub fn compact(&mut self) -> Result<CompactionReport>;
    pub fn verify_integrity(&self) -> Result<IntegrityReport>;
    pub fn flush(&mut self) -> Result<()>;
}
```

### Method notes

- **`add_time_series`** — Accepts a `SingleTimeSeries` or `NonSequentialTimeSeries` (wrapped in
  `TimeSeriesData`). Hashes the array, stores it (deduplicating on the hash), inserts a metadata
  association, and returns its key. Errors with `DuplicateTimeSeries` if the key already exists or
  `ReadOnlyStore` on a read-only store. It is a convenience wrapper over `add_time_series_bulk`.
- **`add_time_series_bulk`** — All-or-nothing: every array put and association insert in the call
  commits together or rolls back together.
- **`get_time_series`** — With `time_range = Some((start, end))`, slices on the time axis; the
  returned series's `initial_timestamp` and `length` reflect the slice. `end` is exclusive.
- **`clear_time_series`** — `Some(uuid)` removes one owner's series; `None` removes all. Returns the
  count removed. Underlying arrays are freed only when their last reference is gone.
- **`get_metadata` / `get_array_by_hash`** — The low-level pair used by external bindings: resolve a
  key to metadata (including `data_hash`), then read the array directly.
- **`verify_integrity`** — Recomputes each stored array's hash and reports mismatches.
- **`flush`** — Issues `nc_sync` so the files can be copied for persistence without closing.

### Forecasts

The four forecast types (`Deterministic`, `DeterministicSingleTimeSeries`, `Probabilistic`,
`Scenarios`) are written with `add_forecast`. `data` is a [`TypedArray`](#typedarray-and-dtype) in
its native shape; the windowing parameters (`horizon`, `interval`, `count`, and for `Probabilistic`
the `percentiles`) plus an optional `logical_type` are recorded in metadata. Dense forecast arrays
(`Deterministic` / `Probabilistic` / `Scenarios`) are stored as standalone NetCDF variables; a
`DeterministicSingleTimeSeries` stores the underlying `SingleTimeSeries` array (column-packed) and
dedups against that series.

```rust
#[allow(clippy::too_many_arguments)]
pub fn add_forecast(
    &mut self,
    owner_uuid: &str, owner_type: &str, owner_category: OwnerCategory, name: &str,
    time_series_type: TimeSeriesType,
    initial_timestamp: DateTime<Utc>, resolution: Duration,
    horizon: Duration, interval: Duration, count: usize,
    data: TypedArray, features: Features,
    units: Option<String>, scaling_factor_multiplier: Option<String>,
    percentiles: Option<Vec<f64>>, logical_type: Option<String>,
) -> Result<TimeSeriesKey>;
```

Conventional array shapes:

| Type                            | `data` shape                                  | `percentiles` |
| ------------------------------- | --------------------------------------------- | ------------- |
| `Deterministic`                 | `(horizon_count, count)`                      | `None`        |
| `DeterministicSingleTimeSeries` | the backing `SingleTimeSeries` array (dedups) | `None`        |
| `Probabilistic`                 | `(percentile_count, horizon_count, count)`    | the vector    |
| `Scenarios`                     | `(scenario_count, horizon_count, count)`      | `None`        |

**Reading forecasts:** `get_time_series` reconstructs `SingleTimeSeries` and
`NonSequentialTimeSeries` only and returns `InvalidParameter` for forecast types. Read a forecast
through the low-level pair instead — resolve its [`TimeSeriesMetadata`](#timeseriesmetadata) with
`get_metadata` (it carries `horizon`, `interval`, `count`, and `percentiles`), then fetch the array
with `get_array_by_hash(&meta.data_hash)`.

## Types

### `TimeSeriesKey`

```rust
pub struct TimeSeriesKey {
    pub owner_uuid: String,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub resolution: Option<Duration>,
    pub features: Features,
}
```

The unique handle for a series; see [Data Model](../explanation/data-model.md#keys).

### `SingleTimeSeries`

```rust
pub struct SingleTimeSeries {
    pub initial_timestamp: DateTime<Utc>,
    pub resolution: Duration,
    pub length: usize,
    pub data: TypedArray,
}

impl SingleTimeSeries {
    pub fn new(initial_timestamp: DateTime<Utc>, resolution: Duration, data: TypedArray) -> Self;
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

### `NonSequentialTimeSeries`

```rust
pub struct NonSequentialTimeSeries {
    pub timestamps: Vec<DateTime<Utc>>,
    pub length: usize,
    pub data: TypedArray,
}
```

`new` validates that timestamps are strictly increasing and match the data length.

### `TimeSeriesData`

```rust
pub enum TimeSeriesData {
    SingleTimeSeries(SingleTimeSeries),
    NonSequentialTimeSeries(NonSequentialTimeSeries),
    // No forecast variants: forecasts are written with `add_forecast` and read
    // via `get_metadata` + `get_array_by_hash`, not through this enum.
}

impl TimeSeriesData {
    pub fn time_series_type(&self) -> TimeSeriesType;
    pub fn as_single(&self) -> Option<&SingleTimeSeries>;
    pub fn as_non_sequential(&self) -> Option<&NonSequentialTimeSeries>;
}
```

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
`length`, `horizon`, `interval`, `count`, `timestamps`), `features`, `scaling_factor_multiplier`,
`units`, `percentiles: Option<Vec<f64>>` (set for `Probabilistic`), and the array typing:
`dtype: Dtype`, `element_shape: Vec<usize>`, and `logical_type: Option<String>`.

### `ListFilter`

A builder; every field is an optional filter, combined with AND.

```rust
ListFilter::new()
    .owner_uuid("42")
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
    pub horizon: Option<Duration>, pub interval: Option<Duration>,
    pub count: Option<usize>, pub resolution: Option<Duration>,
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
        resolution_seconds: i64,
        packed: bool,
    ) -> Result<()>;
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
pub const DATA_FORMAT_VERSION: &str = "0.2.0";
```
