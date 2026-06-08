# Quick Start (Rust)

This walkthrough creates an in-memory store, adds a `SingleTimeSeries`, and reads it back. It is the
shortest path to a working round-trip. For Python or Julia, see the
[Python](../how-to/integrate-python.md) and [Julia](../how-to/integrate-julia.md) how-to guides.

## A Minimal Round-Trip

```rust
use chrono::{Duration, TimeZone, Utc};
use time_series_store_core::{
    create_store, Features, OwnerCategory, SingleTimeSeries, TimeSeriesData, TypedArray,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `in_memory = true` means no filesystem I/O. Pass a path and `false`
    // to write a NetCDF file plus its SQLite sidecar.
    let mut store = create_store(None, true)?;

    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    // A TypedArray carries the element dtype and shape [length, ...].
    let values: Vec<f64> = (0..24).map(|i| 100.0 + i as f64).collect();
    let data = TypedArray::from_f64(vec![24], &values);
    let ts = SingleTimeSeries::new(initial, resolution, data);

    // The owner is identified by a UUID-like string plus a category. Features,
    // units, and a scaling expression are optional.
    let key = store.add_time_series(
        "42",                                       // owner_uuid
        "Generator",                                // owner_type
        OwnerCategory::Component,
        "load",                                     // name
        TimeSeriesData::SingleTimeSeries(ts),
        Features::new(),                            // no features
        Some("MW".into()),                          // units
        None,                                       // scaling_factor_multiplier
    )?;

    let got = store.get_time_series(&key, None)?;
    let single = got.as_single().unwrap();
    println!(
        "round-tripped {} values @ {} starting {}",
        single.length, single.resolution, single.initial_timestamp
    );
    Ok(())
}
```

This is the `examples/basic_rust.rs` program. Run it with:

```sh
cargo run --manifest-path crates/time-series-store-core/Cargo.toml --example basic
```

## What Just Happened

1. **`create_store(None, true)`** built a store backed by an in-memory array backend and an
   in-memory SQLite metadata database.
2. **`add_time_series`** hashed the array, wrote it to the backend (deduplicating on the hash), and
   recorded a metadata association keyed by `(owner_uuid, type, name, resolution, features)`. It
   returned a [`TimeSeriesKey`](../reference/rust-api.md#timeserieskey) that can re-find the series.
3. **`get_time_series(&key, None)`** looked up the association, read the array back by its content
   hash, and reconstructed a `SingleTimeSeries`. Passing `Some((start, end))` instead of `None`
   slices the series on the time axis.

## Writing to Disk

Swap the constructor to persist:

```rust
let mut store = create_store(Some(Path::new("system.nc")), false)?;
// ... add_time_series ...
store.flush()?; // sync buffered NetCDF writes to disk
```

This produces two files that travel together:

- `system.nc` — the NetCDF4 file holding the arrays.
- `system.nc.sqlite` — the sidecar holding the metadata associations.

Reopen them later with `open_store(Path::new("system.nc"), /* read_only */ true)`.

## Next Steps

- Understand the [Data Model](../explanation/data-model.md): owners, keys, and features.
- See exactly what lands on disk in the [On-Disk File Format](../reference/file-format.md).
- Browse the full [Rust API reference](../reference/rust-api.md).
