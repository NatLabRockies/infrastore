//! Read-side inspection commands: `stats`, `summary`, `verify`,
//! `check-consistency`, `resolutions`, and `params`.

use std::path::Path;

use castore_core::Period;
use serde_json::{Value, json};

use crate::color;
use crate::output::{self, Format};
use crate::parse;
use crate::store_access;

/// Render a list of `(label, value)` rows in the selected format. The JSON form
/// is a single object mapping label -> value.
fn render_kv(title: &str, pairs: Vec<(String, Value)>, format: Format) -> Result<(), String> {
    match format {
        Format::Json => {
            let obj: serde_json::Map<String, Value> = pairs.into_iter().collect();
            output::print_json(&Value::Object(obj))
        }
        Format::Csv => {
            let headers = vec!["Metric".to_string(), "Value".to_string()];
            let rows: Vec<Vec<String>> = pairs
                .iter()
                .map(|(k, v)| vec![k.clone(), value_to_cell(v)])
                .collect();
            output::display_csv_rows(&headers, &rows)
        }
        Format::Table => {
            println!("{}", color::header(title));
            let headers = vec!["Metric".to_string(), "Value".to_string()];
            let rows: Vec<Vec<String>> = pairs
                .iter()
                .map(|(k, v)| vec![k.clone(), value_to_cell(v)])
                .collect();
            output::display_table_dyn(&headers, &rows);
            Ok(())
        }
    }
}

fn value_to_cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

/// `stats`: overall counts, detailed counts, per-type counts, distinct arrays.
pub fn stats(store_path: &Path, format: Format) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let counts = store.get_time_series_counts().map_err(|e| e.to_string())?;
    let detailed = store
        .time_series_counts_detailed()
        .map_err(|e| e.to_string())?;
    let by_type = store.counts_by_type().map_err(|e| e.to_string())?;
    let distinct = store.num_distinct_arrays().map_err(|e| e.to_string())?;

    let mut pairs = vec![
        (
            "components_with_time_series".into(),
            json!(counts.components_with_time_series),
        ),
        (
            "static_time_series".into(),
            json!(counts.static_time_series),
        ),
        ("forecasts".into(), json!(counts.forecasts)),
        (
            "components_with_time_series (detailed)".into(),
            json!(detailed.components_with_time_series),
        ),
        (
            "supplemental_attributes_with_time_series".into(),
            json!(detailed.supplemental_attributes_with_time_series),
        ),
        (
            "static_time_series_count".into(),
            json!(detailed.static_time_series_count),
        ),
        ("forecast_count".into(), json!(detailed.forecast_count)),
        ("num_distinct_arrays".into(), json!(distinct)),
    ];
    for (t, n) in by_type {
        pairs.push((format!("count[{}]", t.as_str()), json!(n)));
    }
    render_kv("Store statistics", pairs, format)
}

/// `verify`: run integrity verification; nonzero exit when errors are present.
pub fn verify(store_path: &Path, format: Format) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let report = store.verify_integrity().map_err(|e| e.to_string())?;
    let headers = vec!["Error".to_string()];
    let rows: Vec<Vec<String>> = report.errors.iter().map(|e| vec![e.clone()]).collect();
    match format {
        Format::Json => output::print_json(&json!({ "errors": report.errors }))?,
        Format::Csv => output::display_csv_rows(&headers, &rows)?,
        Format::Table => {
            if report.errors.is_empty() {
                // Scoped deliberately: this command checks stored arrays against
                // their recorded hashes and does not inspect the SQLite catalog.
                println!("{}", color::header("Array integrity OK (no errors)."));
            } else {
                println!("{}", color::header("Integrity errors:"));
                output::display_table_dyn(&headers, &rows);
            }
        }
    }
    if !report.errors.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// `check-consistency`: verify per-resolution static grid consistency.
pub fn check_consistency(
    store_path: &Path,
    resolution: Option<&str>,
    format: Format,
) -> Result<(), String> {
    let resolution = resolution.map(parse::parse_period).transpose()?;
    let store = store_access::open_readonly(store_path)?;
    let rows = store
        .check_static_consistency(resolution)
        .map_err(|e| e.to_string())?;
    let headers = vec![
        "Resolution".to_string(),
        "Initial Timestamp".to_string(),
        "Length".to_string(),
    ];
    let table_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|c| {
            vec![
                parse::format_period(c.resolution),
                c.initial_timestamp.to_rfc3339(),
                c.length.to_string(),
            ]
        })
        .collect();
    match format {
        Format::Json => {
            let items: Vec<Value> = rows
                .iter()
                .map(|c| {
                    json!({
                        "resolution": c.resolution.to_iso8601(),
                        "initial_timestamp": c.initial_timestamp.to_rfc3339(),
                        "length": c.length,
                    })
                })
                .collect();
            output::print_json_wrapped(&items)?;
        }
        Format::Csv => output::display_csv_rows(&headers, &table_rows)?,
        Format::Table => output::display_table_dyn(&headers, &table_rows),
    }
    Ok(())
}

/// `resolutions`: distinct resolutions and forecast intervals in the store.
pub fn resolutions(store_path: &Path, format: Format) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let res = store.get_resolutions(None).map_err(|e| e.to_string())?;
    let intervals = store.get_intervals(None).map_err(|e| e.to_string())?;
    let iso = |v: &[Period]| v.iter().map(|p| p.to_iso8601()).collect::<Vec<_>>();
    match format {
        Format::Json => output::print_json(&json!({
            "resolutions": iso(&res),
            "intervals": iso(&intervals),
        }))?,
        _ => {
            let headers = vec!["Kind".to_string(), "Value".to_string()];
            let mut rows: Vec<Vec<String>> = res
                .iter()
                .map(|p| vec!["resolution".to_string(), parse::format_period(*p)])
                .collect();
            rows.extend(
                intervals
                    .iter()
                    .map(|p| vec!["interval".to_string(), parse::format_period(*p)]),
            );
            if format == Format::Csv {
                output::display_csv_rows(&headers, &rows)?;
            } else {
                output::display_table_dyn(&headers, &rows);
            }
        }
    }
    Ok(())
}

/// `params`: the store's forecast parameters, optionally filtered.
pub fn params(
    store_path: &Path,
    resolution: Option<&str>,
    interval: Option<&str>,
    format: Format,
) -> Result<(), String> {
    let resolution = resolution.map(parse::parse_period).transpose()?;
    let interval = interval.map(parse::parse_period).transpose()?;
    let store = store_access::open_readonly(store_path)?;
    let p = store
        .get_forecast_parameters(resolution, interval)
        .map_err(|e| e.to_string())?;
    let iso = |v: Option<Period>| v.map(|p| p.to_iso8601());
    let pairs = vec![
        ("horizon".into(), json!(iso(p.horizon))),
        ("interval".into(), json!(iso(p.interval))),
        ("count".into(), json!(p.count)),
        ("resolution".into(), json!(iso(p.resolution))),
        (
            "initial_timestamp".into(),
            json!(p.initial_timestamp.map(|t| t.to_rfc3339())),
        ),
    ];
    render_kv("Forecast parameters", pairs, format)
}

/// `summary`: grouped static and/or forecast summaries.
pub fn summary(
    store_path: &Path,
    static_only: bool,
    forecast_only: bool,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    // Default (neither flag) shows both.
    let show_static = static_only || !forecast_only;
    let show_forecast = forecast_only || !static_only;

    let mut static_items: Vec<Value> = Vec::new();
    if show_static {
        for r in store.static_summary().map_err(|e| e.to_string())? {
            static_items.push(json!({
                "owner_type": r.owner_type,
                "owner_category": r.owner_category.as_str(),
                "time_series_type": r.time_series_type.as_str(),
                "name": r.name,
                "initial_timestamp": r.initial_timestamp.map(|t| t.to_rfc3339()),
                "resolution": r.resolution.map(|p| p.to_iso8601()),
                "time_step_count": r.time_step_count,
                "count": r.count,
            }));
        }
    }
    let mut forecast_items: Vec<Value> = Vec::new();
    if show_forecast {
        for r in store.forecast_summary().map_err(|e| e.to_string())? {
            forecast_items.push(json!({
                "owner_type": r.owner_type,
                "owner_category": r.owner_category.as_str(),
                "time_series_type": r.time_series_type.as_str(),
                "name": r.name,
                "initial_timestamp": r.initial_timestamp.map(|t| t.to_rfc3339()),
                "resolution": r.resolution.map(|p| p.to_iso8601()),
                "horizon": r.horizon.map(|p| p.to_iso8601()),
                "interval": r.interval.map(|p| p.to_iso8601()),
                "window_count": r.window_count,
                "count": r.count,
            }));
        }
    }

    match format {
        Format::Json => output::print_json(&json!({
            "static": static_items,
            "forecast": forecast_items,
        }))?,
        _ => {
            if show_static {
                println!("{}", color::header("Static series"));
                let headers: Vec<String> = ["Owner Type", "Type", "Name", "Resolution", "Count"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                let rows: Vec<Vec<String>> = static_items
                    .iter()
                    .map(|v| {
                        vec![
                            json_str(v, "owner_type"),
                            json_str(v, "time_series_type"),
                            json_str(v, "name"),
                            json_str(v, "resolution"),
                            json_str(v, "count"),
                        ]
                    })
                    .collect();
                if format == Format::Csv {
                    output::display_csv_rows(&headers, &rows)?;
                } else {
                    output::display_table_dyn(&headers, &rows);
                }
            }
            if show_forecast {
                println!("{}", color::header("Forecast series"));
                let headers: Vec<String> =
                    ["Owner Type", "Type", "Name", "Horizon", "Interval", "Count"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                let rows: Vec<Vec<String>> = forecast_items
                    .iter()
                    .map(|v| {
                        vec![
                            json_str(v, "owner_type"),
                            json_str(v, "time_series_type"),
                            json_str(v, "name"),
                            json_str(v, "horizon"),
                            json_str(v, "interval"),
                            json_str(v, "count"),
                        ]
                    })
                    .collect();
                if format == Format::Csv {
                    output::display_csv_rows(&headers, &rows)?;
                } else {
                    output::display_table_dyn(&headers, &rows);
                }
            }
        }
    }
    Ok(())
}

fn json_str(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "-".to_string(),
        Some(other) => other.to_string(),
    }
}
