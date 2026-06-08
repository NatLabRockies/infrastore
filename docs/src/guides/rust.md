# Rust Developer Guide

This guide covers using `time-series-store-core` from Rust. For exact signatures see the
[Rust API reference](../reference/rust-api.md).

## Add the Dependency

The crate is part of this workspace. From another crate in the workspace:

```toml
[dependencies]
time-series-store-core = { path = "crates/time-series-store-core" }
chrono = "0.4"
```

You will use `chrono::Duration`/`DateTime<Utc>` for time and the crate's
[`TypedArray`](../reference/rust-api.md#typedarray-and-dtype) for values, since those are the types
the API speaks.

## Open or Create a Store

```rust
use std::path::Path;
use time_series_store_core::{create_store, open_store};

// In-memory (tests, scratch work): no filesystem I/O.
let mut store = create_store(None, true)?;

// On disk: writes system.nc and system.nc.sqlite.
let mut store = create_store(Some(Path::new("system.nc")), false)?;

// Reopen later, read-only.
let store = open_store(Path::new("system.nc"), /* read_only */ true)?;
```

## Add a Series

```rust
use chrono::{Duration, TimeZone, Utc};
use time_series_store_core::{
    Features, FeatureValue, OwnerCategory, SingleTimeSeries, TimeSeriesData, TypedArray,
};

let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
let values: Vec<f64> = (0..24).map(|i| 100.0 + i as f64).collect();
let data = TypedArray::from_f64(vec![24], &values);   // shape [length]; or [length, k1, ...]
let ts = SingleTimeSeries::new(initial, Duration::hours(1), data);

let mut features = Features::new();
features.insert("model_year".into(), FeatureValue::Int(2030));

let key = store.add_time_series(
    "42",                                   // owner_uuid
    "Generator",                            // owner_type
    OwnerCategory::Component,
    "load",                                 // name
    TimeSeriesData::SingleTimeSeries(ts),
    features,
    Some("MW".into()),                      // units
    None,                                   // scaling_factor_multiplier
)?;
```

`add_time_series` returns a [`TimeSeriesKey`](../reference/rust-api.md#timeserieskey) — keep it to
re-find the series, or rebuild it from its fields later. Adding a series whose key already exists
returns `TimeSeriesError::DuplicateTimeSeries`.

### Bulk inserts

For many series at once, `add_time_series_bulk` takes a `Vec<AddRequest>` and commits the whole
batch atomically — any error rolls back every array and association in the call:

```rust
use time_series_store_core::AddRequest;

let keys = store.add_time_series_bulk(vec![
    AddRequest { owner_uuid: "42".into(), owner_type: "Generator".into(),
        owner_category: OwnerCategory::Component, name: "load".into(),
        data: TimeSeriesData::SingleTimeSeries(ts_a), features: Features::new(),
        units: Some("MW".into()), scaling_factor_multiplier: None },
    // ...
])?;
```

## Read a Series

```rust
let data = store.get_time_series(&key, None)?;
let single = data.as_single().expect("v0 stores SingleTimeSeries");
println!("{} values starting {}", single.length, single.initial_timestamp);
```

Slice on the time axis by passing a range; `end` is exclusive and the returned series'
`initial_timestamp`/`length` reflect the slice:

```rust
let start = Utc.with_ymd_and_hms(2024, 1, 1, 6, 0, 0).unwrap();
let end   = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
let window = store.get_time_series(&key, Some((start, end)))?;
```

## Query Metadata

`list_time_series` takes a [`ListFilter`](../reference/rust-api.md#listfilter) builder; every clause
is ANDed, and the `features` clause is a subset match:

```rust
use time_series_store_core::{ListFilter, TimeSeriesType};

let metas = store.list_time_series(
    ListFilter::new()
        .owner_uuid("42")
        .time_series_type(TimeSeriesType::SingleTimeSeries)
        .name("load"),
)?;
for m in &metas {
    println!("{} {:?} units={:?}", m.name, m.resolution, m.units);
}

// All keys for one owner, an existence check, distinct resolutions, counts.
let keys = store.get_time_series_keys("42")?;
let present = store.has_time_series(&key)?;
let resolutions = store.get_resolutions(Some(TimeSeriesType::SingleTimeSeries))?;
let counts = store.get_time_series_counts()?;
```

### The low-level read path

To read values without reconstructing a full `SingleTimeSeries` — for example when bridging to
another store that holds its own keys — resolve metadata and fetch the array by hash:

```rust
let meta = store.get_metadata(&key)?;
let array = store.get_array_by_hash(&meta.data_hash)?;
```

## Forecasts

Forecast types are written with `add_forecast`. `data` is a `TypedArray` in its native shape (the
[data model](../explanation/data-model.md#forecasts) lists the conventional shapes per type); the
store content-addresses it and records the windowing parameters:

```rust
use time_series_store_core::{TimeSeriesType, TypedArray};

// A Deterministic forecast: a (horizon_count, count) matrix.
let (horizon_count, count) = (24, 7);
let values: Vec<f64> = vec![0.0; horizon_count * count];   // row-major, shape (horizon_count, count)
let data = TypedArray::from_f64(vec![horizon_count, count], &values);

let key = store.add_forecast(
    "42", "Generator", OwnerCategory::Component, "load_forecast",
    TimeSeriesType::Deterministic,
    initial, Duration::hours(1),      // initial_timestamp, resolution
    Duration::hours(24),              // horizon
    Duration::hours(24),              // interval
    count,
    data, Features::new(),
    Some("MW".into()), None,          // units, scaling_factor_multiplier
    None,                             // percentiles (Some(vec) for Probabilistic)
    None,                             // logical_type
)?;
```

`get_time_series` reconstructs forecasts too, returning a `TimeSeriesData::Deterministic`,
`Probabilistic`, or `Scenarios` variant (a `DeterministicSingleTimeSeries` is synthesized into a
`Deterministic`). Match on the variant or use the `as_deterministic` / `as_probabilistic` /
`as_scenarios` accessors:

```rust
if let Some(d) = store.get_time_series(&key, None)?.as_deterministic() {
    // d.data is the TypedArray; d.horizon, d.interval, d.count carry the forecast parameters
}
```

The low-level path is still available when you only need the raw array: `get_metadata` exposes
`horizon`, `interval`, `count`, and `percentiles`, and `get_array_by_hash` returns the `TypedArray`
in its stored shape (`to_f64_vec` returns a `Result<_, String>`, so map the error if your function
returns `TimeSeriesError`):

```rust
let meta = store.get_metadata(&key)?;
let arr = store.get_array_by_hash(&meta.data_hash)?;   // arr.shape == [horizon_count, count]
let values = arr.to_f64_vec().map_err(TimeSeriesError::InvalidParameter)?;
```

## Remove and Maintain

```rust
store.remove_time_series(&key)?;       // one series
store.clear_time_series(Some("42"))?;  // all series for an owner
store.clear_time_series(None)?;        // everything

let report = store.compact()?;         // reports reusable slots
let integrity = store.verify_integrity()?;
assert!(integrity.errors.is_empty());
```

Removal is [reference-counted](../explanation/content-addressing.md#deletion-is-reference-counted):
a shared array survives until its last referencing key is gone.

## Persist to Disk

The NetCDF backend buffers writes. Call `flush` before copying the files for backup:

```rust
store.flush()?;   // nc_sync; afterwards system.nc + system.nc.sqlite can be copied as a pair
```

Always keep the `.nc` and `.nc.sqlite` files together — neither is usable alone.

## Error Handling

Every fallible method returns `Result<T, TimeSeriesError>`. Match on the variant to react:

```rust
use time_series_store_core::TimeSeriesError;

match store.get_time_series(&key, None) {
    Ok(data) => { /* ... */ }
    Err(TimeSeriesError::NotFound) => { /* missing */ }
    Err(TimeSeriesError::ReadOnlyStore) => unreachable!("this is a read"),
    Err(e) => return Err(e.into()),
}
```

## Threading

`Store` is `Send + Sync`-friendly: the NetCDF backend guards its handle with a `Mutex`. Share a
store across threads behind your own `Arc<Mutex<Store>>` if you need `&mut` write access from
several threads; concurrent reads are fine. The library does not coordinate multiple _processes_
writing the same files.
