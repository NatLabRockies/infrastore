//! Read-side commands: `list`, `get`, and `info`.

use std::path::Path;

use serde_json::{Map, Value, json};
use time_series_store_core::{
    Dtype, FeatureValue, Features, TimeSeriesData, TimeSeriesMetadata, TypedArray,
};

use crate::color;
use crate::csv_io;
use crate::output::{self, Format};
use crate::parse;
use crate::select::SelectorArgs;
use crate::store_access;

const DEFAULT_LIMIT: usize = 50;

type TimeRange = (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>);

/// `list`: enumerate stored series matching the selector filters.
pub fn list(store_path: &Path, selector: &SelectorArgs, format: Format) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let metas = store
        .list_time_series(selector.to_filter()?)
        .map_err(|e| e.to_string())?;
    let headers: Vec<String> = [
        "Owner",
        "Owner Type",
        "Owner Category",
        "Type",
        "Name",
        "Dtype",
        "Resolution",
        "Length",
        "Units",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    match format {
        Format::Table => {
            output::display_table_dyn(&headers, &metas.iter().map(list_row).collect::<Vec<_>>())
        }
        Format::Csv => {
            output::display_csv_rows(&headers, &metas.iter().map(list_row).collect::<Vec<_>>())?
        }
        Format::Json => {
            let items: Vec<Value> = metas.iter().map(list_json).collect();
            output::print_json_wrapped(&items)?;
        }
    }
    Ok(())
}

fn list_row(m: &TimeSeriesMetadata) -> Vec<String> {
    vec![
        m.owner_id.to_string(),
        m.owner_type.clone(),
        m.owner_category.as_str().to_string(),
        m.time_series_type.as_str().to_string(),
        m.name.clone(),
        m.dtype.as_str().to_string(),
        m.resolution
            .map(parse::format_period)
            .unwrap_or_else(|| "-".to_string()),
        m.length
            .map(|l| l.to_string())
            .unwrap_or_else(|| "-".to_string()),
        m.units.clone().unwrap_or_else(|| "-".to_string()),
    ]
}

fn list_json(m: &TimeSeriesMetadata) -> Value {
    let mut obj = Map::new();
    obj.insert("owner_id".into(), json!(m.owner_id));
    obj.insert("owner_type".into(), json!(m.owner_type));
    obj.insert("owner_category".into(), json!(m.owner_category.as_str()));
    obj.insert("type".into(), json!(m.time_series_type.as_str()));
    obj.insert("name".into(), json!(m.name));
    obj.insert("dtype".into(), json!(m.dtype.as_str()));
    obj.insert(
        "resolution".into(),
        json!(m.resolution.map(parse::format_period)),
    );
    obj.insert("length".into(), json!(m.length));
    obj.insert("units".into(), json!(m.units));
    Value::Object(obj)
}

/// `get`: read a single series and render its values.
pub fn get(
    store_path: &Path,
    selector: &SelectorArgs,
    time_range: Option<&str>,
    limit: Option<usize>,
    full: bool,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let (meta, key) = selector.resolve(&store)?;
    let range = parse_time_range(time_range)?;
    let data = store
        .get_time_series(&key, range)
        .map_err(|e| e.to_string())?;

    match &data {
        TimeSeriesData::SingleTimeSeries(s) => {
            let ts: Vec<String> = (0..s.length)
                .map(|i| {
                    s.resolution
                        .add_to(s.initial_timestamp, i as i64)
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_default()
                })
                .collect();
            render_sequential(&meta, &ts, &s.data, format, limit, full, false)
        }
        TimeSeriesData::NonSequentialTimeSeries(ns) => {
            let ts: Vec<String> = ns.timestamps.iter().map(|t| t.to_rfc3339()).collect();
            render_sequential(&meta, &ts, &ns.data, format, limit, full, true)
        }
        _ => render_forecast(&meta, &data, format, limit, full),
    }
}

/// `info`: metadata plus numeric stats for a single series.
pub fn info(store_path: &Path, selector: &SelectorArgs, format: Format) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let (meta, key) = selector.resolve(&store)?;
    let data = store
        .get_time_series(&key, None)
        .map_err(|e| e.to_string())?;
    let arr = data_array(&data);

    let mut fields: Vec<(String, String)> = Vec::new();
    fields.push(("name".into(), meta.name.clone()));
    fields.push(("owner_id".into(), meta.owner_id.to_string()));
    fields.push(("owner_type".into(), meta.owner_type.clone()));
    fields.push(("owner_category".into(), meta.owner_category.as_str().into()));
    fields.push(("type".into(), meta.time_series_type.as_str().into()));
    fields.push(("dtype".into(), arr.dtype.as_str().into()));
    fields.push(("shape".into(), format!("{:?}", arr.shape)));
    if let Some(r) = meta.resolution {
        fields.push(("resolution".into(), parse::format_period(r)));
    }
    if let Some(t) = meta.initial_timestamp {
        fields.push(("initial_timestamp".into(), t.to_rfc3339()));
    }
    if let Some(l) = meta.length {
        fields.push(("length".into(), l.to_string()));
    }
    if let Some(h) = meta.horizon {
        fields.push(("horizon".into(), parse::format_period(h)));
    }
    if let Some(iv) = meta.interval {
        fields.push(("interval".into(), parse::format_period(iv)));
    }
    if let Some(c) = meta.count {
        fields.push(("count".into(), c.to_string()));
    }
    if let Some(p) = &meta.percentiles {
        fields.push(("percentiles".into(), format!("{p:?}")));
    }
    if let Some(u) = &meta.units {
        fields.push(("units".into(), u.clone()));
    }
    for (k, v) in &meta.features {
        fields.push((format!("feature.{k}"), feature_to_string(v)));
    }
    append_stats(arr, &mut fields);

    match format {
        Format::Table => {
            let rows: Vec<Vec<String>> = fields
                .iter()
                .map(|(k, v)| vec![color::label(k), v.clone()])
                .collect();
            output::display_table_dyn(&field_value_header(), &rows);
        }
        Format::Csv => output::display_csv_rows(&field_value_header(), &as_rows(&fields))?,
        Format::Json => {
            let mut obj = Map::new();
            for (k, v) in &fields {
                obj.insert(k.clone(), json!(v));
            }
            output::print_json(&Value::Object(obj))?;
        }
    }
    Ok(())
}

// --- rendering helpers -----------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_sequential(
    meta: &TimeSeriesMetadata,
    timestamps: &[String],
    arr: &TypedArray,
    format: Format,
    limit: Option<usize>,
    full: bool,
    timestamp_in_csv: bool,
) -> Result<(), String> {
    let per_step = arr.element_shape().iter().product::<usize>().max(1);
    let decoded = csv_io::array_to_strings(arr);
    let length = arr.length();
    let vheaders = value_headers(per_step);

    match format {
        Format::Table => {
            let mut header = vec!["timestamp".to_string()];
            header.extend(vheaders);
            let max = if full {
                length
            } else {
                limit.unwrap_or(DEFAULT_LIMIT)
            };
            let shown = length.min(max);
            let mut rows = Vec::with_capacity(shown);
            for i in 0..shown {
                let mut row = Vec::with_capacity(1 + per_step);
                row.push(timestamps.get(i).cloned().unwrap_or_default());
                for j in 0..per_step {
                    row.push(decoded[i * per_step + j].clone());
                }
                rows.push(row);
            }
            output::display_table_dyn(&header, &rows);
            if length > shown {
                println!(
                    "{}",
                    color::dim(&format!("... {} more rows (use --full)", length - shown))
                );
            }
        }
        Format::Csv => {
            let mut header = Vec::new();
            if timestamp_in_csv {
                header.push("timestamp".to_string());
            }
            header.extend(value_headers(per_step));
            let mut rows = Vec::with_capacity(length);
            for i in 0..length {
                let mut row = Vec::new();
                if timestamp_in_csv {
                    row.push(timestamps.get(i).cloned().unwrap_or_default());
                }
                for j in 0..per_step {
                    row.push(decoded[i * per_step + j].clone());
                }
                rows.push(row);
            }
            output::display_csv_rows(&header, &rows)?;
        }
        Format::Json => {
            let mut obj = Map::new();
            meta_fields(meta, arr, &mut obj);
            obj.insert("timestamps".into(), json!(timestamps));
            obj.insert("values".into(), json!(decoded));
            output::print_json(&Value::Object(obj))?;
        }
    }
    Ok(())
}

fn render_forecast(
    meta: &TimeSeriesMetadata,
    data: &TimeSeriesData,
    format: Format,
    limit: Option<usize>,
    full: bool,
) -> Result<(), String> {
    let arr = data_array(data);
    let decoded = csv_io::array_to_strings(arr);

    match format {
        Format::Table => {
            println!(
                "{}",
                color::dim(&format!(
                    "{} '{}' shape {:?} dtype {} (row-major flat values; use -f csv or -f json for structured output)",
                    meta.time_series_type.as_str(),
                    meta.name,
                    arr.shape,
                    arr.dtype.as_str(),
                ))
            );
            let max = if full {
                decoded.len()
            } else {
                limit.unwrap_or(DEFAULT_LIMIT)
            };
            let shown = decoded.len().min(max);
            let rows: Vec<Vec<String>> = (0..shown)
                .map(|i| vec![i.to_string(), decoded[i].clone()])
                .collect();
            output::display_table_dyn(&["index".to_string(), "value".to_string()], &rows);
            if decoded.len() > shown {
                println!(
                    "{}",
                    color::dim(&format!(
                        "... {} more values (use --full)",
                        decoded.len() - shown
                    ))
                );
            }
        }
        Format::Csv => {
            let rows: Vec<Vec<String>> = decoded.iter().map(|v| vec![v.clone()]).collect();
            output::display_csv_rows(&["value".to_string()], &rows)?;
        }
        Format::Json => {
            let mut obj = Map::new();
            meta_fields(meta, arr, &mut obj);
            if let TimeSeriesData::Scenarios(s) = data {
                obj.insert("scenario_count".into(), json!(s.scenario_count));
            }
            obj.insert("values".into(), json!(decoded));
            output::print_json(&Value::Object(obj))?;
        }
    }
    Ok(())
}

fn value_headers(per_step: usize) -> Vec<String> {
    if per_step <= 1 {
        vec!["value".to_string()]
    } else {
        (0..per_step).map(|i| format!("value[{i}]")).collect()
    }
}

fn data_array(d: &TimeSeriesData) -> &TypedArray {
    match d {
        TimeSeriesData::SingleTimeSeries(s) => &s.data,
        TimeSeriesData::NonSequentialTimeSeries(s) => &s.data,
        TimeSeriesData::Deterministic(d) => &d.data,
        TimeSeriesData::Probabilistic(p) => &p.data,
        TimeSeriesData::Scenarios(s) => &s.data,
    }
}

fn meta_fields(meta: &TimeSeriesMetadata, arr: &TypedArray, obj: &mut Map<String, Value>) {
    obj.insert("name".into(), json!(meta.name));
    obj.insert("owner_id".into(), json!(meta.owner_id));
    obj.insert("owner_type".into(), json!(meta.owner_type));
    obj.insert("owner_category".into(), json!(meta.owner_category.as_str()));
    obj.insert("type".into(), json!(meta.time_series_type.as_str()));
    obj.insert("dtype".into(), json!(arr.dtype.as_str()));
    obj.insert("shape".into(), json!(arr.shape));
    obj.insert("element_shape".into(), json!(arr.element_shape()));
    if let Some(u) = &meta.units {
        obj.insert("units".into(), json!(u));
    }
    if let Some(r) = meta.resolution {
        obj.insert("resolution".into(), json!(parse::format_period(r)));
    }
    if let Some(t) = meta.initial_timestamp {
        obj.insert("initial_timestamp".into(), json!(t.to_rfc3339()));
    }
    if let Some(h) = meta.horizon {
        obj.insert("horizon".into(), json!(parse::format_period(h)));
    }
    if let Some(iv) = meta.interval {
        obj.insert("interval".into(), json!(parse::format_period(iv)));
    }
    if let Some(c) = meta.count {
        obj.insert("count".into(), json!(c));
    }
    if let Some(p) = &meta.percentiles {
        obj.insert("percentiles".into(), json!(p));
    }
    if !meta.features.is_empty() {
        obj.insert("features".into(), features_json(&meta.features));
    }
}

fn append_stats(arr: &TypedArray, fields: &mut Vec<(String, String)>) {
    let vals = csv_io::array_to_f64_lossy(arr);
    if vals.is_empty() {
        return;
    }
    if arr.dtype == Dtype::Bool {
        let t = vals.iter().filter(|x| **x != 0.0).count();
        fields.push(("true_count".into(), t.to_string()));
        fields.push(("false_count".into(), (vals.len() - t).to_string()));
    } else {
        let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
        let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        fields.push(("min".into(), min.to_string()));
        fields.push(("max".into(), max.to_string()));
        fields.push(("mean".into(), mean.to_string()));
    }
    fields.push(("num_elements".into(), vals.len().to_string()));
}

fn features_json(features: &Features) -> Value {
    let mut obj = Map::new();
    for (k, v) in features {
        let value = match v {
            FeatureValue::Int(i) => json!(i),
            FeatureValue::Float(f) => json!(f),
            FeatureValue::Bool(b) => json!(b),
            FeatureValue::Str(s) => json!(s),
        };
        obj.insert(k.clone(), value);
    }
    Value::Object(obj)
}

fn feature_to_string(v: &FeatureValue) -> String {
    match v {
        FeatureValue::Int(i) => i.to_string(),
        FeatureValue::Float(f) => f.to_string(),
        FeatureValue::Bool(b) => b.to_string(),
        FeatureValue::Str(s) => s.clone(),
    }
}

fn field_value_header() -> Vec<String> {
    vec!["field".to_string(), "value".to_string()]
}

fn as_rows(fields: &[(String, String)]) -> Vec<Vec<String>> {
    fields
        .iter()
        .map(|(k, v)| vec![k.clone(), v.clone()])
        .collect()
}

fn parse_time_range(spec: Option<&str>) -> Result<Option<TimeRange>, String> {
    let Some(spec) = spec else {
        return Ok(None);
    };
    let (start, end) = spec
        .split_once("..")
        .ok_or_else(|| format!("invalid --time-range '{spec}' (expected START..END)"))?;
    Ok(Some((
        parse::parse_timestamp(start)?,
        parse::parse_timestamp(end)?,
    )))
}
