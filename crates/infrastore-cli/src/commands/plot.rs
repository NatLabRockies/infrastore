//! The `plot` command: write a self-contained SVG (or HTML) chart.
//!
//! Five views, each answering a question the numeric output cannot:
//!
//! * `line` — the profile itself, one or more series against time.
//! * `duration` — the load duration curve: values sorted descending against the
//!   fraction of time at or above them. Standard in this field, and the fastest
//!   read on how peaky a profile is.
//! * `heatmap` — time-of-day against day. The fastest way to spot a timezone or
//!   DST error, which is the bug class this data is most prone to: a correct
//!   profile shows vertical banding, a shifted one shows a diagonal seam.
//! * `fan` — percentile bands for a `Probabilistic`, overlaid traces for
//!   `Scenarios`. These types have no other readable rendering.
//! * `overlay` — a `Deterministic`'s windows drawn over the `SingleTimeSeries`
//!   it was transformed from: forecast against actual.

use std::path::Path;

use chrono::{DateTime, Timelike, Utc};
use infrastore_core::{
    ListFilter, Period, Store, TimeSeriesData, TimeSeriesMetadata, TimeSeriesType, TypedArray,
};

use crate::chart::{self, svg};
use crate::color;
use crate::csv_io;
use crate::output::{self, Format};
use crate::select::{self, SelectorArgs};
use crate::store_access;

/// Which view to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Kind {
    /// Values against time.
    #[default]
    Line,
    /// Values sorted descending against the fraction of time at or above them.
    Duration,
    /// Time-of-day against day.
    Heatmap,
    /// Percentile bands / scenario traces for one forecast window.
    Fan,
    /// A forecast's windows over the actuals it came from.
    Overlay,
}

/// Everything `plot` was asked to draw.
pub struct Options<'a> {
    pub kind: Kind,
    pub out: &'a Path,
    pub time_range: Option<&'a str>,
    pub title: Option<&'a str>,
    pub width: f64,
    pub height: f64,
    /// Which forecast window `fan` draws, and how many windows `overlay` shows.
    pub window: usize,
    pub limit: Option<usize>,
    /// Only shapes the "wrote it" line — the chart itself is always SVG.
    pub format: Format,
}

/// The smallest canvas worth rendering. Below this the margins alone consume the
/// whole document and the plot area clamps to a sliver, so a chart this size is
/// a mistake rather than a request.
const MIN_CANVAS: f64 = 50.0;

/// Reject a canvas dimension an SVG cannot express.
///
/// `width`/`height` are bare `f64`s that went straight into the root element's
/// `viewBox` and `width`/`height` attributes, so anything clap could parse
/// reached the file: `--width=-100` wrote `width="-100"`, which the SVG spec
/// makes an error, and `--width=nan` wrote `width="NaN"`, which is not a
/// `<length>` at all — `NaN` then leaked into the body geometry as well
/// (`x="NaN"`). Both reported success and exit 0, so a pipeline only found out
/// when something downstream refused to render the file.
fn check_canvas(value: f64, flag: &str) -> Result<(), String> {
    if !value.is_finite() {
        return Err(format!(
            "--{flag} must be a finite number of pixels, not {value}"
        ));
    }
    if value < MIN_CANVAS {
        return Err(format!(
            "--{flag} must be at least {MIN_CANVAS:.0} pixels, got {value}"
        ));
    }
    Ok(())
}

pub fn run(store_path: &Path, selector: &SelectorArgs, opts: &Options<'_>) -> Result<(), String> {
    check_canvas(opts.width, "width")?;
    check_canvas(opts.height, "height")?;
    let store = store_access::open_readonly(store_path)?;
    let range = crate::parse::parse_time_range(opts.time_range)?;
    let document = match opts.kind {
        Kind::Line => line(&store, selector, opts, range)?,
        Kind::Duration => duration(&store, selector, opts, range)?,
        Kind::Heatmap => heatmap(&store, selector, opts, range)?,
        Kind::Fan => fan(&store, selector, opts)?,
        Kind::Overlay => overlay(&store, selector, opts)?,
    };
    write_out(
        opts.out,
        opts.title.unwrap_or("infrastore"),
        &document,
        opts.format,
    )
}

/// Write the SVG, wrapping it in an HTML page when the destination asks for one.
fn write_out(out: &Path, title: &str, document: &str, format: Format) -> Result<(), String> {
    let is_html = out
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"));
    let body = if is_html {
        svg::as_html(title, document)
    } else {
        document.to_string()
    };
    // `-` puts the chart itself on stdout, so there is no room there for a
    // status line in any format.
    if out.as_os_str() == "-" {
        return output::write_raw(&body);
    }
    std::fs::write(out, body).map_err(|e| format!("writing {}: {e}", out.display()))?;
    output::report(
        format,
        || serde_json::json!({ "wrote": out.display().to_string() }),
        || println!("{}", color::header(&format!("Wrote {}.", out.display()))),
    )
}

// --- views ----------------------------------------------------------------

fn line(
    store: &Store,
    selector: &SelectorArgs,
    opts: &Options<'_>,
    range: Option<crate::parse::TimeRange>,
) -> Result<String, String> {
    let curves = static_curves(store, selector, range)?;
    let x_ticks = time_ticks(curves.iter().flat_map(|c| c.times.iter().copied()));
    let series = curves
        .iter()
        .enumerate()
        .map(|(i, c)| svg::Series {
            label: c.label.clone(),
            points: c.line_points(),
            slot: i,
            emphasis: false,
        })
        .collect();
    Ok(svg::Chart {
        title: opts.title.map(str::to_string).unwrap_or_else(|| {
            curves
                .first()
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "Time series".to_string())
        }),
        subtitle: subtitle(&curves),
        x_label: "Time (UTC)".to_string(),
        y_label: units(&curves),
        width: opts.width,
        height: opts.height,
        bands: Vec::new(),
        series,
        x_ticks,
    }
    .render())
}

fn duration(
    store: &Store,
    selector: &SelectorArgs,
    opts: &Options<'_>,
    range: Option<crate::parse::TimeRange>,
) -> Result<String, String> {
    let curves = static_curves(store, selector, range)?;
    let series = curves
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut sorted: Vec<f64> = c.values.iter().copied().filter(|v| v.is_finite()).collect();
            sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
            let n = sorted.len().max(1) as f64;
            svg::Series {
                label: c.label.clone(),
                points: sorted
                    .into_iter()
                    .enumerate()
                    .map(|(k, v)| (k as f64 / n * 100.0, v))
                    .collect(),
                slot: i,
                emphasis: false,
            }
        })
        .collect();
    Ok(svg::Chart {
        title: opts
            .title
            .map(str::to_string)
            .unwrap_or_else(|| "Load duration curve".to_string()),
        subtitle: subtitle(&curves),
        x_label: "Percent of time at or above (%)".to_string(),
        y_label: units(&curves),
        width: opts.width,
        height: opts.height,
        bands: Vec::new(),
        series,
        // Percentages, so the generated numeric ticks are already right.
        x_ticks: Vec::new(),
    }
    .render())
}

fn heatmap(
    store: &Store,
    selector: &SelectorArgs,
    opts: &Options<'_>,
    range: Option<crate::parse::TimeRange>,
) -> Result<String, String> {
    let mut curves = static_curves(store, selector, range)?;
    if curves.len() != 1 {
        return Err(format!(
            "a heatmap draws one series against the calendar; the selector matched {}. \
             Narrow it with --owner-id/--name.",
            curves.len()
        ));
    }
    let curve = curves.remove(0);
    let step = curve.resolution.ok_or(
        "a heatmap needs a regular resolution to lay out time-of-day; this series has none",
    )?;
    let step_secs = match step {
        Period::Fixed(d) => d.num_seconds(),
        Period::Months(_) => {
            return Err(format!(
                "a heatmap lays out time-of-day, which a calendar resolution ({}) has none of",
                step.to_iso8601()
            ));
        }
    };
    if step_secs <= 0 || 86_400i64 % step_secs != 0 {
        return Err(format!(
            "a heatmap needs a resolution that divides a day; {} does not",
            step.to_iso8601()
        ));
    }
    let rows = (86_400 / step_secs) as usize;

    let Some(first) = curve.times.first() else {
        return Err("the selected series has no values in the requested range".to_string());
    };
    let first_day = first.date_naive();
    let last_day = curve.times.last().unwrap_or(first).date_naive();
    let cols = ((last_day - first_day).num_days() + 1).max(1) as usize;

    let mut values = vec![vec![None; cols]; rows];
    for (t, v) in curve.times.iter().zip(&curve.values) {
        let col = (t.date_naive() - first_day).num_days();
        let row = (t.num_seconds_from_midnight() as i64) / step_secs;
        if let (Ok(col), Ok(row)) = (usize::try_from(col), usize::try_from(row))
            && row < rows
            && col < cols
            && v.is_finite()
        {
            values[row][col] = Some(*v);
        }
    }

    // Label the rows that fall on a whole hour, every third hour, so a
    // 5-minute series prints eight labels rather than 288.
    const HOUR_STRIDE: i64 = 3;
    let y_labels = (0..rows)
        .map(|r| {
            let secs = r as i64 * step_secs;
            (secs % (3600 * HOUR_STRIDE) == 0).then(|| format!("{:02}:00", secs / 3600))
        })
        .collect();

    let col_stride = cols.div_ceil(12).max(1);
    let x_labels = (0..cols)
        .map(|c| {
            c.is_multiple_of(col_stride).then(|| {
                (first_day + chrono::Duration::days(c as i64))
                    .format("%m-%d")
                    .to_string()
            })
        })
        .collect();

    Ok(svg::Heatmap {
        title: opts
            .title
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} by hour and day", curve.name)),
        subtitle: format!("{} · {} · {} days", curve.label, step.to_iso8601(), cols),
        x_label: "Day (UTC)".to_string(),
        y_label: "Time of day (UTC)".to_string(),
        x_labels,
        y_labels,
        values,
        width: opts.width,
        height: opts.height,
    }
    .render())
}

fn fan(store: &Store, selector: &SelectorArgs, opts: &Options<'_>) -> Result<String, String> {
    let (meta, key) = selector.resolve(store)?;
    let data = store
        .read_by_id(key, infrastore_core::ReadWindow::full())
        .map_err(|e| e.to_string())?;
    let (arr, leading, labels) = match &data {
        TimeSeriesData::Probabilistic(p) => (
            &p.data,
            p.percentiles.len(),
            p.percentiles
                .iter()
                .map(|v| format!("p{v}"))
                .collect::<Vec<_>>(),
        ),
        TimeSeriesData::Scenarios(s) => (
            &s.data,
            s.scenario_count,
            (0..s.scenario_count).map(|i| format!("s{i}")).collect(),
        ),
        other => {
            return Err(format!(
                "--kind fan draws the spread across a forecast's percentiles or scenarios; \
                 {} has none. Use --kind overlay for a Deterministic.",
                other.time_series_type().as_str()
            ));
        }
    };
    let window = Window::new(&meta, arr, 1, opts.window)?;
    let times = window.times()?;
    let x_ticks = time_ticks(times.iter().copied());
    let decoded = csv_io::array_to_f64_lossy(arr);

    let traces: Vec<Vec<f64>> = (0..leading).map(|s| window.trace(&decoded, s)).collect();

    let mut bands = Vec::new();
    let mut series = Vec::new();
    if matches!(data, TimeSeriesData::Probabilistic(_)) {
        // Symmetric percentile pairs, outermost first, so the bands nest. Every
        // band takes the *same* categorical slot: they measure one quantity at
        // different confidences, which is a sequential reading, and the
        // translucent fills darken where they overlap — so the middle of the
        // fan is visibly the likeliest without a second hue implying a second
        // series.
        let mut lo = 0usize;
        let mut hi = leading.saturating_sub(1);
        while lo < hi {
            bands.push(svg::Band {
                label: format!("{}–{}", labels[lo], labels[hi]),
                points: (0..window.horizon)
                    .map(|h| (millis(times[h]), traces[lo][h], traces[hi][h]))
                    .collect(),
                slot: 0,
            });
            lo += 1;
            hi -= 1;
        }
        if lo == hi {
            series.push(svg::Series {
                label: labels[lo].clone(),
                points: (0..window.horizon)
                    .map(|h| (millis(times[h]), traces[lo][h]))
                    .collect(),
                slot: 0,
                emphasis: true,
            });
        }
    } else {
        // Spaghetti. Past the palette's slot count there is no honest per-trace
        // color, so every trace shares one and the legend says how many.
        let distinct = leading <= chart::MAX_SERIES;
        for (s, trace) in traces.iter().enumerate() {
            series.push(svg::Series {
                label: if distinct {
                    labels[s].clone()
                } else if s == 0 {
                    format!("{leading} scenarios")
                } else {
                    String::new()
                },
                points: (0..window.horizon)
                    .map(|h| (millis(times[h]), trace[h]))
                    .collect(),
                slot: if distinct { s } else { 0 },
                emphasis: false,
            });
        }
    }

    Ok(svg::Chart {
        title: opts
            .title
            .map(str::to_string)
            .unwrap_or_else(|| meta.name.clone()),
        subtitle: format!(
            "{} · owner {} · window {} issued {}",
            meta.time_series_type.as_str(),
            meta.owner_id,
            opts.window,
            window.issue.to_rfc3339()
        ),
        x_label: "Target time (UTC)".to_string(),
        y_label: meta.units.clone().unwrap_or_default(),
        width: opts.width,
        height: opts.height,
        bands,
        series,
        x_ticks,
    }
    .render())
}

fn overlay(store: &Store, selector: &SelectorArgs, opts: &Options<'_>) -> Result<String, String> {
    let (meta, key) = selector.resolve(store)?;
    if !matches!(
        meta.time_series_type,
        TimeSeriesType::Deterministic | TimeSeriesType::DeterministicSingleTimeSeries
    ) {
        return Err(format!(
            "--kind overlay draws a Deterministic against its source SingleTimeSeries; \
             the selector resolved a {}",
            meta.time_series_type.as_str()
        ));
    }
    let data = store
        .read_by_id(key, infrastore_core::ReadWindow::full())
        .map_err(|e| e.to_string())?;
    let arr = match &data {
        TimeSeriesData::Deterministic(d) => &d.data,
        other => {
            return Err(format!(
                "expected a Deterministic array, got {}",
                other.time_series_type().as_str()
            ));
        }
    };
    let decoded = csv_io::array_to_f64_lossy(arr);

    let mut series = Vec::new();
    // The actuals first, so they are slot 0 and drawn under the windows.
    let mut actual_filter = ListFilter::new()
        .owner_id(meta.owner_id)
        .owner_category(meta.owner_category)
        .name(meta.name.clone())
        .time_series_type(TimeSeriesType::SingleTimeSeries)
        .features(meta.features.clone());
    if let Some(r) = meta.resolution {
        actual_filter = actual_filter.resolution(r);
    }
    let actuals = store
        .list_metadata(actual_filter)
        .map_err(|e| e.to_string())?;
    if let Some(source) = actuals.first() {
        let curve = read_curve(store, source, None)?;
        series.push(svg::Series {
            label: "actual".to_string(),
            points: curve
                .times
                .iter()
                .zip(&curve.values)
                .map(|(t, v)| (millis(*t), *v))
                .collect(),
            slot: 0,
            emphasis: true,
        });
    }

    let count = meta.count.unwrap_or(0);
    let shown = opts.limit.unwrap_or(8).min(count);
    for c in opts.window..(opts.window + shown).min(count) {
        let window = Window::new(&meta, arr, 0, c)?;
        let times = window.times()?;
        let trace = window.trace(&decoded, 0);
        series.push(svg::Series {
            label: if c == opts.window {
                "forecast windows".to_string()
            } else {
                String::new()
            },
            points: (0..window.horizon)
                .map(|h| (millis(times[h]), trace[h]))
                .collect(),
            slot: 1,
            emphasis: false,
        });
    }
    if series.is_empty() {
        return Err("nothing to draw: the forecast has no windows".to_string());
    }
    let x_ticks = time_ticks(
        series
            .iter()
            .flat_map(|s| s.points.iter().map(|(x, _)| from_millis(*x))),
    );

    Ok(svg::Chart {
        title: opts
            .title
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} — forecast vs actual", meta.name)),
        subtitle: format!(
            "{} · owner {} · windows {}..{}{}",
            meta.time_series_type.as_str(),
            meta.owner_id,
            opts.window,
            (opts.window + shown).min(count),
            if actuals.is_empty() {
                " · no source SingleTimeSeries found"
            } else {
                ""
            }
        ),
        x_label: "Time (UTC)".to_string(),
        y_label: meta.units.clone().unwrap_or_default(),
        width: opts.width,
        height: opts.height,
        bands: Vec::new(),
        series,
        x_ticks,
    }
    .render())
}

// --- shared data plumbing --------------------------------------------------

/// One decoded static series, ready to plot.
struct Curve {
    label: String,
    name: String,
    owner_id: i64,
    units: Option<String>,
    resolution: Option<Period>,
    times: Vec<DateTime<Utc>>,
    values: Vec<f64>,
    /// Whether the samples are a step function -- true only for a
    /// `PersistentTimeSeries`. See [`Curve::line_points`].
    ///
    /// Only `line` consumes it, because only `line` draws anything *between*
    /// two samples. `duration` sorts the values and `heatmap` places one cell
    /// per sample; neither interpolates, so neither can misreport the shape.
    /// (A duration curve over a step function arguably wants time weighting
    /// rather than one slot per breakpoint, but that is a different chart, not
    /// a different path through this one.)
    step: bool,
}

impl Curve {
    /// The polyline to draw through this curve's samples.
    ///
    /// A step function is emitted as a **stair**: each value is carried at its
    /// own level to the next breakpoint before the line drops or climbs to the
    /// new one. Joining the breakpoints directly would draw a ramp between them
    /// and so put a value on the chart at every instant in between -- which is
    /// exactly the reading `PersistentTimeSeries` exists to rule out. The
    /// stair ends at the last breakpoint rather than running to the right edge:
    /// the value is held forward forever, and the chart should not imply an
    /// expiry the store does not record.
    fn line_points(&self) -> Vec<(f64, f64)> {
        let pairs = self.times.iter().zip(&self.values);
        if !self.step {
            return pairs.map(|(t, v)| (millis(*t), *v)).collect();
        }
        let mut points = Vec::with_capacity(self.times.len() * 2);
        for (i, (t, v)) in pairs.enumerate() {
            points.push((millis(*t), *v));
            // The corner: hold this level up to the next breakpoint, where the
            // point pushed on the following turn supplies the vertical.
            if let Some(next) = self.times.get(i + 1) {
                points.push((millis(*next), *v));
            }
        }
        points
    }
}

/// Every static series the selector matched, decoded and time-stamped.
fn static_curves(
    store: &Store,
    selector: &SelectorArgs,
    range: Option<crate::parse::TimeRange>,
) -> Result<Vec<Curve>, String> {
    let metas = store
        .list_metadata(selector.to_filter()?)
        .map_err(|e| e.to_string())?;
    let metas: Vec<TimeSeriesMetadata> = metas
        .into_iter()
        .filter(|m| {
            matches!(
                m.time_series_type,
                TimeSeriesType::SingleTimeSeries
                    | TimeSeriesType::NonSequentialTimeSeries
                    | TimeSeriesType::PersistentTimeSeries
            )
        })
        .collect();
    if metas.is_empty() {
        return Err(
            "no static time series matched the selector (use --kind fan or --kind overlay \
             for a forecast)"
                .to_string(),
        );
    }
    if metas.len() > chart::MAX_SERIES {
        return Err(format!(
            "{} series matched; a categorical chart has {} distinguishable colors. \
             Narrow the selector, or draw the whole set with `infrastore grid`.",
            metas.len(),
            chart::MAX_SERIES
        ));
    }
    metas.iter().map(|m| read_curve(store, m, range)).collect()
}

fn read_curve(
    store: &Store,
    meta: &TimeSeriesMetadata,
    range: Option<crate::parse::TimeRange>,
) -> Result<Curve, String> {
    let id = select::id_of(meta)?;
    let data = match range {
        Some(r) => store.read_by_ids_range(&[id], r).map(|mut v| v.remove(0)),
        None => store.read_by_id(id, infrastore_core::ReadWindow::full()),
    }
    .map_err(|e| e.to_string())?;
    let (times, arr) = match &data {
        TimeSeriesData::SingleTimeSeries(s) => {
            let times = (0..s.length)
                .map(|i| {
                    s.resolution
                        .add_to(s.initial_timestamp, i as i64)
                        .ok_or_else(|| format!("timestamp overflow at grid index {i}"))
                })
                .collect::<Result<Vec<_>, String>>()?;
            (times, &s.data)
        }
        TimeSeriesData::NonSequentialTimeSeries(ns) => (ns.timestamps.clone(), &ns.data),
        TimeSeriesData::PersistentTimeSeries(p) => (p.timestamps.clone(), &p.data),
        other => {
            return Err(format!(
                "{} is not a static series",
                other.time_series_type().as_str()
            ));
        }
    };
    // Only the first element of a multidimensional timestep is drawn: a line
    // chart has one value per instant, and silently summing or averaging the
    // rest would invent a number the store does not hold.
    let per_step = arr.element_shape().iter().product::<usize>().max(1);
    let decoded = csv_io::array_to_f64_lossy(arr);
    let values = (0..times.len())
        .map(|i| decoded.get(i * per_step).copied().unwrap_or(f64::NAN))
        .collect();
    Ok(Curve {
        label: format!("{}@{}", meta.name, meta.owner_id),
        name: meta.name.clone(),
        owner_id: meta.owner_id,
        units: meta.units.clone(),
        resolution: meta.resolution,
        times,
        values,
        step: meta.time_series_type == TimeSeriesType::PersistentTimeSeries,
    })
}

/// One window of a dense forecast array, shaped `[*leading, H, count, *E]`.
struct Window {
    horizon: usize,
    count: usize,
    per_step: usize,
    index: usize,
    issue: DateTime<Utc>,
    resolution: Period,
}

impl Window {
    fn new(
        meta: &TimeSeriesMetadata,
        arr: &TypedArray,
        leading_axes: usize,
        index: usize,
    ) -> Result<Self, String> {
        let horizon = *arr
            .shape
            .get(leading_axes)
            .ok_or_else(|| format!("unexpected forecast array shape {:?}", arr.shape))?;
        let count = *arr
            .shape
            .get(leading_axes + 1)
            .ok_or_else(|| format!("unexpected forecast array shape {:?}", arr.shape))?;
        if index >= count {
            return Err(format!(
                "--window {index} is out of range: this forecast has {count} windows (0..{})",
                count - 1
            ));
        }
        let interval = meta
            .interval
            .ok_or("forecast metadata is missing interval")?;
        let initial = meta
            .initial_timestamp
            .ok_or("forecast metadata is missing initial_timestamp")?;
        Ok(Self {
            horizon,
            count,
            per_step: arr.shape[leading_axes + 2..]
                .iter()
                .product::<usize>()
                .max(1),
            index,
            issue: interval
                .add_to(initial, index as i64)
                .ok_or_else(|| format!("timestamp overflow at window {index}"))?,
            resolution: meta
                .resolution
                .ok_or("forecast metadata is missing resolution")?,
        })
    }

    /// The target timestamps of this window.
    fn times(&self) -> Result<Vec<DateTime<Utc>>, String> {
        (0..self.horizon)
            .map(|h| {
                self.resolution
                    .add_to(self.issue, h as i64)
                    .ok_or_else(|| format!("timestamp overflow at window {} step {h}", self.index))
            })
            .collect()
    }

    /// The `horizon`-long trace for leading-axis entry `s`, first element only.
    fn trace(&self, decoded: &[f64], s: usize) -> Vec<f64> {
        (0..self.horizon)
            .map(|h| {
                let idx = (((s * self.horizon + h) * self.count) + self.index) * self.per_step;
                decoded.get(idx).copied().unwrap_or(f64::NAN)
            })
            .collect()
    }
}

fn millis(t: DateTime<Utc>) -> f64 {
    t.timestamp_millis() as f64
}

fn from_millis(ms: f64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms as i64).unwrap_or_else(Utc::now)
}

/// About six evenly spaced ticks across a time span, formatted for its width.
fn time_ticks(times: impl Iterator<Item = DateTime<Utc>>) -> Vec<(f64, String)> {
    let mut all: Vec<DateTime<Utc>> = times.collect();
    if all.is_empty() {
        return Vec::new();
    }
    all.sort_unstable();
    all.dedup();
    let span = *all.last().unwrap() - *all.first().unwrap();
    // Below three days a reader needs the hour; above it the date is what
    // distinguishes one tick from the next and the hour is noise.
    let format = if span < chrono::Duration::days(3) {
        "%m-%d %H:%M"
    } else {
        "%Y-%m-%d"
    };
    let n = all.len();
    let step = n.div_ceil(6).max(1);
    (0..n)
        .step_by(step)
        .map(|i| (millis(all[i]), all[i].format(format).to_string()))
        .collect()
}

/// A shared units label, or nothing when the series disagree.
fn units(curves: &[Curve]) -> String {
    let first = curves.first().and_then(|c| c.units.clone());
    if curves.iter().all(|c| c.units == first) {
        first.unwrap_or_default()
    } else {
        String::new()
    }
}

fn subtitle(curves: &[Curve]) -> String {
    match curves.len() {
        1 => format!(
            "owner {} · {} points",
            curves[0].owner_id,
            curves[0].values.len()
        ),
        n => format!("{n} series"),
    }
}
