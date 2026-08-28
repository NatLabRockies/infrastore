//! Standalone benchmark binary for infrastore.
//!
//! Two scenarios:
//!
//! - `add`: measures `add_time_series_bulk` for `SingleTimeSeries` and
//!   `Deterministic`, stressing HDF5 packing (packed columns per dataset)
//!   and SQLite transaction throughput.
//!
//! - `read`: simulates the per-timestep simulation I/O pattern — for each
//!   step t, read all N components at t — and reports per-step timing
//!   statistics.
//!
//! Usage:
//!   infrastore-bench add  [--count N] [--length L] [--in-memory] [--path DIR]
//!   infrastore-bench read [--count N] [--length L] [--steps T]  [--in-memory] [--path DIR]
//!   infrastore-bench all  [--count N] [--length L] [--steps T]  [--in-memory] [--path DIR]

use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant};

use chrono::{TimeZone, Utc};
use clap::{Args, Parser, Subcommand};
use infrastore_core::{
    AddRequest, Deterministic, Features, KeyIdentity, OwnerCategory, SingleTimeSeries, Store,
    TimeSeriesData, TimeSeriesType, TypedArray,
};

// Deterministic forecast horizon, in hours.
const DET_HORIZON_H: usize = 24;
// Milliseconds per hour.
const HOUR_MS: i64 = 3_600_000;

type Error = Box<dyn std::error::Error>;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "infrastore-bench",
    about = "infrastore performance benchmarks",
    long_about = "Benchmarks bulk add (HDF5 packing + SQLite transactions) and \
                  per-timestep simulation reads for SingleTimeSeries and Deterministic."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Log filter directive (also read from RUST_LOG); defaults to `warn`.
    ///
    /// Examples: `debug`, `infrastore_bench=debug`, `infrastore_core=debug`.
    #[arg(long, env = "RUST_LOG", global = true)]
    log_level: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Benchmark bulk addition of time series.
    Add(AddArgs),
    /// Benchmark per-timestep simulation reads.
    Read(ReadArgs),
    /// Run add then read benchmarks back-to-back.
    All(AllArgs),
}

#[derive(Args, Clone)]
struct CommonArgs {
    /// Number of components (time series).
    #[arg(long, default_value_t = 1_000)]
    count: usize,

    /// Timesteps per SingleTimeSeries; window count for Deterministic.
    #[arg(long, default_value_t = 168)]
    length: usize,

    /// Use an in-memory store (no disk I/O).
    #[arg(long)]
    in_memory: bool,

    /// Directory for on-disk store files. Defaults to a system temp dir.
    #[arg(long)]
    path: Option<PathBuf>,
}

#[derive(Args, Clone)]
struct AddArgs {
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args, Clone)]
struct ReadArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Simulation steps to benchmark (default: --length).
    #[arg(long)]
    steps: Option<usize>,
}

#[derive(Args, Clone)]
struct AllArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Simulation steps to benchmark (default: --length).
    #[arg(long)]
    steps: Option<usize>,
}

fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    init_tracing(cli.log_level.as_deref());
    match cli.command {
        Command::Add(args) => run_add(&args)?,
        Command::Read(args) => run_read(&args)?,
        Command::All(AllArgs { common, steps }) => {
            run_add(&AddArgs {
                common: common.clone(),
            })?;
            println!();
            run_read(&ReadArgs { common, steps })?;
        }
    }
    Ok(())
}

// ── Store helpers ─────────────────────────────────────────────────────────────

struct StoreHandle {
    store: Store,
    /// Kept alive to prevent the temp dir from being deleted.
    _tmp: Option<tempfile::TempDir>,
    store_path: Option<PathBuf>,
}

fn create_store(common: &CommonArgs, suffix: &str) -> Result<StoreHandle, Error> {
    if common.in_memory {
        return Ok(StoreHandle {
            store: Store::create(None, true)?,
            _tmp: None,
            store_path: None,
        });
    }
    let (store_path, tmp) = if let Some(ref base) = common.path {
        std::fs::create_dir_all(base)?;
        (base.join(format!("bench_{suffix}.h5")), None)
    } else {
        let tmp = tempfile::tempdir()?;
        let store_path = tmp.path().join(format!("bench_{suffix}.h5"));
        (store_path, Some(tmp))
    };
    Ok(StoreHandle {
        store: Store::create(Some(&store_path), false)?,
        _tmp: tmp,
        store_path: Some(store_path),
    })
}

/// Flush writes, drop the store, and reopen it read-only from disk.
///
/// This forces a cold-ish read path: the store index is rebuilt from the file
/// and the HDF5 chunk cache starts empty. (The OS page cache may still be warm,
/// but that reflects real simulation startup conditions.)
fn flush_and_reopen(handle: StoreHandle) -> Result<StoreHandle, Error> {
    let StoreHandle {
        mut store,
        _tmp,
        store_path,
    } = handle;
    store.flush()?;
    drop(store);
    let path = store_path
        .as_deref()
        .ok_or("cannot reopen an in-memory store")?;
    let store = Store::open(path, true)?;
    Ok(StoreHandle {
        store,
        _tmp,
        store_path,
    })
}

// ── Request / key builders ────────────────────────────────────────────────────

fn make_sts_requests(count: usize, length: usize) -> Vec<AddRequest> {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = chrono::Duration::hours(1);
    (0..count)
        .map(|i| {
            // Deterministic ramp so construction is fast and compressible.
            let values: Vec<f64> = (0..length).map(|t| i as f64 * 1000.0 + t as f64).collect();
            let data = TypedArray::from_f64(vec![length], &values);
            AddRequest {
                owner_id: i as i64,
                owner_type: "Generator".to_string(),
                owner_category: OwnerCategory::Component,
                data: TimeSeriesData::SingleTimeSeries(
                    SingleTimeSeries::new(initial, resolution, data, "active_power")
                        .with_units("MW"),
                ),
                features: Features::default(),
            }
        })
        .collect()
}

fn make_det_requests(count: usize, length: usize) -> Vec<AddRequest> {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let resolution = chrono::Duration::hours(1);
    let horizon = chrono::Duration::hours(DET_HORIZON_H as i64);
    let interval = chrono::Duration::hours(1);
    (0..count)
        .map(|i| {
            let values: Vec<f64> = (0..DET_HORIZON_H * length)
                .map(|j| i as f64 * 1000.0 + j as f64)
                .collect();
            let data = TypedArray::from_f64(vec![DET_HORIZON_H, length], &values);
            let det = Deterministic::new(
                initial,
                resolution,
                horizon,
                interval,
                length,
                data,
                "active_power_forecast",
            )
            .expect("valid Deterministic shape")
            .with_units("MW");
            AddRequest {
                owner_id: i as i64,
                owner_type: "Generator".to_string(),
                owner_category: OwnerCategory::Component,
                data: TimeSeriesData::Deterministic(det),
                features: Features::default(),
            }
        })
        .collect()
}

/// Reconstruct TimeSeriesKeys for SingleTimeSeries without querying the store.
///
/// This works because the bench creates deterministic owner_ids and names.
fn sts_keys(count: usize) -> Vec<KeyIdentity> {
    (0..count)
        .map(|i| KeyIdentity {
            owner_id: i as i64,
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::SingleTimeSeries,
            name: "active_power".to_string(),
            resolution: Some(infrastore_core::Period::Fixed(chrono::Duration::hours(1))),
            interval: None,
            features: Features::default(),
        })
        .collect()
}

/// Reconstruct TimeSeriesKeys for Deterministic without querying the store.
fn det_keys(count: usize) -> Vec<KeyIdentity> {
    (0..count)
        .map(|i| KeyIdentity {
            owner_id: i as i64,
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::Deterministic,
            name: "active_power_forecast".to_string(),
            resolution: Some(infrastore_core::Period::Fixed(chrono::Duration::hours(1))),
            interval: Some(infrastore_core::Period::Fixed(chrono::Duration::hours(1))),
            features: Features::default(),
        })
        .collect()
}

// ── Output formatting ─────────────────────────────────────────────────────────

fn fmt_dur(d: StdDuration) -> String {
    if d.as_secs() >= 60 {
        format!("{:.2}min", d.as_secs_f64() / 60.0)
    } else if d.as_secs() >= 1 {
        format!("{:.2}s", d.as_secs_f64())
    } else if d.as_millis() >= 1 {
        format!("{:.2}ms", d.as_secs_f64() * 1_000.0)
    } else if d.as_micros() >= 1 {
        format!("{:.1}µs", d.as_secs_f64() * 1_000_000.0)
    } else {
        format!("{}ns", d.as_nanos())
    }
}

fn fmt_bytes(n: usize) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2} GB", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.2} MB", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1} KB", n as f64 / 1_000.0)
    } else {
        format!("{n} B")
    }
}

fn fmt_throughput(items: usize, elapsed: StdDuration) -> String {
    if elapsed.as_secs_f64() == 0.0 {
        return "—".to_string();
    }
    let rate = items as f64 / elapsed.as_secs_f64();
    if rate >= 1_000_000.0 {
        format!("{:.1}M/s", rate / 1_000_000.0)
    } else if rate >= 1_000.0 {
        format!("{:.1}k/s", rate / 1_000.0)
    } else {
        format!("{rate:.0}/s")
    }
}

fn fmt_bw(bytes: usize, elapsed: StdDuration) -> String {
    if elapsed.as_secs_f64() == 0.0 {
        return "—".to_string();
    }
    let rate = bytes as f64 / elapsed.as_secs_f64();
    fmt_bytes(rate as usize) + "/s"
}

fn sep() {
    println!("{}", "-".repeat(72));
}

// ── Add benchmark ─────────────────────────────────────────────────────────────

fn run_add(args: &AddArgs) -> Result<(), Error> {
    let c = &args.common;

    sep();
    println!("Bulk add: SingleTimeSeries");
    println!(
        "  count={}, length={} timesteps (1h resolution), storage={}",
        c.count,
        c.length,
        storage_label(c),
    );
    let sts_bytes = c.count * c.length * 8; // f64
    println!("  raw array data: {}", fmt_bytes(sts_bytes));

    let t = Instant::now();
    let sts_reqs = make_sts_requests(c.count, c.length);
    let t_build = t.elapsed();

    let mut handle = create_store(c, "add_sts")?;
    let t = Instant::now();
    let _ = handle.store.add_time_series_bulk(sts_reqs)?;
    let t_add = t.elapsed();

    println!();
    println!("  build requests:     {}", fmt_dur(t_build));
    println!(
        "  add_time_series_bulk: {}   ({}, {})",
        fmt_dur(t_add),
        fmt_throughput(c.count, t_add),
        fmt_bw(sts_bytes, t_add),
    );

    sep();
    println!("Bulk add: Deterministic");
    println!(
        "  count={}, length={} windows (horizon={}h, interval=1h), storage={}",
        c.count,
        c.length,
        DET_HORIZON_H,
        storage_label(c),
    );
    let det_bytes = c.count * DET_HORIZON_H * c.length * 8;
    println!("  raw array data: {}", fmt_bytes(det_bytes));

    let t = Instant::now();
    let det_reqs = make_det_requests(c.count, c.length);
    let t_build = t.elapsed();

    let mut handle = create_store(c, "add_det")?;
    let t = Instant::now();
    let _ = handle.store.add_time_series_bulk(det_reqs)?;
    let t_add = t.elapsed();

    println!();
    println!("  build requests:     {}", fmt_dur(t_build));
    println!(
        "  add_time_series_bulk: {}   ({}, {})",
        fmt_dur(t_add),
        fmt_throughput(c.count, t_add),
        fmt_bw(det_bytes, t_add),
    );
    sep();

    Ok(())
}

// ── Read benchmark ────────────────────────────────────────────────────────────

fn run_read(args: &ReadArgs) -> Result<(), Error> {
    let c = &args.common;
    let steps = args.steps.unwrap_or(c.length);
    let actual_steps = steps.min(c.length);

    // ── SingleTimeSeries ──────────────────────────────────────────────────────
    sep();
    println!("Simulation read: SingleTimeSeries");
    println!(
        "  components={}, length={}, steps={}, storage={}",
        c.count,
        c.length,
        actual_steps,
        storage_label(c),
    );
    println!("  total get_time_series calls: {}", c.count * actual_steps);
    if !c.in_memory {
        println!("  (store reopened read-only between write and read phases)");
    }

    let sts_reqs = make_sts_requests(c.count, c.length);
    let mut handle = create_store(c, "read_sts")?;
    let _ = handle.store.add_time_series_bulk(sts_reqs)?;
    let handle = if c.in_memory {
        handle.store.flush()?;
        handle
    } else {
        flush_and_reopen(handle)?
    };

    let keys = sts_keys(c.count);
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

    let mut step_times: Vec<StdDuration> = Vec::with_capacity(actual_steps);
    for step in 0..actual_steps {
        let t_start = initial + chrono::Duration::milliseconds(step as i64 * HOUR_MS);
        let t_end = initial + chrono::Duration::milliseconds((step + 1) as i64 * HOUR_MS);
        let t0 = Instant::now();
        for key in &keys {
            let _ = handle
                .store
                .get_time_series(key, Some((t_start, t_end).into()))?;
        }
        step_times.push(t0.elapsed());
    }

    println!();
    print_step_stats(&step_times, c.count);

    // ── Deterministic ─────────────────────────────────────────────────────────
    sep();
    println!("Simulation read: Deterministic");
    println!(
        "  components={}, length={}, steps={}, storage={}",
        c.count,
        c.length,
        actual_steps,
        storage_label(c),
    );
    println!("  total get_time_series calls: {}", c.count * actual_steps);
    println!(
        "  note: each call fetches the full [{}×{}] array from storage, then slices to 1 window",
        DET_HORIZON_H, c.length,
    );
    if !c.in_memory {
        println!("  (store reopened read-only between write and read phases)");
    }

    let det_reqs = make_det_requests(c.count, c.length);
    let mut handle = create_store(c, "read_det")?;
    let _ = handle.store.add_time_series_bulk(det_reqs)?;
    let handle = if c.in_memory {
        handle.store.flush()?;
        handle
    } else {
        flush_and_reopen(handle)?
    };

    let keys = det_keys(c.count);

    let mut step_times: Vec<StdDuration> = Vec::with_capacity(actual_steps);
    for step in 0..actual_steps {
        let t_start = initial + chrono::Duration::milliseconds(step as i64 * HOUR_MS);
        let t_end = initial + chrono::Duration::milliseconds((step + 1) as i64 * HOUR_MS);
        let t0 = Instant::now();
        for key in &keys {
            let _ = handle
                .store
                .get_time_series(key, Some((t_start, t_end).into()))?;
        }
        step_times.push(t0.elapsed());
    }

    println!();
    print_step_stats(&step_times, c.count);
    sep();

    Ok(())
}

fn print_step_stats(step_times: &[StdDuration], count: usize) {
    if step_times.is_empty() {
        println!("  (no steps measured)");
        return;
    }
    let total: StdDuration = step_times.iter().copied().sum();
    let mut sorted = step_times.to_vec();
    sorted.sort();

    let n = sorted.len();
    let median = sorted[(n - 1) / 2];
    let p95 = sorted[(n * 95 / 100).min(n - 1)];
    let min = sorted[0];
    let max = *sorted.last().unwrap();

    let total_reads = n * count;

    println!(
        "  step time:  min={}  median={}  p95={}  max={}",
        fmt_dur(min),
        fmt_dur(median),
        fmt_dur(p95),
        fmt_dur(max),
    );
    println!("  total:      {} ({} steps)", fmt_dur(total), n);
    println!(
        "  throughput: {} component-reads",
        fmt_throughput(total_reads, total),
    );
}

fn storage_label(c: &CommonArgs) -> &'static str {
    if c.in_memory { "in-memory" } else { "on-disk" }
}

fn init_tracing(level: Option<&str>) {
    use tracing_subscriber::EnvFilter;
    let filter = match level {
        Some(l) => EnvFilter::new(l),
        None => EnvFilter::new("warn"),
    };
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
