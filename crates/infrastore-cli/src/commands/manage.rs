//! Write-side maintenance commands: `remove`, `transform`, and `template`.

use std::io::{IsTerminal, Write};
use std::path::Path;

use crate::color;
use crate::parse;
use crate::select::SelectorArgs;
use crate::store_access;

/// `remove`: delete a single series, confirming first when interactive.
pub fn remove(
    store_path: &Path,
    selector: &SelectorArgs,
    force: bool,
    dry_run: bool,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let (meta, key) = selector.resolve(&store)?;
    drop(store);

    if dry_run {
        println!(
            "Would remove {} '{}' (owner {}).",
            meta.time_series_type.as_str(),
            meta.name,
            meta.owner_id
        );
        return Ok(());
    }

    if !force && std::io::stdin().is_terminal() {
        print!(
            "Remove {} '{}' (owner {})? [y/N] ",
            meta.time_series_type.as_str(),
            meta.name,
            meta.owner_id
        );
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("{}", color::dim("Aborted."));
            return Ok(());
        }
    }

    let mut store = store_access::open_writable(store_path)?;
    store.remove_time_series(&key).map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    println!(
        "{}",
        color::header(&format!(
            "Removed '{}' (owner {}).",
            meta.name, meta.owner_id
        ))
    );
    Ok(())
}

/// `transform`: derive DeterministicSingleTimeSeries from stored SingleTimeSeries,
/// optionally scoped to an owner category and/or resolution.
pub fn transform(
    store_path: &Path,
    horizon: &str,
    interval: &str,
    owner_category: Option<&str>,
    resolution: Option<&str>,
) -> Result<(), String> {
    let horizon = parse::parse_period(horizon)?;
    let interval = parse::parse_period(interval)?;
    let owner_category = owner_category
        .map(parse::parse_owner_category)
        .transpose()?;
    let resolution = resolution.map(parse::parse_period).transpose()?;
    let mut store = store_access::open_writable(store_path)?;
    let n = store
        .transform_single_time_series(
            horizon,
            interval,
            owner_category,
            resolution,
            Default::default(),
        )
        .map_err(|e| e.to_string())?
        .transformed;
    store.flush().map_err(|e| e.to_string())?;
    println!(
        "{}",
        color::header(&format!(
            "Transformed {n} SingleTimeSeries into DeterministicSingleTimeSeries."
        ))
    );
    Ok(())
}

/// `rename`: rename the single series a selector resolves to.
pub fn rename(
    store_path: &Path,
    selector: &SelectorArgs,
    new_name: &str,
    dry_run: bool,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let (meta, key) = selector.resolve(&store)?;
    drop(store);
    if dry_run {
        println!(
            "Would rename '{}' (owner {}) to '{new_name}'.",
            meta.name, meta.owner_id
        );
        return Ok(());
    }
    let mut store = store_access::open_writable(store_path)?;
    store
        .rename_time_series(&key, new_name)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    println!(
        "{}",
        color::header(&format!(
            "Renamed '{}' (owner {}) to '{new_name}'.",
            meta.name, meta.owner_id
        ))
    );
    Ok(())
}

/// `remove --all`: remove every series matching the selector (may be several).
pub fn remove_all(
    store_path: &Path,
    selector: &SelectorArgs,
    force: bool,
    dry_run: bool,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let filter = selector.to_filter()?;
    let matches = store
        .list_time_series(filter.clone())
        .map_err(|e| e.to_string())?;
    drop(store);
    if matches.is_empty() {
        println!("{}", color::dim("No time series matched the selector."));
        return Ok(());
    }
    if dry_run {
        println!("Would remove {} time series:", matches.len());
        for m in &matches {
            println!(
                "  - owner={} type={} name={}",
                m.owner_id,
                m.time_series_type.as_str(),
                m.name
            );
        }
        return Ok(());
    }
    if !force && std::io::stdin().is_terminal() {
        print!(
            "Remove {} time series matching the selector? [y/N] ",
            matches.len()
        );
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("{}", color::dim("Aborted."));
            return Ok(());
        }
    }
    let mut store = store_access::open_writable(store_path)?;
    let n = store.remove_by_filter(filter).map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    println!("{}", color::header(&format!("Removed {n} time series.")));
    Ok(())
}

/// `clear`: remove all series, or all for one owner, confirming when interactive.
pub fn clear(
    store_path: &Path,
    owner_id: Option<i64>,
    owner_category: Option<&str>,
    force: bool,
    dry_run: bool,
) -> Result<(), String> {
    let owner = match (owner_id, owner_category) {
        (Some(id), Some(cat)) => Some((id, parse::parse_owner_category(cat)?)),
        (None, None) => None,
        _ => {
            return Err("clear requires both --owner-id and --owner-category, or neither".into());
        }
    };
    if dry_run {
        let store = store_access::open_readonly(store_path)?;
        let mut filter = infrastore_core::ListFilter::new();
        if let Some((id, cat)) = owner {
            filter = filter.owner_id(id).owner_category(cat);
        }
        let n = store.list_keys(filter).map_err(|e| e.to_string())?.len();
        println!("Would clear {n} time series.");
        return Ok(());
    }
    if !force && std::io::stdin().is_terminal() {
        let scope = match owner {
            Some((id, _)) => format!("owner {id}"),
            None => "the entire store".to_string(),
        };
        print!("Clear all time series for {scope}? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("{}", color::dim("Aborted."));
            return Ok(());
        }
    }
    let mut store = store_access::open_writable(store_path)?;
    let n = store.clear_time_series(owner).map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    println!("{}", color::header(&format!("Cleared {n} time series.")));
    Ok(())
}

/// `replace-owner`: reassign every series from one owner to another.
pub fn replace_owner(
    store_path: &Path,
    old: i64,
    new: i64,
    owner_category: &str,
    dry_run: bool,
) -> Result<(), String> {
    let category = parse::parse_owner_category(owner_category)?;
    if dry_run {
        let store = store_access::open_readonly(store_path)?;
        let filter = infrastore_core::ListFilter::new()
            .owner_id(old)
            .owner_category(category);
        let n = store.list_keys(filter).map_err(|e| e.to_string())?.len();
        println!("Would reassign {n} time series from owner {old} to {new}.");
        return Ok(());
    }
    let mut store = store_access::open_writable(store_path)?;
    let n = store
        .replace_owner(old, new, category)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    println!(
        "{}",
        color::header(&format!(
            "Reassigned {n} time series from owner {old} to {new}."
        ))
    );
    Ok(())
}

/// `copy`: copy the single series a selector resolves to onto another owner.
pub fn copy(
    store_path: &Path,
    selector: &SelectorArgs,
    dst_owner_id: i64,
    dst_owner_type: &str,
    new_name: Option<&str>,
    dry_run: bool,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let (meta, key) = selector.resolve(&store)?;
    drop(store);
    if dry_run {
        println!(
            "Would copy '{}' (owner {}) to owner {dst_owner_id} ({dst_owner_type}) as '{}'.",
            meta.name,
            meta.owner_id,
            new_name.unwrap_or(&meta.name)
        );
        return Ok(());
    }
    let mut store = store_access::open_writable(store_path)?;
    store
        .copy_time_series(&key, dst_owner_id, dst_owner_type, new_name)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    println!(
        "{}",
        color::header(&format!(
            "Copied to owner {dst_owner_id} ({dst_owner_type})."
        ))
    );
    Ok(())
}

/// `persist`: write the store to a new HDF5 + SQLite artifact.
pub fn persist(store_path: &Path, dest: &Path) -> Result<(), String> {
    let mut store = store_access::open_writable(store_path)?;
    store.persist_to(dest).map_err(|e| e.to_string())?;
    println!(
        "{}",
        color::header(&format!("Persisted store to {}.", dest.display()))
    );
    Ok(())
}

/// `compact`: reclaim reusable space; print the compaction report. Confirms
/// first when interactive (it rewrites store internals); `--force` bypasses.
pub fn compact(
    store_path: &Path,
    force: bool,
    format: crate::output::Format,
) -> Result<(), String> {
    if !force && std::io::stdin().is_terminal() {
        print!("Compact the store (rewrites internal bookkeeping)? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("{}", color::dim("Aborted."));
            return Ok(());
        }
    }
    let mut store = store_access::open_writable(store_path)?;
    let report = store.compact().map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    match format {
        crate::output::Format::Json => crate::output::print_json(&serde_json::json!({
            "slots_reclaimed": report.slots_reclaimed,
            "datasets_dropped": report.datasets_dropped,
            "feature_sets_reclaimed": report.feature_sets_reclaimed,
            "timestamp_sets_reclaimed": report.timestamp_sets_reclaimed,
        }))?,
        _ => {
            let headers = vec!["Metric".to_string(), "Value".to_string()];
            let rows = vec![
                vec![
                    "slots_reclaimed".to_string(),
                    report.slots_reclaimed.to_string(),
                ],
                vec![
                    "datasets_dropped".to_string(),
                    report.datasets_dropped.to_string(),
                ],
                vec![
                    "feature_sets_reclaimed".to_string(),
                    report.feature_sets_reclaimed.to_string(),
                ],
                vec![
                    "timestamp_sets_reclaimed".to_string(),
                    report.timestamp_sets_reclaimed.to_string(),
                ],
            ];
            if format == crate::output::Format::Csv {
                crate::output::display_csv_rows(&headers, &rows)?;
            } else {
                crate::output::display_table_dyn(&headers, &rows);
            }
        }
    }
    Ok(())
}

/// `template`: print an example descriptor for the given time-series type.
pub fn template(ts_type: &str) -> Result<(), String> {
    let kind = parse::parse_ts_type(ts_type)?;
    use infrastore_core::TimeSeriesType::*;
    let body = match kind {
        SingleTimeSeries => SINGLE,
        NonSequentialTimeSeries => NON_SEQUENTIAL,
        Deterministic => DETERMINISTIC,
        Probabilistic => PROBABILISTIC,
        Scenarios => SCENARIOS,
        DeterministicSingleTimeSeries => {
            return Err(
                "DeterministicSingleTimeSeries is derived via `infrastore transform`, not a descriptor"
                    .to_string(),
            );
        }
    };
    print!("{body}");
    Ok(())
}

const SINGLE: &str = r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "component",
  "name": "load",
  "type": "single",
  "element_type": "f64",
  "units": "MW",
  "ext": "Profile",
  "csv": "load.csv",
  "has_header": true,
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "1h",
  "features": {
    "model_year": 2030
  }
}
"#;

const NON_SEQUENTIAL: &str = r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "component",
  "name": "events",
  "type": "non_sequential",
  "element_type": "f64",
  "units": "MW",
  "csv": "events.csv",
  "has_header": true
}
"#;

const DETERMINISTIC: &str = r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "component",
  "name": "load_forecast",
  "type": "deterministic",
  "element_type": "f64",
  "units": "MW",
  "csv": "forecast.csv",
  "has_header": true,
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "1h",
  "horizon": "24h",
  "interval": "1h",
  "count": 7
}
"#;

const PROBABILISTIC: &str = r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "component",
  "name": "load_prob",
  "type": "probabilistic",
  "element_type": "f64",
  "units": "MW",
  "csv": "prob.csv",
  "has_header": true,
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "1h",
  "horizon": "24h",
  "interval": "1h",
  "count": 7,
  "percentiles": [10.0, 50.0, 90.0]
}
"#;

const SCENARIOS: &str = r#"{
  "owner_id": 42,
  "owner_type": "Generator",
  "owner_category": "component",
  "name": "load_scenarios",
  "type": "scenarios",
  "element_type": "f64",
  "units": "MW",
  "csv": "scenarios.csv",
  "has_header": true,
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "1h",
  "horizon": "24h",
  "interval": "1h",
  "count": 7,
  "scenario_count": 10
}
"#;
