//! Terminal sparklines: "does this profile look sane" without leaving the shell.
//!
//! One line of Unicode block elements per element of the series. It answers the
//! shape question — a flat line, a nightly trough, a step that should not be
//! there — and nothing else; anything needing axes belongs in `infrastore plot`.

/// Eighth-blocks, ascending. A value's bucket indexes straight into this.
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// What a bucket with no finite value renders as. A space rather than `▁`:
/// a gap in the data must not look like a zero.
const GAP: char = ' ';

/// Default width when the terminal size is unknown.
pub const DEFAULT_WIDTH: usize = 72;

/// A rendered sparkline plus the range it was scaled against.
pub struct Sparkline {
    pub line: String,
    pub min: f64,
    pub max: f64,
    /// Values that were not finite (NaN / ±inf), rendered as gaps.
    pub non_finite: usize,
}

/// Render `values` as a single sparkline `width` characters wide.
///
/// Longer inputs are bucketed, and each bucket draws its *most extreme* sample —
/// the one furthest from the series mean — rather than its average or an
/// arbitrary representative. Both alternatives lose the thing this plot exists
/// to show: a one-hour spike in a year of hourly data is a hundredth of its
/// bucket, so averaging flattens it back into the baseline, and sampling drops
/// it outright with 99% probability. Taking the extreme keeps peaks *and*
/// troughs visible, which is what "does this profile look sane" is asking.
///
/// The cost is that a column is not a summary of its bucket — an isolated
/// outlier will make one column tall on its own. That is the intended reading
/// for a sanity check; `infrastore plot` draws the real curve.
///
/// Shorter inputs are drawn as-is, one character per value, rather than
/// stretched.
pub fn render(values: &[f64], width: usize) -> Sparkline {
    let width = width.max(1);
    let finite: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    let non_finite = values.len() - finite.len();
    let (min, max) = finite
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(*v), hi.max(*v))
        });
    if finite.is_empty() {
        return Sparkline {
            line: GAP.to_string().repeat(values.len().min(width)),
            min: f64::NAN,
            max: f64::NAN,
            non_finite,
        };
    }
    let mean = finite.iter().sum::<f64>() / finite.len() as f64;

    let buckets = width.min(values.len().max(1));
    let mut line = String::with_capacity(buckets);
    for b in 0..buckets {
        let start = b * values.len() / buckets;
        let end = ((b + 1) * values.len() / buckets)
            .max(start + 1)
            .min(values.len());
        let extreme = values[start..end]
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .max_by(|a, b| {
                (a - mean)
                    .abs()
                    .partial_cmp(&(b - mean).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        match extreme {
            Some(v) => line.push(block(v, min, max)),
            None => line.push(GAP),
        }
    }
    Sparkline {
        line,
        min,
        max,
        non_finite,
    }
}

fn block(value: f64, min: f64, max: f64) -> char {
    // A constant series sits on the bottom block rather than dividing by zero.
    // Rendering it mid-height would suggest a mid-range value it does not have.
    if (max - min).abs() < f64::EPSILON {
        return BLOCKS[0];
    }
    let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
    let idx = (t * (BLOCKS.len() - 1) as f64).round() as usize;
    BLOCKS[idx.min(BLOCKS.len() - 1)]
}

/// The terminal's width, minus room for the labels a sparkline row carries.
pub fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .map(|c| c.saturating_sub(24).max(16))
        .unwrap_or(DEFAULT_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ramp_rises_across_the_line() {
        let values: Vec<f64> = (0..8).map(|i| i as f64).collect();
        let s = render(&values, 8);
        assert_eq!(s.line, "▁▂▃▄▅▆▇█");
        assert_eq!(s.min, 0.0);
        assert_eq!(s.max, 7.0);
    }

    /// The property that makes this sparkline trustworthy: one anomalous hour
    /// in a thousand still shows. Averaging its bucket would divide it by 100
    /// and sampling would almost certainly miss it.
    #[test]
    fn a_narrow_spike_survives_downsampling() {
        let mut values = vec![0.0; 1000];
        values[500] = 100.0;
        let s = render(&values, 10);
        assert_eq!(s.line, "▁▁▁▁▁█▁▁▁▁", "the spike should be in bucket 5");
        assert_eq!(s.max, 100.0);
    }

    /// A trough is as much of an anomaly as a spike, so the same rule has to
    /// catch it — an outage hour in a flat profile must not average away.
    #[test]
    fn a_narrow_trough_survives_too() {
        let mut values = vec![10.0; 1000];
        values[500] = 0.0;
        let s = render(&values, 10);
        assert_eq!(s.line, "█████▁████", "the outage should be in bucket 5");
        assert_eq!(s.min, 0.0);
    }

    #[test]
    fn non_finite_values_render_as_gaps_not_zeros() {
        let s = render(&[f64::NAN, f64::NAN], 2);
        assert_eq!(s.line, "  ");
        assert_eq!(s.non_finite, 2);
    }

    #[test]
    fn a_constant_series_does_not_divide_by_zero() {
        let s = render(&[5.0; 10], 10);
        assert_eq!(s.line, "▁".repeat(10));
        assert_eq!(s.min, 5.0);
        assert_eq!(s.max, 5.0);
    }
}
