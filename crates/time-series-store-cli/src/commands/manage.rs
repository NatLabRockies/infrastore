//! Write-side maintenance commands: `remove`, `transform`, and `template`.

use std::io::{IsTerminal, Write};
use std::path::Path;

use crate::color;
use crate::parse;
use crate::select::SelectorArgs;
use crate::store_access;

/// `remove`: delete a single series, confirming first when interactive.
pub fn remove(store_path: &Path, selector: &SelectorArgs, force: bool) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let (meta, key) = selector.resolve(&store)?;
    drop(store);

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

/// `transform`: derive DeterministicSingleTimeSeries from stored SingleTimeSeries.
pub fn transform(store_path: &Path, horizon: &str, interval: &str) -> Result<(), String> {
    let horizon = parse::parse_duration(horizon)?;
    let interval = parse::parse_duration(interval)?;
    let mut store = store_access::open_writable(store_path)?;
    let n = store
        .transform_single_time_series(horizon, interval, None, None)
        .map_err(|e| e.to_string())?;
    store.flush().map_err(|e| e.to_string())?;
    println!(
        "{}",
        color::header(&format!(
            "Transformed {n} SingleTimeSeries into DeterministicSingleTimeSeries."
        ))
    );
    Ok(())
}

/// `template`: print an example descriptor for the given time-series type.
pub fn template(ts_type: &str) -> Result<(), String> {
    let kind = parse::parse_ts_type(ts_type)?;
    use time_series_store_core::TimeSeriesType::*;
    let body = match kind {
        SingleTimeSeries => SINGLE,
        NonSequentialTimeSeries => NON_SEQUENTIAL,
        Deterministic => DETERMINISTIC,
        Probabilistic => PROBABILISTIC,
        Scenarios => SCENARIOS,
        DeterministicSingleTimeSeries => {
            return Err(
                "DeterministicSingleTimeSeries is derived via `tss transform`, not a descriptor"
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
  "dtype": "f64",
  "units": "MW",
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
  "dtype": "f64",
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
  "dtype": "f64",
  "units": "MW",
  "csv": "forecast.csv",
  "has_header": false,
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
  "dtype": "f64",
  "units": "MW",
  "csv": "prob.csv",
  "has_header": false,
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
  "dtype": "f64",
  "units": "MW",
  "csv": "scenarios.csv",
  "has_header": false,
  "initial_timestamp": "2024-01-01T00:00:00Z",
  "resolution": "1h",
  "horizon": "24h",
  "interval": "1h",
  "count": 7,
  "scenario_count": 10
}
"#;
