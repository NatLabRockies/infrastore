//! Minimal Rust example: in-memory store, add a series, read it back.
//!
//! Run with: `cargo run --manifest-path crates/castore-core/Cargo.toml --example basic`
//! Or as a workspace target if exposed.

use castore_core::{
    Features, OwnerCategory, SingleTimeSeries, TimeSeriesData, TypedArray, create_store,
};
use chrono::{Duration, TimeZone, Utc};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut store = create_store(None, true)?;

    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let values: Vec<f64> = (0..24).map(|i| 100.0 + i as f64).collect();
    let data = TypedArray::from_f64(vec![24], &values);
    let ts = SingleTimeSeries::new(initial, resolution, data, "load");

    let key = store.add_time_series(
        42,
        "Generator",
        OwnerCategory::Component,
        TimeSeriesData::SingleTimeSeries(ts),
        Features::new(),
        Some("MW".into()),
    )?;

    let got = store.get_time_series(key.identity(), None)?;
    let single = got.as_single().unwrap();
    println!(
        "round-tripped {} values @ {} resolution starting {}",
        single.length, single.resolution, single.initial_timestamp
    );
    Ok(())
}
