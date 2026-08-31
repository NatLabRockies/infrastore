//! The `export` command: bulk read-direction inverse of `add`. Writes each
//! matched series' values (CSV with real timestamps, or structured JSON) to a
//! file per series under `--dir`, or to stdout when exactly one series
//! matches.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use infrastore_core::{TimeSeriesData, TimeSeriesMetadata};
use serde_json::{Map, Value, json};

use crate::color;
use crate::csv_io;
use crate::fields;
use crate::output::{self, Format};
use crate::select::{self, SelectorArgs};
use crate::store_access;

use super::show;

pub fn run(
    store_path: &Path,
    selector: &SelectorArgs,
    dir: Option<&Path>,
    time_range: Option<&str>,
    format: Format,
) -> Result<(), String> {
    // `table` is the global default, so the first `export` anyone ran used to
    // fail on a flag they never passed. There is no table export to fall back
    // to, and CSV is both the format `add` reads back and the one `--dir` is
    // for, so it is what an unspecified format means here.
    let format = match format {
        Format::Table => Format::Csv,
        other => other,
    };
    let file_ext = match format {
        Format::Csv => "csv",
        Format::Jsonl => "jsonl",
        _ => "json",
    };

    let range = crate::parse::parse_time_range(time_range)?;
    let store = store_access::open_readonly(store_path)?;
    let metas = store
        .list_metadata(selector.to_filter()?)
        .map_err(|e| e.to_string())?;
    if metas.is_empty() {
        return match dir {
            // Without --dir, stdout *is* the exported series, so a notice
            // written there would be indistinguishable from content. It goes to
            // stderr and stdout stays empty.
            None => {
                eprintln!("{}", color::dim_err("No time series matched the selector."));
                Ok(())
            }
            // With --dir, stdout carries the status report either way, so the
            // empty case reports a zero in the same shape as a real one.
            Some(dir) => output::report(
                format,
                || json!({ "exported": 0, "dir": dir.display().to_string(), "files": [] }),
                || println!("{}", color::dim("No time series matched the selector.")),
            ),
        };
    }
    if dir.is_none() && metas.len() > 1 {
        return Err(format!(
            "{} series matched; pass --dir to export multiple series",
            metas.len()
        ));
    }

    // One batched read instead of N catalog round-trips.
    let ids: Vec<infrastore_core::TimeSeriesId> =
        metas.iter().map(select::id_of).collect::<Result<_, _>>()?;
    // Bounds, not a window: an export names the span it wants and takes
    // whatever each series has in it.
    let datas = match range {
        Some(r) => store.read_by_ids_range(&ids, r),
        None => store.read_by_ids(&ids, infrastore_core::ReadWindow::full()),
    }
    .map_err(|e| e.to_string())?;

    match dir {
        None => {
            let content = render(&metas[0], &datas[0], format)?;
            output::write_raw(&content)?;
        }
        Some(dir) => {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
            let stems = unique_file_stems(&metas);
            let mut written = Vec::with_capacity(metas.len());
            for ((meta, data), stem) in metas.iter().zip(&datas).zip(&stems) {
                let path = dir.join(format!("{stem}.{file_ext}"));
                let content = render(meta, data, format)?;
                std::fs::write(&path, content)
                    .map_err(|e| format!("writing {}: {e}", path.display()))?;
                written.push(path.display().to_string());
            }
            return output::report(
                format,
                || {
                    json!({
                        "exported": written.len(),
                        "dir": dir.display().to_string(),
                        "files": written,
                    })
                },
                || {
                    for path in &written {
                        println!("exported {path}");
                    }
                    println!(
                        "{}",
                        color::header(&format!(
                            "Exported {} time series to {}.",
                            written.len(),
                            dir.display()
                        ))
                    );
                },
            );
        }
    }
    Ok(())
}

/// `<owner_id>_<owner_type>_<name>_<type>` with path-hostile characters mapped
/// to `-`, plus a short identity digest when one stem would otherwise be shared.
///
/// The plain stem omits resolution, interval, and features — all part of a
/// series' identity — so two distinct series could land on one path and the
/// second would silently overwrite the first. `unique_file_stems` resolves that
/// by suffixing a short hash of the full identity, and only for the stems that
/// actually collide, so the common case keeps its readable filename.
fn file_stem(meta: &TimeSeriesMetadata) -> String {
    let raw = format!(
        "{}_{}_{}_{}",
        meta.owner_id,
        meta.owner_type,
        meta.name,
        meta.time_series_type.as_str()
    );
    sanitize(&raw)
}

fn sanitize(raw: &str) -> String {
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

/// One filename stem per input metadata, guaranteed distinct.
///
/// Colliding stems gain a suffix built from the identity fields the plain stem
/// drops — resolution, interval, features — which is more use in a directory
/// listing than an opaque digest (`..._model_year-2030.csv` says what it is). A
/// trailing ordinal is the backstop for the pathological case where even that
/// is not enough, so the result is unique by construction rather than by
/// assumption.
///
/// Collision detection is case-insensitive: macOS and Windows filesystems treat
/// `Load` and `load` as one path, so two series differing only in the case of a
/// name would overwrite each other there but not on Linux. Disambiguating both
/// keeps an export identical across platforms.
fn unique_file_stems(metas: &[TimeSeriesMetadata]) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for m in metas {
        *counts.entry(file_stem(m).to_lowercase()).or_default() += 1;
    }
    let mut taken: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(metas.len());
    for m in metas {
        let base = file_stem(m);
        let mut stem = if counts.get(&base.to_lowercase()).copied().unwrap_or(0) > 1 {
            match identity_suffix(m) {
                Some(suffix) => format!("{base}_{suffix}"),
                None => base.clone(),
            }
        } else {
            base.clone()
        };
        if taken.contains(&stem.to_lowercase()) {
            let mut n = 2;
            while taken.contains(&format!("{stem}_{n}").to_lowercase()) {
                n += 1;
            }
            stem = format!("{stem}_{n}");
        }
        taken.insert(stem.to_lowercase());
        out.push(stem);
    }
    out
}

/// The identity fields a plain stem omits, rendered for a filename. `None` when
/// the series carries none of them.
fn identity_suffix(meta: &TimeSeriesMetadata) -> Option<String> {
    /// Long feature maps would otherwise push past filesystem name limits.
    const MAX: usize = 60;

    let mut parts = Vec::new();
    if let Some(r) = meta.resolution {
        parts.push(r.to_iso8601());
    }
    if let Some(i) = meta.interval {
        parts.push(i.to_iso8601());
    }
    for (k, v) in &meta.features {
        parts.push(format!("{k}-{}", crate::fields::feature_value_str(v)));
    }
    if parts.is_empty() {
        return None;
    }
    let mut s = sanitize(&parts.join("_"));
    s.truncate(MAX);
    Some(s)
}

fn render(
    meta: &TimeSeriesMetadata,
    data: &TimeSeriesData,
    format: Format,
) -> Result<String, String> {
    match format {
        Format::Csv => render_csv(meta, data),
        other => render_json(meta, data, other),
    }
}

fn render_csv(meta: &TimeSeriesMetadata, data: &TimeSeriesData) -> Result<String, String> {
    let (headers, rows) = match data {
        TimeSeriesData::SingleTimeSeries(s) => {
            let timestamps: Vec<String> = (0..s.length)
                .map(|i| {
                    s.resolution
                        .add_to(s.initial_timestamp, i as i64)
                        .map(|t| fields::render_timestamp(t, s.time_reference.as_ref()))
                        .ok_or_else(|| format!("timestamp overflow at grid index {i}"))
                })
                .collect::<Result<_, String>>()?;
            sequential_rows(&timestamps, &s.data)
        }
        TimeSeriesData::NonSequentialTimeSeries(ns) => {
            let timestamps: Vec<String> =
                fields::render_timestamps(&ns.timestamps, ns.time_reference.as_ref());
            sequential_rows(&timestamps, &ns.data)
        }
        // The breakpoints and their values, which is the whole series. The CSV
        // this writes is exactly what `add` reads back for the type, so the
        // round trip closes the same way it does for the irregular type above.
        TimeSeriesData::PersistentTimeSeries(p) => {
            let timestamps: Vec<String> =
                fields::render_timestamps(&p.timestamps, p.time_reference.as_ref());
            sequential_rows(&timestamps, &p.data)
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
    arr: &infrastore_core::TypedArray,
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

/// One series as a JSON document: pretty for `-f json`, one compact line for
/// `-f jsonl`.
///
/// `jsonl` is advertised globally as line-delimited output, and an exported file
/// full of pretty-printed documents is not something a line-oriented consumer
/// can read. One series is one document either way — the two formats differ only
/// in whether it is allowed to wrap.
fn render_json(
    meta: &TimeSeriesMetadata,
    data: &TimeSeriesData,
    format: Format,
) -> Result<String, String> {
    let mut obj = Map::new();
    obj.insert("owner_id".into(), json!(meta.owner_id));
    obj.insert("owner_type".into(), json!(meta.owner_type));
    obj.insert("owner_category".into(), json!(meta.owner_category.as_str()));
    obj.insert("type".into(), json!(meta.time_series_type.as_str()));
    obj.insert("name".into(), json!(meta.name));
    obj.insert("units".into(), json!(meta.units));
    obj.insert("quantity_kind".into(), json!(meta.quantity_kind));
    obj.insert(
        "unit_system".into(),
        json!(meta.unit_system.map(|u| u.as_str())),
    );
    obj.insert(
        "time_reference".into(),
        json!(
            meta.time_reference
                .as_ref()
                .map(infrastore_core::TimeReference::as_storage_string)
        ),
    );
    obj.insert("component_field".into(), json!(meta.component_field));
    obj.insert("application_data".into(), json!(meta.application_data));
    obj.insert("element_type".into(), json!(meta.element_type.to_string()));
    // Features are part of identity: an export that drops them cannot describe
    // which of several same-named series it holds, and cannot be turned back
    // into a descriptor that would recreate it.
    obj.insert("features".into(), fields::features_json(&meta.features));
    obj.insert("data_hash".into(), json!(fields::hash_hex(&meta.data_hash)));
    match data {
        TimeSeriesData::SingleTimeSeries(s) => {
            obj.insert(
                "initial_timestamp".into(),
                json!(fields::render_timestamp(
                    s.initial_timestamp,
                    s.time_reference.as_ref()
                )),
            );
            obj.insert("resolution".into(), json!(s.resolution.to_iso8601()));
            obj.insert("shape".into(), json!(s.data.shape));
            obj.insert(
                "values".into(),
                json!(csv_io::array_to_json_values(&s.data)),
            );
        }
        TimeSeriesData::NonSequentialTimeSeries(ns) => {
            let timestamps: Vec<String> =
                fields::render_timestamps(&ns.timestamps, ns.time_reference.as_ref());
            obj.insert("timestamps".into(), json!(timestamps));
            obj.insert("shape".into(), json!(ns.data.shape));
            obj.insert(
                "values".into(),
                json!(csv_io::array_to_json_values(&ns.data)),
            );
        }
        TimeSeriesData::PersistentTimeSeries(p) => {
            let timestamps: Vec<String> =
                fields::render_timestamps(&p.timestamps, p.time_reference.as_ref());
            obj.insert("timestamps".into(), json!(timestamps));
            obj.insert("shape".into(), json!(p.data.shape));
            obj.insert(
                "values".into(),
                json!(csv_io::array_to_json_values(&p.data)),
            );
        }
        _ => {
            let arr = match data {
                TimeSeriesData::Deterministic(d) => &d.data,
                TimeSeriesData::Probabilistic(p) => &p.data,
                TimeSeriesData::Scenarios(s) => &s.data,
                _ => unreachable!(),
            };
            if let Some(t) = meta.initial_timestamp {
                obj.insert(
                    "initial_timestamp".into(),
                    json!(fields::render_timestamp(t, meta.time_reference.as_ref())),
                );
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
            obj.insert("values".into(), json!(csv_io::array_to_json_values(arr)));
        }
    }
    let value = Value::Object(obj);
    let text = match format {
        Format::Jsonl => serde_json::to_string(&value),
        _ => serde_json::to_string_pretty(&value),
    };
    text.map(|s| s + "\n").map_err(|e| e.to_string())
}
