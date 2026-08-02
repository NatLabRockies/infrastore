//! Chart rendering: a terminal sparkline and a hand-written SVG backend.
//!
//! Hand-written on purpose. `deny.toml` makes every new dependency a license
//! review, and a charting crate is a policy decision where a few hundred lines
//! of `<path d="...">` is not. The command surface (`get --plot`, `plot --kind`)
//! is deliberately independent of what is behind it, so a real charting backend
//! could replace this module without any command changing.
//!
//! # Color
//!
//! The categorical slots, the sequential ramp, and the chart chrome below are a
//! validated palette used unchanged and **in fixed slot order** — the ordering
//! is what makes adjacent pairs distinguishable under color-vision deficiency,
//! so slots are assigned by series position and never cycled or reordered.
//! Three of the light-mode hues sit below 3:1 against the light surface, which
//! obliges the "relief" the palette requires: every chart with more than one
//! series carries a text legend, and one with four or fewer also direct-labels
//! each line, so identity is never carried by color alone.

pub mod spark;
pub mod svg;

/// Categorical series colors, light surface, in assignment order.
pub const SERIES_LIGHT: [&str; 8] = [
    "#2a78d6", // blue
    "#eb6834", // orange
    "#1baf7a", // aqua
    "#eda100", // yellow
    "#e87ba4", // magenta
    "#008300", // green
    "#4a3aa7", // violet
    "#e34948", // red
];

/// The same eight hues stepped for the dark surface — a selected dark palette,
/// not an automatic inversion of the light one.
pub const SERIES_DARK: [&str; 8] = [
    "#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767",
];

/// How many series a categorical chart will paint before refusing.
///
/// Past the eighth slot there is no ninth hue to assign — a generated one would
/// not be validated against the others — so the command errors and says how to
/// narrow, rather than cycling colors and producing a chart whose legend lies.
pub const MAX_SERIES: usize = SERIES_LIGHT.len();

/// Blue sequential ramp, light to dark, for magnitude (the heatmap).
pub const SEQUENTIAL: [&str; 12] = [
    "#cde2fb", "#b7d3f6", "#9ec5f4", "#86b6ef", "#6da7ec", "#5598e7", "#3987e5", "#2a78d6",
    "#256abf", "#1c5cab", "#184f95", "#104281",
];

/// Chart chrome: `(light, dark)` for each role.
pub const SURFACE: (&str, &str) = ("#fcfcfb", "#1a1a19");
pub const INK_PRIMARY: (&str, &str) = ("#0b0b0b", "#ffffff");
pub const INK_SECONDARY: (&str, &str) = ("#52514e", "#c3c2b7");
pub const INK_MUTED: (&str, &str) = ("#898781", "#898781");
pub const GRIDLINE: (&str, &str) = ("#e1e0d9", "#2c2c2a");
pub const AXIS: (&str, &str) = ("#c3c2b7", "#383835");

/// Escape the five characters that cannot appear literally in XML text or an
/// attribute value. Every caller-supplied string (series names, titles, units)
/// goes through this — a series named `a<b` would otherwise produce a file no
/// SVG parser accepts.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// "Nice" tick values covering `[min, max]`, at most `target` of them.
///
/// The standard 1/2/5×10^n rule: ticks land on numbers a reader recognizes
/// (0, 25, 50) instead of wherever the data happened to end (0, 23.7, 47.4).
pub fn nice_ticks(min: f64, max: f64, target: usize) -> Vec<f64> {
    if !min.is_finite() || !max.is_finite() || target == 0 {
        return Vec::new();
    }
    if (max - min).abs() < f64::EPSILON {
        return vec![min];
    }
    let raw = (max - min) / target as f64;
    let magnitude = 10f64.powf(raw.abs().log10().floor());
    let normalized = raw / magnitude;
    let step = magnitude
        * if normalized <= 1.0 {
            1.0
        } else if normalized <= 2.0 {
            2.0
        } else if normalized <= 5.0 {
            5.0
        } else {
            10.0
        };
    let first = (min / step).ceil() * step;
    let mut out = Vec::new();
    let mut v = first;
    // `target + 2` is a hard stop: a pathological step could otherwise spin.
    while v <= max + step * 1e-9 && out.len() < target + 2 {
        out.push(v);
        v += step;
    }
    out
}

/// Format a number for an axis tick or a label: no trailing zeros, no
/// exponent for the magnitudes power-systems data actually has.
pub fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return "-".to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    let abs = v.abs();
    let text = if !(1e-3..1e6).contains(&abs) {
        format!("{v:.3e}")
    } else {
        let decimals = if abs >= 100.0 {
            0
        } else if abs >= 1.0 {
            2
        } else {
            4
        };
        format!("{v:.decimals$}")
    };
    if text.contains('.') && !text.contains('e') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}
