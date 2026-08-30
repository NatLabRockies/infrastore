//! Read-side inspection commands: `stats`, `summary`, `verify`,
//! `check-consistency`, `resolutions`, and `params`.

use std::collections::BTreeMap;
use std::path::Path;

use infrastore_core::Period;
use serde_json::{Value, json};

use crate::color;
use crate::fields;
use crate::output::{self, Format};
use crate::parse;
use crate::select::SelectorArgs;
use crate::store_access;

/// Render a list of `(label, value)` rows in the selected format. The JSON form
/// is a single object mapping label -> value.
fn render_kv(title: &str, pairs: Vec<(String, Value)>, format: Format) -> Result<(), String> {
    match format {
        f if f.is_json() => {
            let obj: serde_json::Map<String, Value> = pairs.into_iter().collect();
            output::print_value(f, &Value::Object(obj))
        }
        Format::Csv => {
            let headers = vec!["Metric".to_string(), "Value".to_string()];
            let rows: Vec<Vec<String>> = pairs
                .iter()
                .map(|(k, v)| vec![k.clone(), value_to_cell(v)])
                .collect();
            output::display_csv_rows(&headers, &rows)
        }
        _ => {
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
///
/// The labels below are deliberately explicit about *what is being counted*:
/// catalog rows (associations) and distinct stored arrays are different
/// quantities, and content addressing makes them diverge sharply — a store
/// where every series shares one array has thousands of associations and one
/// array.
pub fn stats(store_path: &Path, format: Format) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let counts = store.get_time_series_counts().map_err(|e| e.to_string())?;
    let detailed = store
        .time_series_counts_detailed()
        .map_err(|e| e.to_string())?;
    let by_type = store.counts_by_type().map_err(|e| e.to_string())?;
    let distinct = store.num_distinct_arrays().map_err(|e| e.to_string())?;

    let mut pairs = vec![
        // Associations: one per stored time series, i.e. catalog rows.
        (
            "associations.static".into(),
            json!(counts.static_time_series),
        ),
        ("associations.forecast".into(), json!(counts.forecasts)),
        (
            "associations.total".into(),
            json!(counts.static_time_series + counts.forecasts),
        ),
        // Owners: distinct ids that have at least one association.
        (
            "owners.components".into(),
            json!(detailed.components_with_time_series),
        ),
        (
            "owners.supplemental_attributes".into(),
            json!(detailed.supplemental_attributes_with_time_series),
        ),
        (
            "owners.total".into(),
            json!(counts.components_with_time_series),
        ),
        // Arrays: distinct content hashes. Always <= associations, and far
        // smaller wherever series share data.
        (
            "arrays.distinct_static".into(),
            json!(detailed.static_time_series_count),
        ),
        (
            "arrays.distinct_forecast".into(),
            json!(detailed.forecast_count),
        ),
        ("arrays.distinct_total".into(), json!(distinct)),
    ];
    for (t, n) in by_type {
        pairs.push((format!("associations.by_type[{}]", t.as_str()), json!(n)));
    }
    render_kv("Store statistics", pairs, format)
}

/// `arrays`: one row per distinct stored array, with the series that share it.
///
/// This is the inspection counterpart to `stats`' `arrays.distinct_total`: it
/// shows *which* series collapsed onto one array and where that array lives in
/// the HDF5 file, which is what makes a dataset with fewer columns than the
/// catalog has rows explicable rather than alarming.
pub fn arrays(
    store_path: &Path,
    selector: &SelectorArgs,
    data_hash: Option<&str>,
    format: Format,
) -> Result<(), String> {
    let wanted = data_hash.map(parse::parse_hash_prefix).transpose()?;
    let store = store_access::open_readonly(store_path)?;
    let rows = store
        .list_metadata(selector.to_filter()?)
        .map_err(|e| e.to_string())?;

    // BTreeMap keeps the output stably ordered by hash across runs, which
    // matters for diffing two inspections of the same store.
    let mut groups: BTreeMap<[u8; 32], Vec<infrastore_core::TimeSeriesMetadata>> =
        BTreeMap::new();
    for key in rows {
        let hash = key.data_hash;
        if let Some(prefix) = &wanted
            && !fields::hash_hex(&hash).starts_with(prefix)
        {
            continue;
        }
        groups.entry(hash).or_default().push(key);
    }

    if let Some(prefix) = &wanted
        && groups.is_empty()
    {
        return Err(format!(
            "no stored array has a hash starting with '{prefix}'"
        ));
    }

    let mut items = Vec::with_capacity(groups.len());
    for (hash, keys) in &groups {
        let location = match store.locate_array(hash) {
            Ok(loc) => loc.to_string(),
            Err(e) => format!("unavailable: {e}"),
        };
        let (sts_refs, dst_refs) = store
            .count_array_references(hash)
            .map_err(|e| e.to_string())?;
        items.push(json!({
            "data_hash": fields::hash_hex(hash),
            "location": location,
            "refs": keys.len(),
            "refs_single": sts_refs,
            "refs_dst": dst_refs,
            "keys": keys.iter().map(key_json).collect::<Vec<_>>(),
        }));
    }

    let headers: Vec<String> = ["Hash", "Refs", "Location", "Series"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let table_rows: Vec<Vec<String>> = groups
        .iter()
        .zip(&items)
        .map(|((hash, keys), item)| {
            vec![
                fields::short_hash(hash),
                keys.len().to_string(),
                item["location"].as_str().unwrap_or("-").to_string(),
                summarize_keys(keys),
            ]
        })
        .collect();

    match format {
        f if f.is_json() => output::print_items(f, &items)?,
        Format::Csv => output::display_csv_rows(&headers, &table_rows)?,
        _ => output::display_table_dyn(&headers, &table_rows),
    }
    Ok(())
}

/// A row as a JSON object, spelling out every identity field.
fn key_json(id: &infrastore_core::TimeSeriesMetadata) -> Value {
    json!({
        "owner_id": id.owner_id,
        "owner_category": id.owner_category.as_str(),
        "type": id.time_series_type.as_str(),
        "name": id.name,
        "resolution": id.resolution.map(|p| p.to_iso8601()),
        "interval": id.interval.map(|p| p.to_iso8601()),
        "features": fields::features_json(&id.features),
    })
}

/// A compact `name (owner N)` list for the table view, truncated so one
/// heavily-shared array cannot flood the terminal.
fn summarize_keys(keys: &[infrastore_core::TimeSeriesMetadata]) -> String {
    const MAX: usize = 3;
    let mut parts: Vec<String> = keys
        .iter()
        .take(MAX)
        .map(|k| format!("{} (owner {})", k.name, k.owner_id))
        .collect();
    if keys.len() > MAX {
        parts.push(format!("+{} more", keys.len() - MAX));
    }
    parts.join(", ")
}

/// `store-info`: what this artifact is, before you open it with anything else.
pub fn store_info(store_path: &Path, format: Format) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let sqlite = store_access::catalog_path(store_path);
    let size = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.len())
            .map(Value::from)
            .unwrap_or(Value::Null)
    };
    let compression = match store.compression() {
        infrastore_core::Compression::None => "none".to_string(),
        infrastore_core::Compression::Deflate { level, shuffle } => {
            format!("deflate:{level}{}", if shuffle { " +shuffle" } else { "" })
        }
    };
    let pairs = vec![
        ("hdf5_path".into(), json!(store_path.display().to_string())),
        ("hdf5_bytes".into(), size(store_path)),
        ("sqlite_path".into(), json!(sqlite.display().to_string())),
        ("sqlite_bytes".into(), size(&sqlite)),
        ("storage_backend".into(), json!("hdf5")),
        (
            "data_format_version".into(),
            json!(infrastore_core::DATA_FORMAT_VERSION),
        ),
        ("compression".into(), json!(compression)),
        (
            "time_references".into(),
            json!(time_reference_audit(&store)?),
        ),
        ("cli_version".into(), json!(env!("CARGO_PKG_VERSION"))),
    ];
    render_kv("Store", pairs, format)
}

/// The distinct `time_reference` spellings the catalog holds, with any IANA zone
/// name this build's tz database does not recognize flagged.
///
/// The store deliberately does not gate on zone existence — that would couple
/// legitimate data to a release cadence (see `TimeReference::validate`) — so it
/// is *audited* instead, and this is where the audit is readable. A typo like
/// `America/Dever` is then findable in one command, rather than surfacing at
/// some later read in some other language.
///
/// One `?` flag per unrecognized name rather than a separate list: the point is
/// to make a wrong spelling jump out of the line it is on.
fn time_reference_audit(store: &infrastore_core::Store) -> Result<Vec<String>, String> {
    let (references, unspecified) = store.list_time_references().map_err(|e| e.to_string())?;
    let mut out: Vec<String> = references
        .iter()
        .map(|reference| {
            let spelling = reference.as_storage_string();
            if crate::fields::zone_is_known(reference) {
                spelling
            } else {
                format!("{spelling} (unrecognized zone?)")
            }
        })
        .collect();
    if unspecified {
        out.push("(unspecified)".to_string());
    }
    Ok(out)
}

/// `verify`: run integrity verification; nonzero exit when errors are present.
pub fn verify(store_path: &Path, format: Format) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let report = store.verify_integrity().map_err(|e| e.to_string())?;
    let headers = vec!["Error".to_string()];
    let rows: Vec<Vec<String>> = report.errors.iter().map(|e| vec![e.clone()]).collect();
    match format {
        f if f.is_json() => output::print_value(f, &json!({ "errors": report.errors }))?,
        Format::Csv => output::display_csv_rows(&headers, &rows)?,
        _ => {
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
        f if f.is_json() => {
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
            output::print_items(f, &items)?;
        }
        Format::Csv => output::display_csv_rows(&headers, &table_rows)?,
        _ => output::display_table_dyn(&headers, &table_rows),
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
        f if f.is_json() => output::print_value(
            f,
            &json!({
                "resolutions": iso(&res),
                "intervals": iso(&intervals),
            }),
        )?,
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

    // Static and forecast series are two shapes, and the human table shows them
    // as two tables under two headings. CSV cannot: a stream carrying a prose
    // line, a 6-column header, a 1-line heading and an 8-column header is not a
    // CSV at all, and a strict reader dies on row two. Every other query command
    // in this file has a dedicated `Format::Csv` arm for exactly this reason.
    //
    // So machine output is one uniform table, `Kind` naming which shape a row
    // is and the columns that do not apply left as `-`. Nothing is lost against
    // the human view, and the JSON form keeps its two separate lists.
    match format {
        f if f.is_json() => output::print_value(
            f,
            &json!({
                "static": static_items,
                "forecast": forecast_items,
            }),
        )?,
        Format::Csv => {
            let headers: Vec<String> = [
                "Kind",
                "Owner Type",
                "Type",
                "Name",
                "Resolution",
                "Time Steps",
                "Horizon",
                "Interval",
                "Windows",
                "Series",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            let mut rows: Vec<Vec<String>> = Vec::new();
            for v in &static_items {
                rows.push(vec![
                    "static".to_string(),
                    json_str(v, "owner_type"),
                    json_str(v, "time_series_type"),
                    json_str(v, "name"),
                    json_str(v, "resolution"),
                    json_str(v, "time_step_count"),
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    json_str(v, "count"),
                ]);
            }
            for v in &forecast_items {
                rows.push(vec![
                    "forecast".to_string(),
                    json_str(v, "owner_type"),
                    json_str(v, "time_series_type"),
                    json_str(v, "name"),
                    json_str(v, "resolution"),
                    "-".to_string(),
                    json_str(v, "horizon"),
                    json_str(v, "interval"),
                    json_str(v, "window_count"),
                    json_str(v, "count"),
                ]);
            }
            output::display_csv_rows(&headers, &rows)?;
        }
        _ => {
            // `Series` (not `Count`): this column is how many series fall in
            // the group, while the forecast table's own `count` is the number
            // of windows. `Time Steps` / `Windows` name the per-series shape.
            if show_static {
                println!("{}", color::header("Static series"));
                let headers: Vec<String> = [
                    "Owner Type",
                    "Type",
                    "Name",
                    "Resolution",
                    "Time Steps",
                    "Series",
                ]
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
                            json_str(v, "time_step_count"),
                            json_str(v, "count"),
                        ]
                    })
                    .collect();
                output::display_table_dyn(&headers, &rows);
            }
            if show_forecast {
                println!("{}", color::header("Forecast series"));
                let headers: Vec<String> = [
                    "Owner Type",
                    "Type",
                    "Name",
                    "Resolution",
                    "Horizon",
                    "Interval",
                    "Windows",
                    "Series",
                ]
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
                            json_str(v, "resolution"),
                            json_str(v, "horizon"),
                            json_str(v, "interval"),
                            json_str(v, "window_count"),
                            json_str(v, "count"),
                        ]
                    })
                    .collect();
                output::display_table_dyn(&headers, &rows);
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
