//! A self-contained SVG backend: line/area charts and a heatmap.
//!
//! The output is one file with no external references — no fonts, no scripts,
//! no stylesheet — so it opens in a browser, drops into a report, and survives
//! being emailed. Both themes are written into a single `<style>` block keyed on
//! `prefers-color-scheme`, because a chart that is only legible on a white page
//! is half a chart.

use std::fmt::Write as _;

use super::{
    AXIS, GRIDLINE, INK_MUTED, INK_PRIMARY, INK_SECONDARY, SEQUENTIAL, SERIES_DARK, SERIES_LIGHT,
    SURFACE, fmt_num, nice_ticks, xml_escape,
};

/// Plot-area padding, in user units.
///
/// The top margin stacks three rows above the plot — title, subtitle, legend —
/// rather than overlapping them, which is what [`TITLE_Y`], [`SUBTITLE_Y`], and
/// [`LEGEND_Y`] below space out.
const MARGIN_LEFT: f64 = 68.0;
const MARGIN_TOP: f64 = 78.0;
const MARGIN_BOTTOM: f64 = 56.0;

/// Text baselines for the header rows.
const TITLE_Y: f64 = 24.0;
const SUBTITLE_Y: f64 = 42.0;
const LEGEND_Y: f64 = 64.0;
/// Right margin without direct labels. [`Chart::render`] widens it when it has
/// labels to place outside the plot area.
const MARGIN_RIGHT: f64 = 24.0;

/// At or below this many series, each line is also labelled at its right end.
///
/// The palette's contrast relief: several light-mode hues sit under 3:1 against
/// the surface, so identity must not rest on the swatch alone. Above the
/// threshold the labels would collide with each other and the legend carries it.
const DIRECT_LABEL_MAX: usize = 4;

/// One polyline.
pub struct Series {
    pub label: String,
    /// `(x, y)` in domain units. A non-finite `y` breaks the line rather than
    /// interpolating across the gap.
    pub points: Vec<(f64, f64)>,
    /// Categorical slot. Assigned by position and never cycled.
    pub slot: usize,
    /// Draw thicker and label it even past [`DIRECT_LABEL_MAX`]: the actuals a
    /// forecast is overlaid on, which is the one line a reader is comparing to.
    pub emphasis: bool,
}

/// A filled range between two bounds, for percentile fans.
pub struct Band {
    pub label: String,
    /// `(x, lower, upper)` in domain units.
    pub points: Vec<(f64, f64, f64)>,
    pub slot: usize,
}

/// An x/y chart. Ticks are supplied by the caller for a time axis (which knows
/// how to format an instant) and generated numerically otherwise.
pub struct Chart {
    pub title: String,
    pub subtitle: String,
    pub x_label: String,
    pub y_label: String,
    pub width: f64,
    pub height: f64,
    pub bands: Vec<Band>,
    pub series: Vec<Series>,
    /// `(x, label)` pairs. Empty means "generate numeric ticks".
    pub x_ticks: Vec<(f64, String)>,
}

impl Chart {
    pub fn render(&self) -> String {
        let labelled: Vec<&Series> = self
            .series
            .iter()
            .filter(|s| s.emphasis || self.series.len() <= DIRECT_LABEL_MAX)
            .collect();
        let margin_right = if labelled.is_empty() {
            MARGIN_RIGHT
        } else {
            let longest = labelled
                .iter()
                .map(|s| s.label.chars().count())
                .max()
                .unwrap_or(0);
            // ~6.2 user units per character at 11px; the cap keeps one very long
            // series name from squeezing the plot area to nothing.
            MARGIN_RIGHT + (longest as f64 * 6.2).min(self.width * 0.25)
        };
        let plot_w = (self.width - MARGIN_LEFT - margin_right).max(1.0);
        let plot_h = (self.height - MARGIN_TOP - MARGIN_BOTTOM).max(1.0);

        let (x_min, x_max, y_min, y_max) = self.domain();
        let sx = |x: f64| MARGIN_LEFT + normalize(x, x_min, x_max) * plot_w;
        let sy = |y: f64| MARGIN_TOP + (1.0 - normalize(y, y_min, y_max)) * plot_h;

        let mut out = String::new();
        header(&mut out, self.width, self.height);
        titles(&mut out, &self.title, &self.subtitle);

        // Horizontal gridlines only: a reader traces a value across, and a full
        // grid competes with the data for ink.
        for t in nice_ticks(y_min, y_max, 5) {
            let y = sy(t);
            let _ = writeln!(
                out,
                r#"  <line class="grid" x1="{:.1}" y1="{y:.1}" x2="{:.1}" y2="{y:.1}"/>"#,
                MARGIN_LEFT,
                MARGIN_LEFT + plot_w
            );
            let _ = writeln!(
                out,
                r#"  <text class="tick" x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
                MARGIN_LEFT - 8.0,
                y + 3.5,
                xml_escape(&fmt_num(t))
            );
        }

        let x_ticks: Vec<(f64, String)> = if self.x_ticks.is_empty() {
            nice_ticks(x_min, x_max, 6)
                .into_iter()
                .map(|t| (t, fmt_num(t)))
                .collect()
        } else {
            self.x_ticks.clone()
        };
        for (t, label) in &x_ticks {
            if *t < x_min || *t > x_max {
                continue;
            }
            let x = sx(*t);
            let _ = writeln!(
                out,
                r#"  <line class="axis" x1="{x:.1}" y1="{:.1}" x2="{x:.1}" y2="{:.1}"/>"#,
                MARGIN_TOP + plot_h,
                MARGIN_TOP + plot_h + 5.0
            );
            let _ = writeln!(
                out,
                r#"  <text class="tick" x="{x:.1}" y="{:.1}" text-anchor="middle">{}</text>"#,
                MARGIN_TOP + plot_h + 18.0,
                xml_escape(label)
            );
        }

        // Baseline and left axis.
        let _ = writeln!(
            out,
            r#"  <line class="axis" x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}"/>"#,
            MARGIN_LEFT,
            MARGIN_TOP + plot_h,
            MARGIN_LEFT + plot_w,
            MARGIN_TOP + plot_h
        );

        // Bands under the lines they belong to.
        for band in &self.bands {
            let mut d = String::new();
            for (i, (x, _, hi)) in band.points.iter().enumerate() {
                let _ = write!(
                    d,
                    "{}{:.1},{:.1} ",
                    if i == 0 { "M" } else { "L" },
                    sx(*x),
                    sy(*hi)
                );
            }
            for (x, lo, _) in band.points.iter().rev() {
                let _ = write!(d, "L{:.1},{:.1} ", sx(*x), sy(*lo));
            }
            if !d.is_empty() {
                let _ = writeln!(
                    out,
                    r#"  <path class="band s{}" d="{}Z"/>"#,
                    band.slot % SERIES_LIGHT.len(),
                    d.trim_end()
                );
            }
        }

        for s in &self.series {
            for run in finite_runs(&s.points) {
                let d: String = run
                    .iter()
                    .enumerate()
                    .map(|(i, (x, y))| {
                        format!(
                            "{}{:.1},{:.1}",
                            if i == 0 { "M" } else { "L" },
                            sx(*x),
                            sy(*y)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                // A single-point run has no length to stroke, so it is drawn as
                // a dot: an hour of data in an otherwise empty range is a real
                // reading, not nothing.
                if run.len() == 1 {
                    let _ = writeln!(
                        out,
                        r#"  <circle class="dot s{}" cx="{:.1}" cy="{:.1}" r="3"/>"#,
                        s.slot % SERIES_LIGHT.len(),
                        sx(run[0].0),
                        sy(run[0].1)
                    );
                } else {
                    let _ = writeln!(
                        out,
                        r#"  <path class="line s{}{}" d="{d}"/>"#,
                        s.slot % SERIES_LIGHT.len(),
                        if s.emphasis { " emph" } else { "" }
                    );
                }
            }
        }

        for s in &labelled {
            if let Some((x, y)) = s.points.iter().rev().find(|(_, y)| y.is_finite()) {
                let _ = writeln!(
                    out,
                    r#"  <text class="direct s{}" x="{:.1}" y="{:.1}">{}</text>"#,
                    s.slot % SERIES_LIGHT.len(),
                    sx(*x) + 6.0,
                    sy(*y) + 3.5,
                    xml_escape(&s.label)
                );
            }
        }

        axis_labels(
            &mut out,
            &self.x_label,
            &self.y_label,
            self.width,
            self.height,
            plot_h,
        );
        // One series is named by the title; a legend box would just repeat it.
        if self.series.len() + self.bands.len() > 1 {
            let entries: Vec<(usize, &str)> = self
                .bands
                .iter()
                .map(|b| (b.slot, b.label.as_str()))
                .chain(self.series.iter().map(|s| (s.slot, s.label.as_str())))
                .collect();
            legend(&mut out, &entries, MARGIN_LEFT, LEGEND_Y);
        }
        out.push_str("</svg>\n");
        out
    }

    fn domain(&self) -> (f64, f64, f64, f64) {
        let mut x_min = f64::INFINITY;
        let mut x_max = f64::NEG_INFINITY;
        let mut y_min = f64::INFINITY;
        let mut y_max = f64::NEG_INFINITY;
        let mut see = |x: f64, y: f64| {
            if x.is_finite() {
                x_min = x_min.min(x);
                x_max = x_max.max(x);
            }
            if y.is_finite() {
                y_min = y_min.min(y);
                y_max = y_max.max(y);
            }
        };
        for s in &self.series {
            for (x, y) in &s.points {
                see(*x, *y);
            }
        }
        for b in &self.bands {
            for (x, lo, hi) in &b.points {
                see(*x, *lo);
                see(*x, *hi);
            }
        }
        if !x_min.is_finite() {
            x_min = 0.0;
            x_max = 1.0;
        }
        if !y_min.is_finite() {
            y_min = 0.0;
            y_max = 1.0;
        }
        // Pad the value axis so a line does not run along the frame, and give a
        // constant series a band to sit in the middle of.
        let span = y_max - y_min;
        if span.abs() < f64::EPSILON {
            let pad = y_max.abs().max(1.0) * 0.1;
            (x_min, x_max, y_min - pad, y_max + pad)
        } else {
            (x_min, x_max, y_min - span * 0.05, y_max + span * 0.05)
        }
    }
}

/// A heatmap of `values[row][col]`, e.g. hour-of-day against day.
pub struct Heatmap {
    pub title: String,
    pub subtitle: String,
    pub x_label: String,
    pub y_label: String,
    /// Column tick labels; `None` entries are unlabelled columns.
    pub x_labels: Vec<Option<String>>,
    pub y_labels: Vec<Option<String>>,
    /// `values[row][col]`, `None` where there is no reading.
    pub values: Vec<Vec<Option<f64>>>,
    pub width: f64,
    pub height: f64,
}

impl Heatmap {
    pub fn render(&self) -> String {
        let rows = self.values.len();
        let cols = self.values.first().map(Vec::len).unwrap_or(0);
        let margin_right = 84.0; // the colorbar
        let plot_w = (self.width - MARGIN_LEFT - margin_right).max(1.0);
        let plot_h = (self.height - MARGIN_TOP - MARGIN_BOTTOM).max(1.0);
        let cw = plot_w / cols.max(1) as f64;
        let ch = plot_h / rows.max(1) as f64;

        let (min, max) = self
            .values
            .iter()
            .flatten()
            .flatten()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                (lo.min(*v), hi.max(*v))
            });

        let mut out = String::new();
        header(&mut out, self.width, self.height);
        titles(&mut out, &self.title, &self.subtitle);

        for (r, row) in self.values.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let x = MARGIN_LEFT + c as f64 * cw;
                let y = MARGIN_TOP + r as f64 * ch;
                match cell {
                    // A missing cell is left as the surface with a hairline, so
                    // "no reading" cannot be mistaken for "the lowest reading".
                    None => {
                        let _ = writeln!(
                            out,
                            r#"  <rect class="cell empty" x="{x:.2}" y="{y:.2}" width="{cw:.2}" height="{ch:.2}"/>"#
                        );
                    }
                    Some(v) => {
                        let _ = writeln!(
                            out,
                            r#"  <rect class="cell" x="{x:.2}" y="{y:.2}" width="{cw:.2}" height="{ch:.2}" fill="{}"><title>{}</title></rect>"#,
                            ramp(*v, min, max),
                            xml_escape(&fmt_num(*v))
                        );
                    }
                }
            }
        }

        for (r, label) in self.y_labels.iter().enumerate() {
            if let Some(label) = label {
                let _ = writeln!(
                    out,
                    r#"  <text class="tick" x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
                    MARGIN_LEFT - 8.0,
                    MARGIN_TOP + (r as f64 + 0.5) * ch + 3.5,
                    xml_escape(label)
                );
            }
        }
        for (c, label) in self.x_labels.iter().enumerate() {
            if let Some(label) = label {
                let _ = writeln!(
                    out,
                    r#"  <text class="tick" x="{:.1}" y="{:.1}" text-anchor="middle">{}</text>"#,
                    MARGIN_LEFT + (c as f64 + 0.5) * cw,
                    MARGIN_TOP + plot_h + 16.0,
                    xml_escape(label)
                );
            }
        }

        colorbar(
            &mut out,
            self.width - margin_right + 24.0,
            MARGIN_TOP,
            plot_h,
            min,
            max,
        );
        axis_labels(
            &mut out,
            &self.x_label,
            &self.y_label,
            self.width,
            self.height,
            plot_h,
        );
        out.push_str("</svg>\n");
        out
    }
}

fn ramp(v: f64, min: f64, max: f64) -> &'static str {
    let t = if (max - min).abs() < f64::EPSILON {
        0.5
    } else {
        ((v - min) / (max - min)).clamp(0.0, 1.0)
    };
    let idx = (t * (SEQUENTIAL.len() - 1) as f64).round() as usize;
    SEQUENTIAL[idx.min(SEQUENTIAL.len() - 1)]
}

fn colorbar(out: &mut String, x: f64, y: f64, h: f64, min: f64, max: f64) {
    let step_h = h / SEQUENTIAL.len() as f64;
    // Drawn top-down from the darkest step, so "up the bar" means "more".
    for (i, color) in SEQUENTIAL.iter().rev().enumerate() {
        let _ = writeln!(
            out,
            r#"  <rect x="{x:.1}" y="{:.2}" width="14" height="{step_h:.2}" fill="{color}"/>"#,
            y + i as f64 * step_h
        );
    }
    for (frac, value) in [(0.0, max), (1.0, min)] {
        let _ = writeln!(
            out,
            r#"  <text class="tick" x="{:.1}" y="{:.1}">{}</text>"#,
            x + 20.0,
            y + frac * h + if frac == 0.0 { 8.0 } else { 0.0 },
            xml_escape(&fmt_num(value))
        );
    }
}

/// The swatch + label row above the plot.
///
/// An entry with an empty label is skipped rather than drawn as a bare swatch.
/// Several traces routinely share one slot and one meaning — a forecast's
/// windows, a spaghetti plot's scenarios — and only the first carries the label
/// that names the group; emitting a swatch for each of the rest would claim
/// there are more distinct things on the chart than there are.
fn legend(out: &mut String, entries: &[(usize, &str)], x: f64, y: f64) {
    let mut cursor = x;
    for (slot, label) in entries.iter().filter(|(_, label)| !label.is_empty()) {
        let _ = writeln!(
            out,
            r#"  <rect class="swatch s{}" x="{cursor:.1}" y="{:.1}" width="10" height="10" rx="2"/>"#,
            slot % SERIES_LIGHT.len(),
            y - 8.0
        );
        let _ = writeln!(
            out,
            r#"  <text class="legend" x="{:.1}" y="{y:.1}">{}</text>"#,
            cursor + 15.0,
            xml_escape(label)
        );
        cursor += 15.0 + 11.0 + label.chars().count() as f64 * 6.2;
    }
}

fn axis_labels(out: &mut String, x_label: &str, y_label: &str, w: f64, h: f64, plot_h: f64) {
    if !x_label.is_empty() {
        let _ = writeln!(
            out,
            r#"  <text class="axis-label" x="{:.1}" y="{:.1}" text-anchor="middle">{}</text>"#,
            w / 2.0,
            h - 12.0,
            xml_escape(x_label)
        );
    }
    if !y_label.is_empty() {
        let cy = MARGIN_TOP + plot_h / 2.0;
        let _ = writeln!(
            out,
            r#"  <text class="axis-label" x="16" y="{cy:.1}" text-anchor="middle" transform="rotate(-90 16 {cy:.1})">{}</text>"#,
            xml_escape(y_label)
        );
    }
}

fn titles(out: &mut String, title: &str, subtitle: &str) {
    if !title.is_empty() {
        let _ = writeln!(
            out,
            r#"  <text class="title" x="{MARGIN_LEFT:.1}" y="{TITLE_Y:.1}">{}</text>"#,
            xml_escape(title)
        );
    }
    if !subtitle.is_empty() {
        let _ = writeln!(
            out,
            r#"  <text class="subtitle" x="{MARGIN_LEFT:.1}" y="{SUBTITLE_Y:.1}">{}</text>"#,
            xml_escape(subtitle)
        );
    }
}

fn header(out: &mut String, w: f64, h: f64) {
    let _ = writeln!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w:.0} {h:.0}" width="{w:.0}" height="{h:.0}" font-family="system-ui, -apple-system, &quot;Segoe UI&quot;, sans-serif">"#
    );
    out.push_str(&style_block());
    let _ = writeln!(
        out,
        r#"  <rect class="surface" width="100%" height="100%"/>"#
    );
}

/// The whole theme, both modes, inline. Everything downstream is class-based so
/// the light/dark pair is declared exactly once per role.
fn style_block() -> String {
    let mut s = String::from("  <style>\n");
    let mode = |dark: bool| {
        let pick = |pair: (&'static str, &'static str)| if dark { pair.1 } else { pair.0 };
        let series = if dark { SERIES_DARK } else { SERIES_LIGHT };
        let mut b = String::new();
        let _ = write!(
            b,
            "    .surface{{fill:{}}}\n    \
             .title{{fill:{};font-size:15px;font-weight:600}}\n    \
             .subtitle{{fill:{};font-size:11.5px}}\n    \
             .legend{{fill:{};font-size:11.5px}}\n    \
             .tick{{fill:{};font-size:11px;font-variant-numeric:tabular-nums}}\n    \
             .axis-label{{fill:{};font-size:11.5px}}\n    \
             .grid{{stroke:{};stroke-width:1}}\n    \
             .axis{{stroke:{};stroke-width:1}}\n    \
             .line{{fill:none;stroke-width:2;stroke-linejoin:round;stroke-linecap:round}}\n    \
             .line.emph{{stroke-width:3}}\n    \
             .band{{stroke:none;opacity:0.18}}\n    \
             .direct{{font-size:11px}}\n    \
             .cell.empty{{fill:{};stroke:{};stroke-width:0.5}}\n",
            pick(SURFACE),
            pick(INK_PRIMARY),
            pick(INK_SECONDARY),
            pick(INK_SECONDARY),
            pick(INK_MUTED),
            pick(INK_SECONDARY),
            pick(GRIDLINE),
            pick(AXIS),
            pick(SURFACE),
            pick(GRIDLINE),
        );
        for (i, color) in series.iter().enumerate() {
            let _ = write!(
                b,
                "    .line.s{i}{{stroke:{color}}}\n    .dot.s{i}{{fill:{color}}}\n    \
                 .band.s{i}{{fill:{color}}}\n    .swatch.s{i}{{fill:{color}}}\n    \
                 .direct.s{i}{{fill:{color}}}\n"
            );
        }
        b
    };
    s.push_str(&mode(false));
    s.push_str("    @media (prefers-color-scheme: dark) {\n");
    s.push_str(&mode(true).replace("    .", "      ."));
    s.push_str("    }\n  </style>\n");
    s
}

/// Split a series at its non-finite values, so a gap is a break in the line
/// rather than a straight segment across missing data.
fn finite_runs(points: &[(f64, f64)]) -> Vec<Vec<(f64, f64)>> {
    let mut runs = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    for p in points {
        if p.0.is_finite() && p.1.is_finite() {
            current.push(*p);
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

fn normalize(v: f64, min: f64, max: f64) -> f64 {
    if (max - min).abs() < f64::EPSILON {
        0.5
    } else {
        (v - min) / (max - min)
    }
}

/// Wrap an SVG document in a minimal self-contained HTML page.
pub fn as_html(title: &str, svg: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<style>\n\
         html{{color-scheme:light dark}}\n\
         body{{margin:0;display:grid;place-items:center;min-height:100vh;\
         background:{};font-family:system-ui,-apple-system,\"Segoe UI\",sans-serif}}\n\
         @media (prefers-color-scheme: dark){{body{{background:{}}}}}\n\
         svg{{max-width:100%;height:auto}}\n</style>\n</head>\n<body>\n{svg}</body>\n</html>\n",
        xml_escape(title),
        "#f9f9f7",
        "#0d0d0d",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart(points: Vec<(f64, f64)>) -> Chart {
        Chart {
            title: "t".into(),
            subtitle: String::new(),
            x_label: String::new(),
            y_label: String::new(),
            width: 900.0,
            height: 420.0,
            bands: Vec::new(),
            series: vec![Series {
                label: "a".into(),
                points,
                slot: 0,
                emphasis: false,
            }],
            x_ticks: Vec::new(),
        }
    }

    #[test]
    fn a_rendered_chart_is_one_self_contained_svg_document() {
        let svg = chart((0..10).map(|i| (i as f64, i as f64)).collect()).render();
        assert!(svg.starts_with("<svg "));
        assert!(svg.trim_end().ends_with("</svg>"));
        // Nothing may reach off the file: no scripts, no remote references.
        assert!(!svg.contains("http://") || svg.matches("http://").count() == 1);
        assert!(!svg.contains("<script"));
    }

    /// Caller text lands inside XML, so it has to be escaped or the file will
    /// not parse. A series really can be named `P<50`.
    #[test]
    fn caller_text_is_xml_escaped() {
        let mut c = chart(vec![(0.0, 0.0), (1.0, 1.0)]);
        c.title = "P<50 & \"peak\"".into();
        let svg = c.render();
        assert!(svg.contains("P&lt;50 &amp; &quot;peak&quot;"), "{svg}");
        assert!(!svg.contains("P<50"));
    }

    /// A gap must break the line, not be bridged by a segment that implies data
    /// nobody stored.
    #[test]
    fn non_finite_values_split_the_path() {
        let runs = finite_runs(&[(0.0, 1.0), (1.0, f64::NAN), (2.0, 3.0), (3.0, 4.0)]);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len(), 1);
        assert_eq!(runs[1].len(), 2);
    }

    /// A forecast's windows, or a spaghetti plot's scenarios, are many traces
    /// with one meaning: only the first is labelled, and the unlabelled rest
    /// must not each add a swatch claiming to be a separate thing.
    #[test]
    fn unlabelled_traces_add_nothing_to_the_legend() {
        let mut c = chart(vec![(0.0, 0.0), (1.0, 1.0)]);
        c.series[0].label = "actual".into();
        for _ in 0..5 {
            c.series.push(Series {
                label: String::new(),
                points: vec![(0.0, 0.5), (1.0, 0.5)],
                slot: 1,
                emphasis: false,
            });
        }
        let svg = c.render();
        assert_eq!(
            svg.matches(r#"class="swatch"#).count(),
            1,
            "one labelled series, so one swatch:\n{svg}"
        );
    }

    #[test]
    fn both_themes_are_written_into_the_file() {
        let svg = chart(vec![(0.0, 0.0), (1.0, 1.0)]).render();
        assert!(svg.contains(SERIES_LIGHT[0]));
        assert!(svg.contains(SERIES_DARK[0]));
        assert!(svg.contains("prefers-color-scheme: dark"));
    }
}
