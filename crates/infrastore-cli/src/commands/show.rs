//! Read-side commands: `list`, `get`, and `info`.

use std::path::Path;

use infrastore_core::{Dtype, TimeSeriesData, TimeSeriesMetadata, TypedArray};
use serde_json::{Map, Value, json};

use crate::color;
use crate::csv_io;
use crate::fields;
use crate::output::{self, Format};
use crate::parse;
use crate::select::SelectorArgs;
use crate::store_access;

const DEFAULT_LIMIT: usize = 50;

type TimeRange = (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>);

/// `list`: enumerate stored series matching the selector filters.
///
/// The table's default columns cover every field that is part of a series'
/// identity — owner, category, type, name, resolution, interval, and features —
/// so two distinct rows can never render identically. `wide` adds the remaining
/// metadata; `-f json` always emits everything.
pub fn list(
    store_path: &Path,
    selector: &SelectorArgs,
    limit: Option<usize>,
    wide: bool,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let all = store
        .list_time_series(selector.to_filter()?)
        .map_err(|e| e.to_string())?;
    let total = all.len();
    let metas: &[TimeSeriesMetadata] = match limit {
        Some(n) if n < total => &all[..n],
        _ => &all,
    };

    match format {
        Format::Table => {
            let headers = list_headers(wide);
            let rows: Vec<Vec<String>> = metas.iter().map(|m| list_row(m, wide)).collect();
            output::display_table_dyn(&headers, &rows);
            if metas.len() < total {
                println!(
                    "{}",
                    color::dim(&format!(
                        "... {} more series (use --limit or -f csv)",
                        total - metas.len()
                    ))
                );
            }
        }
        Format::Csv => {
            let headers = list_headers(wide);
            let rows: Vec<Vec<String>> = metas.iter().map(|m| list_row(m, wide)).collect();
            output::display_csv_rows(&headers, &rows)?;
        }
        Format::Json => {
            let items: Vec<Value> = metas.iter().map(list_json).collect();
            output::print_json_wrapped(&items)?;
        }
    }
    Ok(())
}

/// Columns that pin down identity, plus the physical facts a reader needs.
/// `Hash` is the leading [`fields::SHORT_HASH_LEN`] characters of the array's
/// content hash: it is what ties a catalog row to the bytes in the HDF5 file,
/// and it makes array sharing (two series with one hash) visible at a glance.
const LIST_HEADERS: &[&str] = &[
    "Owner",
    "Owner Type",
    "Category",
    "Type",
    "Name",
    "Features",
    "Element Type",
    "Resolution",
    "Interval",
    "Length",
    "Units",
    "Hash",
];

const LIST_HEADERS_WIDE: &[&str] = &[
    "Initial Timestamp",
    "Horizon",
    "Count",
    "Element Shape",
    "Ext",
];

fn list_headers(wide: bool) -> Vec<String> {
    let mut h: Vec<String> = LIST_HEADERS.iter().map(|s| s.to_string()).collect();
    if wide {
        h.extend(LIST_HEADERS_WIDE.iter().map(|s| s.to_string()));
    }
    h
}

fn list_row(m: &TimeSeriesMetadata, wide: bool) -> Vec<String> {
    let mut row = vec![
        m.owner_id.to_string(),
        m.owner_type.clone(),
        m.owner_category.as_str().to_string(),
        m.time_series_type.as_str().to_string(),
        m.name.clone(),
        fields::features_str(&m.features),
        m.element_type.to_string(),
        fields::opt_period(m.resolution),
        fields::opt_period(m.interval),
        fields::opt(m.length),
        m.units.clone().unwrap_or_else(|| "-".to_string()),
        fields::short_hash(&m.data_hash),
    ];
    if wide {
        row.extend([
            fields::opt(m.initial_timestamp.map(|t| t.to_rfc3339())),
            fields::opt_period(m.horizon),
            fields::opt(m.count),
            format!("{:?}", m.element_shape),
            m.ext.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }
    row
}

/// The full metadata row as JSON. Unlike the table this holds nothing back —
/// it is the scripting entry point, so omitting features or the hash here just
/// forces callers back to `info` one series at a time.
fn list_json(m: &TimeSeriesMetadata) -> Value {
    let mut obj = Map::new();
    obj.insert("owner_id".into(), json!(m.owner_id));
    obj.insert("owner_type".into(), json!(m.owner_type));
    obj.insert("owner_category".into(), json!(m.owner_category.as_str()));
    obj.insert("type".into(), json!(m.time_series_type.as_str()));
    obj.insert("name".into(), json!(m.name));
    obj.insert("features".into(), fields::features_json(&m.features));
    obj.insert("data_hash".into(), json!(fields::hash_hex(&m.data_hash)));
    obj.insert("element_type".into(), json!(m.element_type.to_string()));
    obj.insert("element_shape".into(), json!(m.element_shape));
    obj.insert(
        "resolution".into(),
        json!(m.resolution.map(parse::format_period)),
    );
    obj.insert(
        "initial_timestamp".into(),
        json!(m.initial_timestamp.map(|t| t.to_rfc3339())),
    );
    obj.insert("length".into(), json!(m.length));
    obj.insert("horizon".into(), json!(m.horizon.map(parse::format_period)));
    obj.insert(
        "interval".into(),
        json!(m.interval.map(parse::format_period)),
    );
    obj.insert("count".into(), json!(m.count));
    obj.insert("percentiles".into(), json!(m.percentiles));
    obj.insert("units".into(), json!(m.units));
    obj.insert("ext".into(), json!(m.ext));
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
                        .ok_or_else(|| {
                            format!(
                                "timestamp overflow at grid index {i} (initial {}, \
                                 resolution {})",
                                s.initial_timestamp, s.resolution
                            )
                        })
                })
                .collect::<Result<_, String>>()?;
            render_sequential(&meta, &ts, &s.data, format, limit, full)
        }
        TimeSeriesData::NonSequentialTimeSeries(ns) => {
            let ts: Vec<String> = ns.timestamps.iter().map(|t| t.to_rfc3339()).collect();
            render_sequential(&meta, &ts, &ns.data, format, limit, full)
        }
        _ => render_forecast(&meta, &data, format, limit, full),
    }
}

/// `info`: metadata plus numeric stats for a single series.
///
/// `no_stats` skips reading the array, which is the only part of this command
/// that touches the HDF5 file at all.
pub fn info(
    store_path: &Path,
    selector: &SelectorArgs,
    no_stats: bool,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let (meta, _key) = selector.resolve(&store)?;

    // Always-present fields first, then the optional ones in the order a reader
    // scans for them.
    let mut rows: Vec<(String, Value)> = vec![
        ("name".into(), json!(meta.name)),
        ("owner_id".into(), json!(meta.owner_id)),
        ("owner_type".into(), json!(meta.owner_type)),
        ("owner_category".into(), json!(meta.owner_category.as_str())),
        ("type".into(), json!(meta.time_series_type.as_str())),
        ("element_type".into(), json!(meta.element_type.to_string())),
        ("element_shape".into(), json!(meta.element_shape)),
    ];
    if let Some(r) = meta.resolution {
        rows.push(("resolution".into(), json!(parse::format_period(r))));
    }
    if let Some(t) = meta.initial_timestamp {
        rows.push(("initial_timestamp".into(), json!(t.to_rfc3339())));
    }
    if let Some(l) = meta.length {
        rows.push(("length".into(), json!(l)));
    }
    if let Some(h) = meta.horizon {
        rows.push(("horizon".into(), json!(parse::format_period(h))));
    }
    if let Some(iv) = meta.interval {
        rows.push(("interval".into(), json!(parse::format_period(iv))));
    }
    if let Some(c) = meta.count {
        rows.push(("count".into(), json!(c)));
    }
    if let Some(p) = &meta.percentiles {
        rows.push(("percentiles".into(), json!(p)));
    }
    if let Some(u) = &meta.units {
        rows.push(("units".into(), json!(u)));
    }
    if let Some(lt) = &meta.ext {
        rows.push(("ext".into(), json!(lt)));
    }
    rows.push(("features".into(), fields::features_json(&meta.features)));

    // The content hash and its physical location: everything needed to go and
    // look at the same bytes with h5dump/h5py, which the hash alone cannot do
    // because a packed array is one column of a shared, possibly spilled
    // dataset.
    rows.push(("data_hash".into(), json!(fields::hash_hex(&meta.data_hash))));
    match store.locate_array(&meta.data_hash) {
        Ok(loc) => {
            rows.push(("location".into(), json!(loc.to_string())));
            if let infrastore_core::ArrayLocation::Packed { dataset, column } = &loc {
                rows.push(("hdf5_dataset".into(), json!(dataset)));
                rows.push(("hdf5_column".into(), json!(column)));
            } else if let infrastore_core::ArrayLocation::Standalone { dataset } = &loc {
                rows.push(("hdf5_dataset".into(), json!(dataset)));
            }
        }
        // A catalog row whose array is missing is a real (and interesting)
        // state; report it rather than failing the whole command.
        Err(e) => rows.push(("location".into(), json!(format!("unavailable: {e}")))),
    }
    let (sts_refs, dst_refs) = store
        .count_array_references(&meta.data_hash)
        .map_err(|e| e.to_string())?;
    rows.push(("array_refs_single".into(), json!(sts_refs)));
    rows.push(("array_refs_dst".into(), json!(dst_refs)));

    if !no_stats {
        let data = store
            .get_time_series(&meta_key(&meta), None)
            .map_err(|e| e.to_string())?;
        let arr = data_array(&data);
        rows.push(("shape".into(), json!(arr.shape)));
        append_stats(arr, &mut rows);
    }

    match format {
        Format::Table => {
            let table = flat_rows(&rows, true);
            output::display_table_dyn(&field_value_header(), &table);
        }
        Format::Csv => {
            let table = flat_rows(&rows, false);
            output::display_csv_rows(&field_value_header(), &table)?;
        }
        Format::Json => {
            let obj: Map<String, Value> = rows.into_iter().collect();
            output::print_json(&Value::Object(obj))?;
        }
    }
    Ok(())
}

/// The field/value rows for the two-column views.
///
/// `features` is expanded into one `feature.<key>` row per entry rather than
/// printed as a JSON blob: this view is line-oriented and routinely grepped, and
/// one feature per line is what that wants. The JSON view keeps the nested
/// object, which is what a parser wants.
fn flat_rows(rows: &[(String, Value)], colorize: bool) -> Vec<Vec<String>> {
    let label = |k: &str| {
        if colorize {
            color::label(k)
        } else {
            k.to_string()
        }
    };
    let mut out = Vec::with_capacity(rows.len());
    for (k, v) in rows {
        match (k.as_str(), v) {
            ("features", Value::Object(map)) => {
                for (fk, fv) in map {
                    out.push(vec![label(&format!("feature.{fk}")), value_cell(fv)]);
                }
            }
            _ => out.push(vec![label(k), value_cell(v)]),
        }
    }
    out
}

/// Flatten a JSON value for a two-column table/CSV cell: strings unquoted,
/// everything else in its JSON spelling.
fn value_cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

fn meta_key(meta: &TimeSeriesMetadata) -> infrastore_core::KeyIdentity {
    crate::select::key_of(meta)
}

// --- rendering helpers -----------------------------------------------------

fn render_sequential(
    meta: &TimeSeriesMetadata,
    timestamps: &[String],
    arr: &TypedArray,
    format: Format,
    limit: Option<usize>,
    full: bool,
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
            // Every sequential CSV carries its timestamp column. A
            // SingleTimeSeries used to emit values only, on the grounds that its
            // grid is reconstructible from initial_timestamp + resolution — but
            // those live in the metadata, not in the file being piped, so
            // `get -f csv > out.csv` silently dropped the time axis.
            let mut header = vec!["timestamp".to_string()];
            header.extend(value_headers(per_step));
            let mut rows = Vec::with_capacity(length);
            for i in 0..length {
                let mut row = Vec::with_capacity(1 + per_step);
                row.push(timestamps.get(i).cloned().unwrap_or_default());
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
            obj.insert("values".into(), json!(csv_io::array_to_json_values(arr)));
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
            let (headers, rows) = forecast_csv_rows(meta, data)?;
            output::display_csv_rows(&headers, &rows)?;
        }
        Format::Json => {
            let mut obj = Map::new();
            meta_fields(meta, arr, &mut obj);
            if let TimeSeriesData::Scenarios(s) = data {
                obj.insert("scenario_count".into(), json!(s.scenario_count));
            }
            obj.insert("values".into(), json!(csv_io::array_to_json_values(arr)));
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

/// Timestamped CSV rows for a dense forecast: one row per (window, step) with
/// `issue_time` / `target_time`, and one value column per leading series
/// (percentile / scenario) x element entry. Shared by `get -f csv` and
/// `export`.
pub fn forecast_csv_rows(
    meta: &TimeSeriesMetadata,
    data: &TimeSeriesData,
) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let arr = data_array(data);
    let decoded = csv_io::array_to_strings(arr);
    let initial = meta
        .initial_timestamp
        .ok_or("forecast metadata is missing initial_timestamp")?;
    let resolution = meta
        .resolution
        .ok_or("forecast metadata is missing resolution")?;
    let interval = meta
        .interval
        .ok_or("forecast metadata is missing interval")?;

    // Leading series axis (percentiles / scenarios) and its column labels;
    // Deterministic has none.
    let (series_labels, h_axis) = match data {
        TimeSeriesData::Probabilistic(p) => (
            p.percentiles
                .iter()
                .map(|p| format!("p{p}"))
                .collect::<Vec<_>>(),
            1,
        ),
        TimeSeriesData::Scenarios(s) => {
            ((0..s.scenario_count).map(|i| format!("s{i}")).collect(), 1)
        }
        _ => (Vec::new(), 0),
    };
    let horizon_len = *arr
        .shape
        .get(h_axis)
        .ok_or_else(|| format!("unexpected forecast array shape {:?}", arr.shape))?;
    let count = *arr
        .shape
        .get(h_axis + 1)
        .ok_or_else(|| format!("unexpected forecast array shape {:?}", arr.shape))?;
    let per_step: usize = arr.shape[h_axis + 2..].iter().product::<usize>().max(1);
    let num_series = series_labels.len().max(1);

    let mut headers = vec!["issue_time".to_string(), "target_time".to_string()];
    if series_labels.is_empty() {
        headers.extend(value_headers(per_step));
    } else {
        for label in &series_labels {
            if per_step <= 1 {
                headers.push(format!("value[{label}]"));
            } else {
                for j in 0..per_step {
                    headers.push(format!("value[{label}][{j}]"));
                }
            }
        }
    }

    let mut rows = Vec::with_capacity(count * horizon_len);
    for c in 0..count {
        let issue = interval
            .add_to(initial, c as i64)
            .ok_or_else(|| format!("timestamp overflow at window {c}"))?;
        for h in 0..horizon_len {
            let target = resolution
                .add_to(issue, h as i64)
                .ok_or_else(|| format!("timestamp overflow at window {c} step {h}"))?;
            let mut row = vec![issue.to_rfc3339(), target.to_rfc3339()];
            for s in 0..num_series {
                for j in 0..per_step {
                    let idx = (((s * horizon_len + h) * count) + c) * per_step + j;
                    row.push(decoded[idx].clone());
                }
            }
            rows.push(row);
        }
    }
    Ok((headers, rows))
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
    if let Some(lt) = &meta.ext {
        obj.insert("ext".into(), json!(lt));
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
    obj.insert("features".into(), fields::features_json(&meta.features));
    obj.insert("data_hash".into(), json!(fields::hash_hex(&meta.data_hash)));
}

fn append_stats(arr: &TypedArray, rows: &mut Vec<(String, Value)>) {
    let vals = csv_io::array_to_f64_lossy(arr);
    if vals.is_empty() {
        return;
    }
    if arr.dtype == Dtype::Bool {
        let t = vals.iter().filter(|x| **x != 0.0).count();
        rows.push(("true_count".into(), json!(t)));
        rows.push(("false_count".into(), json!(vals.len() - t)));
    } else {
        let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
        let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        rows.push(("min".into(), json!(min)));
        rows.push(("max".into(), json!(max)));
        rows.push(("mean".into(), json!(mean)));
    }
    rows.push(("num_elements".into(), json!(vals.len())));
}

fn field_value_header() -> Vec<String> {
    vec!["field".to_string(), "value".to_string()]
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
