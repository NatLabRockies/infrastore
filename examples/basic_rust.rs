//! Minimal Rust example: in-memory store, add a series, read it back.
//!
//! Run with: `cargo run --manifest-path crates/time-series-store-core/Cargo.toml --example basic`
//! Or as a workspace target if exposed.

use chrono::{Duration, TimeZone, Utc};
use ndarray::ArrayD;
use time_series_store_core::{
    create_store, Features, OwnerCategory, SingleTimeSeries, TimeSeriesData,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut store = create_store(None, true)?;

    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = Duration::hours(1);
    let data: ArrayD<f64> = ArrayD::from_shape_vec(
        vec![24],
        (0..24).map(|i| 100.0 + i as f64).collect(),
    )?;
    let ts = SingleTimeSeries::new(initial, resolution, data);

    let key = store.add_time_series(
        42,
        "Generator",
        OwnerCategory::Component,
        "load",
        TimeSeriesData::SingleTimeSeries(ts),
        Features::new(),
        Some("MW".into()),
        None,
    )?;

    let got = store.get_time_series(&key, None)?;
    let single = got.as_single().unwrap();
    println!(
        "round-tripped {} values @ {} resolution starting {}",
        single.length, single.resolution, single.initial_timestamp
    );
    Ok(())
}
