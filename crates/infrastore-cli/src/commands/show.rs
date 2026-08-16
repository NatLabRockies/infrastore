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
        f if f.is_json() => {
            let items: Vec<Value> = metas.iter().map(list_json).collect();
            output::print_items(f, &items)?;
        }
        Format::Csv => {
            let headers = list_headers(wide);
            let rows: Vec<Vec<String>> = metas.iter().map(|m| list_row(m, wide)).collect();
            output::display_csv_rows(&headers, &rows)?;
        }
        _ => {
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
    "Quantity Kind",
    "Unit System",
    "Component Field",
    "Application Data",
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
            m.quantity_kind.clone().unwrap_or_else(|| "-".to_string()),
            m.unit_system
                .map(|u| u.as_str().to_string())
                .unwrap_or_else(|| "-".to_string()),
            m.component_field.clone().unwrap_or_else(|| "-".to_string()),
            m.application_data
                .clone()
                .unwrap_or_else(|| "-".to_string()),
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
    obj.insert("quantity_kind".into(), json!(m.quantity_kind));
    obj.insert(
        "unit_system".into(),
        json!(m.unit_system.map(|u| u.as_str())),
    );
    obj.insert("component_field".into(), json!(m.component_field));
    obj.insert("application_data".into(), json!(m.application_data));
    Value::Object(obj)
}

/// How many rows a view shows, and from which end.
///
/// The three flags do two different jobs, and the split is deliberate.
///
/// `--stride` *selects data*: "every 24th row" is a different series, not a
/// shorter view of the same one, so it applies in every format — a `-f csv`
/// pipe of daily samples is exactly what someone asks for with it.
///
/// `--limit` / `--full` / `--tail` *bound a display*, and apply to the table
/// only. A CSV or JSON stream is consumed by another program, and a silently
/// short one is a data bug in whatever reads it; that is why the table's
/// default 50-row cap has never applied there, and why an explicit bound does
/// not either. Thin a pipe with `--stride`, or slice it with `--time-range`.
///
/// Order matters within the display half: striding first and then taking from
/// an end means `--stride 24 --tail 7` reads as "the last seven daily samples",
/// which is what someone typing it means.
#[derive(Debug, Clone, Copy, Default)]
pub struct RowWindow {
    pub limit: Option<usize>,
    pub full: bool,
    pub tail: bool,
    pub stride: Option<usize>,
}

impl RowWindow {
    /// Which of `len` rows this window keeps, and how many the display bound
    /// dropped.
    ///
    /// Returns the selection rather than the rows: a caller that only needs
    /// fifty rows of a million-row series has no reason to build the other
    /// 999,950 first, and [`Selection`] is the arithmetic that lets it build
    /// only what it prints.
    fn select(&self, len: usize, format: Format) -> Result<(Selection, usize), String> {
        let step = match self.stride {
            Some(0) => return Err("--stride must be at least 1".to_string()),
            Some(n) => n,
            None => 1,
        };
        let strided = len.div_ceil(step);
        if format != Format::Table {
            return Ok((Selection::new(0, step, strided), 0));
        }
        let max = match (self.full, self.limit) {
            (true, _) => strided,
            (_, Some(n)) => n,
            (_, None) => DEFAULT_LIMIT,
        };
        let shown = strided.min(max);
        let dropped = strided - shown;
        let first = if self.tail { strided - shown } else { 0 };
        Ok((Selection::new(first * step, step, shown), dropped))
    }

    /// Apply the window to rows that are already built, returning the kept rows
    /// and the number that were dropped.
    fn apply<T: Clone>(&self, rows: &[T], format: Format) -> Result<(Vec<T>, usize), String> {
        let (sel, dropped) = self.select(rows.len(), format)?;
        Ok((sel.iter().map(|i| rows[i].clone()).collect(), dropped))
    }
}

/// An arithmetic run of row indices: `count` rows starting at `start`, `step`
/// apart. The output of [`RowWindow::select`].
#[derive(Debug, Clone, Copy)]
struct Selection {
    start: usize,
    step: usize,
    count: usize,
}

impl Selection {
    fn new(start: usize, step: usize, count: usize) -> Self {
        Self { start, step, count }
    }

    fn iter(self) -> impl Iterator<Item = usize> {
        (0..self.count).map(move |k| self.start + k * self.step)
    }

    fn len(self) -> usize {
        self.count
    }
}

/// Everything `get` was asked for beyond the selector.
pub struct GetOptions<'a> {
    pub time_range: Option<&'a str>,
    pub rows: RowWindow,
    /// Draw a terminal sparkline instead of the value rows.
    pub plot: bool,
    pub plot_width: Option<usize>,
    /// Restrict a forecast to one window, by index or by issue time.
    pub window: Option<usize>,
    pub issue_time: Option<&'a str>,
}

/// `get`: read a single series and render its values.
pub fn get(
    store_path: &Path,
    selector: &SelectorArgs,
    opts: &GetOptions<'_>,
    format: Format,
) -> Result<(), String> {
    let store = store_access::open_readonly(store_path)?;
    let (meta, key) = selector.resolve(&store)?;
    let range = parse::parse_time_range(opts.time_range)?;
    let data = store
        .get_time_series(&key, range)
        .map_err(|e| e.to_string())?;

    if opts.plot {
        return render_plot(&meta, &data, opts.plot_width);
    }

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
            reject_forecast_flags(opts, &meta)?;
            render_sequential(&meta, &ts, &s.data, format, opts.rows)
        }
        TimeSeriesData::NonSequentialTimeSeries(ns) => {
            let ts: Vec<String> = ns.timestamps.iter().map(|t| t.to_rfc3339()).collect();
            reject_forecast_flags(opts, &meta)?;
            render_sequential(&meta, &ts, &ns.data, format, opts.rows)
        }
        _ => {
            let window = resolve_window(&meta, opts)?;
            render_forecast(&meta, &data, format, opts.rows, window)
        }
    }
}

fn reject_forecast_flags(opts: &GetOptions<'_>, meta: &TimeSeriesMetadata) -> Result<(), String> {
    if opts.window.is_some() || opts.issue_time.is_some() {
        return Err(format!(
            "--window/--issue-time select one window of a forecast; '{}' is a {}",
            meta.name,
            meta.time_series_type.as_str()
        ));
    }
    Ok(())
}

/// The forecast window index `--window` / `--issue-time` names, if either was
/// given.
///
/// `--issue-time` is resolved against the stored `initial_timestamp` and
/// `interval` rather than searched for: a timestamp that is not exactly a window
/// boundary is a mistake worth reporting, not one to round away.
fn resolve_window(
    meta: &TimeSeriesMetadata,
    opts: &GetOptions<'_>,
) -> Result<Option<usize>, String> {
    match (opts.window, opts.issue_time) {
        (Some(_), Some(_)) => Err("--window and --issue-time both name a window; use one".into()),
        (Some(w), None) => Ok(Some(w)),
        (None, Some(spec)) => {
            let wanted = parse::parse_timestamp(spec)?;
            let initial = meta
                .initial_timestamp
                .ok_or("forecast metadata is missing initial_timestamp")?;
            let interval = meta
                .interval
                .ok_or("forecast metadata is missing interval")?;
            let count = meta.count.unwrap_or(0);
            for c in 0..count {
                match interval.add_to(initial, c as i64) {
                    Some(t) if t == wanted => return Ok(Some(c)),
                    _ => continue,
                }
            }
            Err(format!(
                "no window is issued at {}; this forecast has {count} windows starting at {} \
                 every {}",
                wanted.to_rfc3339(),
                initial.to_rfc3339(),
                interval.to_iso8601()
            ))
        }
        (None, None) => Ok(None),
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
    if let Some(q) = &meta.quantity_kind {
        rows.push(("quantity_kind".into(), json!(q)));
    }
    if let Some(u) = meta.unit_system {
        rows.push(("unit_system".into(), json!(u.as_str())));
    }
    if let Some(c) = &meta.component_field {
        rows.push(("component_field".into(), json!(c)));
    }
    if let Some(lt) = &meta.application_data {
        rows.push(("application_data".into(), json!(lt)));
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
        f if f.is_json() => {
            let obj: Map<String, Value> = rows.into_iter().collect();
            output::print_value(f, &Value::Object(obj))?;
        }
        Format::Csv => {
            let table = flat_rows(&rows, false);
            output::display_csv_rows(&field_value_header(), &table)?;
        }
        _ => {
            let table = flat_rows(&rows, true);
            output::display_table_dyn(&field_value_header(), &table);
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
    window: RowWindow,
) -> Result<(), String> {
    let per_step = arr.element_shape().iter().product::<usize>().max(1);
    let length = arr.length();
    // Select the rows before decoding any of them: a table showing fifty rows of
    // a year of five-minute data would otherwise stringify all 105,120 first.
    let (sel, dropped) = window.select(length, format)?;
    let elem = arr.dtype.size();
    let row_bytes = |i: usize| {
        arr.bytes
            .get(i * per_step * elem..(i + 1) * per_step * elem)
            .unwrap_or(&[])
    };
    let timestamp_at = |i: usize| timestamps.get(i).cloned().unwrap_or_default();

    match format {
        f if f.is_json() => {
            let mut obj = Map::new();
            meta_fields(meta, arr, &mut obj);
            // `--stride` selects data rather than bounding a display, so the
            // emitted grid really is shorter than the stored one — and the shape
            // printed beside it has to say so instead of describing an array
            // this document does not contain.
            if sel.len() != length {
                let mut shape = arr.shape.clone();
                if let Some(rows) = shape.first_mut() {
                    *rows = sel.len();
                }
                obj.insert("shape".into(), json!(shape));
            }
            if sel.step > 1 {
                obj.insert("stride".into(), json!(sel.step));
            }
            let ts: Vec<String> = sel.iter().map(timestamp_at).collect();
            let values: Vec<Value> = sel
                .iter()
                .flat_map(|i| csv_io::bytes_to_json_values(arr.dtype, row_bytes(i)))
                .collect();
            obj.insert("timestamps".into(), json!(ts));
            obj.insert("values".into(), json!(values));
            output::print_value(f, &Value::Object(obj))?;
        }
        // Every sequential CSV carries its timestamp column. A SingleTimeSeries
        // used to emit values only, on the grounds that its grid is
        // reconstructible from initial_timestamp + resolution — but those live
        // in the metadata, not in the file being piped, so `get -f csv >
        // out.csv` silently dropped the time axis.
        _ => {
            let mut header = vec!["timestamp".to_string()];
            header.extend(value_headers(per_step));
            let rows: Vec<Vec<String>> = sel
                .iter()
                .map(|i| {
                    let mut row = Vec::with_capacity(1 + per_step);
                    row.push(timestamp_at(i));
                    row.extend(csv_io::bytes_to_strings(arr.dtype, row_bytes(i)));
                    row
                })
                .collect();
            if format == Format::Csv {
                output::display_csv_rows(&header, &rows)?;
            } else {
                output::display_table_dyn(&header, &rows);
                report_dropped(dropped);
            }
        }
    }
    Ok(())
}

/// A dense forecast, rendered as the structured view in every format.
///
/// The table used to print `index,value` over the row-major flattening and
/// point at `-f csv` for anything readable — but [`forecast_csv_rows`] was
/// already computing the good view for that flag, so the table now uses it too:
/// `issue_time`, `target_time`, and one column per percentile / scenario.
fn render_forecast(
    meta: &TimeSeriesMetadata,
    data: &TimeSeriesData,
    format: Format,
    rows_window: RowWindow,
    window: Option<usize>,
) -> Result<(), String> {
    let arr = data_array(data);
    let (headers, mut rows) = forecast_csv_rows(meta, data)?;

    // One window is a contiguous run of `horizon` rows, because
    // `forecast_csv_rows` emits window-major.
    if let Some(c) = window {
        let count = meta.count.unwrap_or(0);
        if c >= count {
            return Err(format!(
                "--window {c} is out of range: this forecast has {count} windows (0..{})",
                count.saturating_sub(1)
            ));
        }
        let horizon = rows.len() / count.max(1);
        rows = rows[c * horizon..(c + 1) * horizon].to_vec();
    }

    match format {
        f if f.is_json() => {
            let mut obj = Map::new();
            meta_fields(meta, arr, &mut obj);
            if let TimeSeriesData::Scenarios(s) = data {
                obj.insert("scenario_count".into(), json!(s.scenario_count));
            }
            let stride = rows_window.stride.filter(|&n| n > 1);
            // A window slice keeps the readable shape rather than the stored
            // one: the flat array's index arithmetic is exactly what a caller
            // asking for one window does not want to redo. A stride is the same
            // argument — the rows kept are no longer the stored array, so
            // emitting that array's flat values would answer a different
            // question than the one asked.
            if window.is_some() || stride.is_some() {
                let (shown, _) = rows_window.apply(&rows, format)?;
                if let Some(c) = window {
                    obj.insert("window".into(), json!(c));
                }
                if let Some(n) = stride {
                    obj.insert("stride".into(), json!(n));
                }
                obj.insert("columns".into(), json!(headers));
                obj.insert("rows".into(), json!(shown));
            } else {
                obj.insert("values".into(), json!(csv_io::array_to_json_values(arr)));
            }
            output::print_value(f, &Value::Object(obj))?;
        }
        Format::Csv => {
            let (shown, _) = rows_window.apply(&rows, format)?;
            output::display_csv_rows(&headers, &shown)?;
        }
        _ => {
            let (shown, dropped) = rows_window.apply(&rows, format)?;
            output::display_table_dyn(&headers, &shown);
            report_dropped(dropped);
        }
    }
    Ok(())
}

fn report_dropped(dropped: usize) {
    if dropped > 0 {
        println!(
            "{}",
            color::dim(&format!(
                "... {dropped} more rows (use --full, --limit, or --tail)"
            ))
        );
    }
}

/// `get --plot`: one sparkline per element of the series.
fn render_plot(
    meta: &TimeSeriesMetadata,
    data: &TimeSeriesData,
    width: Option<usize>,
) -> Result<(), String> {
    let arr = data_array(data);
    let decoded = csv_io::array_to_f64_lossy(arr);
    let per_step = arr.element_shape().iter().product::<usize>().max(1);
    // A forecast has no single time axis, so its whole flattened array is drawn
    // as one trace — enough to see whether the numbers are plausible, which is
    // all a sparkline claims. `infrastore plot --kind fan` draws the structure.
    let steps = if meta.time_series_type.is_forecast() {
        decoded.len()
    } else {
        arr.length()
    };
    let elements = if meta.time_series_type.is_forecast() {
        1
    } else {
        per_step
    };
    let width = width.unwrap_or_else(crate::chart::spark::terminal_width);

    println!(
        "{}",
        color::header(&format!(
            "{} '{}' (owner {}) — {} values{}",
            meta.time_series_type.as_str(),
            meta.name,
            meta.owner_id,
            decoded.len(),
            meta.units
                .as_ref()
                .map(|u| format!(", {u}"))
                .unwrap_or_default(),
        ))
    );
    for e in 0..elements {
        let values: Vec<f64> = (0..steps)
            .map(|i| decoded.get(i * elements + e).copied().unwrap_or(f64::NAN))
            .collect();
        let s = crate::chart::spark::render(&values, width);
        let label = if elements > 1 {
            format!("[{e}] ")
        } else {
            String::new()
        };
        println!(
            "{label}{} {}",
            s.line,
            color::dim(&format!(
                "min {} max {}{}",
                crate::chart::fmt_num(s.min),
                crate::chart::fmt_num(s.max),
                if s.non_finite > 0 {
                    format!(" ({} non-finite)", s.non_finite)
                } else {
                    String::new()
                }
            ))
        );
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
    if let Some(q) = &meta.quantity_kind {
        obj.insert("quantity_kind".into(), json!(q));
    }
    if let Some(u) = meta.unit_system {
        obj.insert("unit_system".into(), json!(u.as_str()));
    }
    if let Some(c) = &meta.component_field {
        obj.insert("component_field".into(), json!(c));
    }
    if let Some(lt) = &meta.application_data {
        obj.insert("application_data".into(), json!(lt));
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

/// Numeric stats over the decoded array.
///
/// The array is already in memory as `f64` by the time this runs, so the
/// distribution shape — spread, quartiles, tails — costs one sort on top of the
/// pass that computed min/max/mean, and it is what tells a plausible profile
/// from a broken one. `non_finite` is reported separately rather than folded in:
/// a NaN in a load profile is a data bug, and a mean that quietly ignored it
/// would hide the bug rather than surface it.
fn append_stats(arr: &TypedArray, rows: &mut Vec<(String, Value)>) {
    let vals = csv_io::array_to_f64_lossy(arr);
    if vals.is_empty() {
        return;
    }
    if arr.dtype == Dtype::Bool {
        let t = vals.iter().filter(|x| **x != 0.0).count();
        rows.push(("true_count".into(), json!(t)));
        rows.push(("false_count".into(), json!(vals.len() - t)));
        rows.push(("num_elements".into(), json!(vals.len())));
        return;
    }

    let finite: Vec<f64> = vals.iter().copied().filter(|v| v.is_finite()).collect();
    let non_finite = vals.len() - finite.len();
    rows.push(("num_elements".into(), json!(vals.len())));
    rows.push(("non_finite".into(), json!(non_finite)));
    rows.push(("first".into(), json!(finite_json(vals.first().copied()))));
    rows.push(("last".into(), json!(finite_json(vals.last().copied()))));
    if finite.is_empty() {
        return;
    }

    let n = finite.len() as f64;
    let mean = finite.iter().sum::<f64>() / n;
    // Sample standard deviation (n-1). A single value has no spread to report.
    let stddev = if finite.len() > 1 {
        (finite.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
    } else {
        0.0
    };
    let mut sorted = finite;
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    rows.push(("min".into(), json!(sorted[0])));
    rows.push(("max".into(), json!(sorted[sorted.len() - 1])));
    rows.push(("mean".into(), json!(mean)));
    rows.push(("stddev".into(), json!(stddev)));
    for p in [5u32, 25, 50, 75, 95] {
        rows.push((format!("p{p}"), json!(percentile(&sorted, f64::from(p)))));
    }
}

/// The `p`-th percentile of an ascending slice, by linear interpolation between
/// neighbours (the "inclusive" definition NumPy and Excel both default to).
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (p / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64)
    }
}

/// A value JSON can carry, or `null` for the non-finite ones it cannot spell.
fn finite_json(v: Option<f64>) -> Value {
    match v {
        Some(v) if v.is_finite() => json!(v),
        _ => Value::Null,
    }
}

fn field_value_header() -> Vec<String> {
    vec!["field".to_string(), "value".to_string()]
}
