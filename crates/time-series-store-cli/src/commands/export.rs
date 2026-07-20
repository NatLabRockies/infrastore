//! The `export` command: bulk read-direction inverse of `add`. Writes each
//! matched series' values (CSV with real timestamps, or structured JSON) to a
//! file per series under `--dir`, or to stdout when exactly one series
//! matches.

use std::path::Path;

use serde_json::{Map, Value, json};
use time_series_store_core::{TimeSeriesData, TimeSeriesMetadata};

use crate::color;
use crate::csv_io;
use crate::output::Format;
use crate::select::{self, SelectorArgs};
use crate::store_access;

use super::show;

pub fn run(
    store_path: &Path,
    selector: &SelectorArgs,
    dir: Option<&Path>,
    format: Format,
) -> Result<(), String> {
    let ext = match format {
        Format::Csv => "csv",
        Format::Json => "json",
        Format::Table => {
            return Err("export writes files; use -f csv or -f json".to_string());
        }
    };

    let store = store_access::open_readonly(store_path)?;
    let metas = store
        .list_time_series(selector.to_filter()?)
        .map_err(|e| e.to_string())?;
    if metas.is_empty() {
        println!("{}", color::dim("No time series matched the selector."));
        return Ok(());
    }
    if dir.is_none() && metas.len() > 1 {
        return Err(format!(
            "{} series matched; pass --dir to export multiple series",
            metas.len()
        ));
    }

    // One batched read instead of N catalog round-trips.
    let identities: Vec<_> = metas.iter().map(select::key_of).collect();
    let refs: Vec<&_> = identities.iter().collect();
    let datas = store.bulk_read(&refs).map_err(|e| e.to_string())?;

    match dir {
        None => {
            let content = render(&metas[0], &datas[0], format)?;
            print!("{content}");
        }
        Some(dir) => {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
            for (meta, data) in metas.iter().zip(&datas) {
                let path = dir.join(format!("{}.{ext}", file_stem(meta)));
                let content = render(meta, data, format)?;
                std::fs::write(&path, content)
                    .map_err(|e| format!("writing {}: {e}", path.display()))?;
                println!("exported {}", path.display());
            }
            println!(
                "{}",
                color::header(&format!(
                    "Exported {} time series to {}.",
                    metas.len(),
                    dir.display()
                ))
            );
        }
    }
    Ok(())
}

/// `<owner_id>_<owner_type>_<name>_<type>` with path-hostile characters mapped
/// to `-`.
fn file_stem(meta: &TimeSeriesMetadata) -> String {
    let raw = format!(
        "{}_{}_{}_{}",
        meta.owner_id,
        meta.owner_type,
        meta.name,
        meta.time_series_type.as_str()
    );
    raw.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn render(
    meta: &TimeSeriesMetadata,
    data: &TimeSeriesData,
    format: Format,
) -> Result<String, String> {
    match format {
        Format::Csv => render_csv(meta, data),
        Format::Json => render_json(meta, data),
        Format::Table => unreachable!("rejected in run"),
    }
}

fn render_csv(meta: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<String, String> {
    let (headers, rows) = match data {
        TimeSeriesData::SingleTimeSeries(s) => {
            let timestamps: Vec<String> = (0..s.length)
                .map(|i| {
                    s.resolution
                        .add_to(s.initial_timestamp, i as i64)
                        .map(|t| t.to_rfc3339())
                        .ok_or_else(|| format!("timestamp overflow at grid index {i}"))
                })
                .collect::<Result<_, String>>()?;
            sequential_rows(&timestamps, &s.data)
        }
        TimeSeriesData::NonSequentialTimeSeries(ns) => {
            let timestamps: Vec<String> = ns.timestamps.iter().map(|t| t.to_rfc3339()).collect();
            sequential_rows(&timestamps, &ns.data)
        }
        _ => show::forecast_csv_rows(meta, data)?,
    };
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(&headers).map_err(|e| e.to_string())?;
    for row in &rows {
        writer.write_record(row).map_err(|e| e.to_string())?;
    }
    let bytes = writer.into_inner().map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

fn sequential_rows(
    timestamps: &[String],
    arr: &time_series_store_core::TypedArray,
) -> (Vec<String>, Vec<Vec<String>>) {
    let per_step = arr.element_shape().iter().product::<usize>().max(1);
    let decoded = csv_io::array_to_strings(arr);
    let mut headers = vec!["timestamp".to_string()];
    if per_step <= 1 {
        headers.push("value".to_string());
    } else {
        headers.extend((0..per_step).map(|i| format!("value[{i}]")));
    }
    let rows = (0..arr.length())
        .map(|i| {
            let mut row = Vec::with_capacity(1 + per_step);
            row.push(timestamps.get(i).cloned().unwrap_or_default());
            for j in 0..per_step {
                row.push(decoded[i * per_step + j].clone());
            }
            row
        })
        .collect();
    (headers, rows)
}

fn render_json(meta: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<String, String> {
    let mut obj = Map::new();
    obj.insert("owner_id".into(), json!(meta.owner_id));
    obj.insert("owner_type".into(), json!(meta.owner_type));
    obj.insert("owner_category".into(), json!(meta.owner_category.as_str()));
    obj.insert("type".into(), json!(meta.time_series_type.as_str()));
    obj.insert("name".into(), json!(meta.name));
    obj.insert("units".into(), json!(meta.units));
    obj.insert("logical_type".into(), json!(meta.logical_type));
    match data {
        TimeSeriesData::SingleTimeSeries(s) => {
            obj.insert(
                "initial_timestamp".into(),
                json!(s.initial_timestamp.to_rfc3339()),
            );
            obj.insert("resolution".into(), json!(s.resolution.to_iso8601()));
            obj.insert("shape".into(), json!(s.data.shape));
            obj.insert("values".into(), json!(csv_io::array_to_strings(&s.data)));
        }
        TimeSeriesData::NonSequentialTimeSeries(ns) => {
            let timestamps: Vec<String> = ns.timestamps.iter().map(|t| t.to_rfc3339()).collect();
            obj.insert("timestamps".into(), json!(timestamps));
            obj.insert("shape".into(), json!(ns.data.shape));
            obj.insert("values".into(), json!(csv_io::array_to_strings(&ns.data)));
        }
        _ => {
            let arr = match data {
                TimeSeriesData::Deterministic(d) => &d.data,
                TimeSeriesData::Probabilistic(p) => &p.data,
                TimeSeriesData::Scenarios(s) => &s.data,
                _ => unreachable!(),
            };
            if let Some(t) = meta.initial_timestamp {
                obj.insert("initial_timestamp".into(), json!(t.to_rfc3339()));
            }
            if let Some(r) = meta.resolution {
                obj.insert("resolution".into(), json!(r.to_iso8601()));
            }
            if let Some(h) = meta.horizon {
                obj.insert("horizon".into(), json!(h.to_iso8601()));
            }
            if let Some(iv) = meta.interval {
                obj.insert("interval".into(), json!(iv.to_iso8601()));
            }
            if let Some(c) = meta.count {
                obj.insert("count".into(), json!(c));
            }
            if let Some(p) = &meta.percentiles {
                obj.insert("percentiles".into(), json!(p));
            }
            if let TimeSeriesData::Scenarios(s) = data {
                obj.insert("scenario_count".into(), json!(s.scenario_count));
            }
            obj.insert("shape".into(), json!(arr.shape));
            obj.insert("values".into(), json!(csv_io::array_to_strings(arr)));
        }
    }
    serde_json::to_string_pretty(&Value::Object(obj))
        .map(|s| s + "\n")
        .map_err(|e| e.to_string())
}
