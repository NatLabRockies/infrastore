//! Calendar-aware time period.
//!
//! [`Period`] is the core's representation of a `resolution`, `horizon`, or
//! `interval`. It distinguishes two kinds of period:
//!
//! - **regular** ([`Period::Fixed`]) — a fixed nanosecond span (`Hour`,
//!   `Minute`, `Day`, `Week`), backed by a [`chrono::Duration`]. Arithmetic is
//!   ordinary duration math.
//! - **irregular** ([`Period::Months`]) — a count of calendar months (a
//!   `Month` is 1, a `Quarter` is 3, a `Year` is 12). A calendar month has no
//!   fixed span, so arithmetic goes through chrono's calendar-aware
//!   `checked_add_months`.
//!
//! Equality (and the catalog uniqueness key) is by `(kind, magnitude)`: a
//! `Fixed` period is **never** equal to a `Months` period even when their spans
//! happen to coincide for a particular month. This is the property that keeps a
//! monthly resolution from over-matching a millisecond-canonicalized one.
//!
//! The on-disk and over-the-wire encoding is the ISO-8601 duration string
//! ([`Period::to_iso8601`] / [`Period::from_iso8601`]), e.g. `PT1H`, `P1M`,
//! `P1Y`.
//!
//! # Smallest supported period
//!
//! **One millisecond** for [`Period::Fixed`], one month for [`Period::Months`].
//! The millisecond floor is not incidental — it is the unit every `Fixed`
//! computation works in:
//!
//! - [`Period::is_positive`] tests `num_milliseconds() > 0`, so a sub-millisecond
//!   duration is not a positive period and every forecast constructor rejects it;
//! - [`Period::to_iso8601`] emits at most three fractional-second digits, so
//!   `PT0.001S` is the smallest non-zero period that can be encoded;
//! - [`Period::from_iso8601`] **rejects** more than three fractional digits, so a
//!   finer period cannot be read back from disk or off the wire either.
//!
//! `Period::Fixed` wraps a [`chrono::Duration`], which *can* hold a finer span,
//! and the conversion is lossy in one direction only: constructing
//! `Period::fixed(Duration::microseconds(500))` succeeds but encodes as `PT0S`.
//! Such a period is not [`Period::is_positive`], and **no write path accepts one
//! as a resolution** — the forecast constructors reject it, and so does the
//! static path, which validates a `SingleTimeSeries` before it is stored. A
//! series on a grid finer than a millisecond is therefore not storable at all,
//! rather than storable and unreadable. Callers needing a finer grid should scale
//! the unit instead: a 500 µs series is a 500-unit series that records the unit
//! in `units`.
//!
//! Note that this floor applies to *periods*, not to timestamps: an
//! `initial_timestamp` is stored as an RFC3339 string and keeps nanoseconds, so a
//! grid may be millisecond-*spaced* while being nanosecond-*offset* in its phase.
//! [`Period::steps_between`] therefore compares grid landings exactly rather than
//! in whole milliseconds.

use chrono::{DateTime, Datelike, Duration, Months, Utc};

use crate::error::{Result, TimeSeriesError};

const MS_PER_SEC: i64 = 1_000;
const MS_PER_MIN: i64 = 60_000;
const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_DAY: i64 = 86_400_000;
const MS_PER_WEEK: i64 = 604_800_000;

/// A calendar-aware time period. See the module docs for the regular/irregular
/// distinction and equality semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Period {
    /// A fixed nanosecond span (`Hour`, `Minute`, `Day`, `Week`, …).
    Fixed(Duration),
    /// A count of calendar months (`Month` = 1, `Quarter` = 3, `Year` = 12).
    Months(i32),
}

impl Period {
    /// A fixed-span period from a [`Duration`].
    pub fn fixed(d: Duration) -> Self {
        Period::Fixed(d)
    }

    /// An irregular period of `n` calendar months.
    pub fn months(n: i32) -> Self {
        Period::Months(n)
    }

    /// Whether this is a calendar (irregular) period.
    pub fn is_irregular(&self) -> bool {
        matches!(self, Period::Months(_))
    }

    /// Whether two periods are the same kind (both `Fixed` or both `Months`).
    pub fn same_kind(&self, other: &Period) -> bool {
        matches!(
            (self, other),
            (Period::Fixed(_), Period::Fixed(_)) | (Period::Months(_), Period::Months(_))
        )
    }

    /// Whether the period is strictly positive (the requirement for a
    /// resolution, horizon, or interval).
    pub fn is_positive(&self) -> bool {
        match self {
            Period::Fixed(d) => d.num_milliseconds() > 0,
            Period::Months(m) => *m > 0,
        }
    }

    /// Whether the period is exactly zero (the interval of a single-window
    /// forecast, which has no second window to step to).
    pub fn is_zero(&self) -> bool {
        match self {
            Period::Fixed(d) => d.num_milliseconds() == 0,
            Period::Months(m) => *m == 0,
        }
    }

    /// The zero period — the canonical interval of a single-window forecast.
    pub fn zero() -> Self {
        Period::Fixed(Duration::zero())
    }

    /// Advance `dt` by `k` of this period (calendar-aware for [`Period::Months`]).
    /// Returns `None` on arithmetic overflow or an out-of-range date.
    pub fn add_to(&self, dt: DateTime<Utc>, k: i64) -> Option<DateTime<Utc>> {
        match self {
            Period::Fixed(d) => {
                let total_ms = d.num_milliseconds().checked_mul(k)?;
                dt.checked_add_signed(Duration::try_milliseconds(total_ms)?)
            }
            Period::Months(m) => {
                let total = (*m as i64).checked_mul(k)?;
                if total >= 0 {
                    dt.checked_add_months(Months::new(u32::try_from(total).ok()?))
                } else {
                    dt.checked_sub_months(Months::new(u32::try_from(-total).ok()?))
                }
            }
        }
    }

    /// The number of whole periods from `start` to `at` on this period's grid.
    ///
    /// Errors if `at` is before `start` or does not land exactly on the grid
    /// `start, start+1·self, start+2·self, …` (no rounding or clamping). Both
    /// period kinds confirm the landing with `self.add_to(start, k) == at`:
    ///
    /// - for [`Period::Months`] that rejects day-of-month/time-of-day mismatches
    ///   (e.g. Jan-31 + 1 month is Feb-28, not Feb-31);
    /// - for [`Period::Fixed`] it rejects a `at` whose offset from a grid point is
    ///   finer than the millisecond the step count is computed in. Without it, an
    ///   `at` in the open range `(grid point, grid point + 1ms)` divides cleanly
    ///   in whole milliseconds and would be reported as *on* the grid, leaving a
    ///   caller with a step index whose grid point is strictly before `at`.
    ///   Callers that want to snap an arbitrary bound onto the grid instead of
    ///   rejecting it should use [`Period::floor_steps`] / [`Period::ceil_steps`].
    pub fn steps_between(&self, start: DateTime<Utc>, at: DateTime<Utc>) -> Result<usize> {
        let off_grid = |what: &str| {
            TimeSeriesError::InvalidParameter(format!(
                "timestamp {at} is {what} the {} grid starting at {start}",
                self.to_iso8601()
            ))
        };
        if at < start {
            return Err(off_grid("before"));
        }
        match self {
            Period::Fixed(d) => {
                let step_ms = d.num_milliseconds();
                if step_ms <= 0 {
                    return Err(TimeSeriesError::InvalidParameter(
                        "period must be strictly positive".to_string(),
                    ));
                }
                let delta_ms = (at - start).num_milliseconds();
                if delta_ms % step_ms != 0 {
                    return Err(off_grid("not aligned to"));
                }
                let k = delta_ms / step_ms;
                // `delta_ms` truncates toward zero, so divisibility alone accepts
                // any sub-millisecond offset past a grid point. Verify the exact
                // landing, as the `Months` branch does.
                if self.add_to(start, k) != Some(at) {
                    return Err(off_grid("not aligned to"));
                }
                Ok(k as usize)
            }
            Period::Months(m) => {
                if *m <= 0 {
                    return Err(TimeSeriesError::InvalidParameter(
                        "period must be strictly positive".to_string(),
                    ));
                }
                let months = months_between(start, at);
                if months < 0 || months % (*m as i64) != 0 {
                    return Err(off_grid("not aligned to"));
                }
                let k = months / (*m as i64);
                // Verify the exact landing to catch day-of-month/time mismatches.
                if self.add_to(start, k) != Some(at) {
                    return Err(off_grid("not aligned to"));
                }
                Ok(k as usize)
            }
        }
    }

    /// The largest step index `k >= 0` whose grid point `self.add_to(start, k)`
    /// is still `<= at`, clamped to 0 when `at <= start`. Unlike
    /// [`Period::steps_between`] this does **not** require `at` to land on the
    /// grid — it is for time-range slicing, where the query bounds are arbitrary.
    pub fn floor_steps(&self, start: DateTime<Utc>, at: DateTime<Utc>) -> usize {
        if at <= start {
            return 0;
        }
        match self {
            Period::Fixed(d) => {
                let step = d.num_milliseconds();
                if step <= 0 {
                    return 0;
                }
                ((at - start).num_milliseconds() / step) as usize
            }
            Period::Months(m) => {
                if *m <= 0 {
                    return 0;
                }
                let mut k = (months_between(start, at) / (*m as i64)).max(0);
                // Adjust for day-of-month: the calendar-month estimate can be off
                // by one in either direction.
                while k > 0 && self.add_to(start, k).is_none_or(|t| t > at) {
                    k -= 1;
                }
                while self.add_to(start, k + 1).is_some_and(|t| t <= at) {
                    k += 1;
                }
                k as usize
            }
        }
    }

    /// The smallest step index `k >= 0` whose grid point `self.add_to(start, k)`
    /// is `>= at`, clamped to 0 when `at <= start`. Companion to
    /// [`Period::floor_steps`] for time-range slicing.
    pub fn ceil_steps(&self, start: DateTime<Utc>, at: DateTime<Utc>) -> usize {
        let f = self.floor_steps(start, at);
        if self.add_to(start, f as i64).is_some_and(|t| t >= at) {
            f
        } else {
            f + 1
        }
    }

    /// How many of `self` fit in `other` (i.e. `other / self`) as an exact,
    /// strictly-positive integer. Requires both periods to be the same kind;
    /// mixing a `Fixed` and a `Months` period is an error.
    ///
    /// Used to derive a forecast's per-window length `H = horizon / resolution`
    /// and `interval_steps = interval / resolution`.
    pub fn divide_into(&self, other: &Period) -> Result<usize> {
        let bad = |msg: String| Err(TimeSeriesError::InvalidParameter(msg));
        let ratio = |num: i64, den: i64| -> Result<usize> {
            if den <= 0 {
                return Err(TimeSeriesError::InvalidParameter(
                    "period must be strictly positive".to_string(),
                ));
            }
            if num <= 0 || num % den != 0 {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "{} is not a positive integer multiple of {}",
                    other.to_iso8601(),
                    self.to_iso8601()
                )));
            }
            Ok((num / den) as usize)
        };
        match (self, other) {
            (Period::Fixed(den), Period::Fixed(num)) => {
                ratio(num.num_milliseconds(), den.num_milliseconds())
            }
            (Period::Months(den), Period::Months(num)) => ratio(*num as i64, *den as i64),
            _ => bad(format!(
                "cannot combine periods of different kinds: {} and {}",
                self.to_iso8601(),
                other.to_iso8601()
            )),
        }
    }

    /// Render to a canonical ISO-8601 duration string (`PT1H`, `P1M`, `P1Y`).
    ///
    /// The encoding is a pure function of the value, so two equal periods always
    /// produce the same string — required for the catalog uniqueness key. A
    /// `Months` count that is a whole number of years is rendered with `Y`.
    pub fn to_iso8601(&self) -> String {
        match self {
            Period::Months(m) => {
                // Use a leading `-` for negatives so the output round-trips
                // through `from_iso8601` (which strips an optional sign), matching
                // the `Fixed` branch's convention.
                let sign = if *m < 0 { "-" } else { "" };
                let a = m.unsigned_abs();
                if a != 0 && a % 12 == 0 {
                    format!("{sign}P{}Y", a / 12)
                } else {
                    format!("{sign}P{a}M")
                }
            }
            Period::Fixed(d) => {
                let mut ms = d.num_milliseconds();
                let sign = if ms < 0 { "-" } else { "" };
                ms = ms.abs();
                let days = ms / MS_PER_DAY;
                ms %= MS_PER_DAY;
                let hours = ms / MS_PER_HOUR;
                ms %= MS_PER_HOUR;
                let mins = ms / MS_PER_MIN;
                ms %= MS_PER_MIN;
                let secs = ms / MS_PER_SEC;
                let millis = ms % MS_PER_SEC;

                let mut date = String::new();
                if days > 0 {
                    date.push_str(&format!("{days}D"));
                }
                let mut time = String::new();
                if hours > 0 {
                    time.push_str(&format!("{hours}H"));
                }
                if mins > 0 {
                    time.push_str(&format!("{mins}M"));
                }
                if secs > 0 || millis > 0 {
                    if millis > 0 {
                        let frac = format!("{millis:03}");
                        time.push_str(&format!("{secs}.{}S", frac.trim_end_matches('0')));
                    } else {
                        time.push_str(&format!("{secs}S"));
                    }
                }
                let mut out = format!("{sign}P{date}");
                if !time.is_empty() {
                    out.push('T');
                    out.push_str(&time);
                } else if date.is_empty() {
                    out.push_str("T0S");
                }
                out
            }
        }
    }

    /// Parse a canonical ISO-8601 duration string into a [`Period`].
    ///
    /// Calendar units (`Y`, `M` before the `T`) map to [`Period::Months`];
    /// fixed units (`W`, `D`, and `H`/`M`/`S` after the `T`) map to
    /// [`Period::Fixed`]. Mixing calendar and fixed units in one string is an
    /// error (a period is one kind or the other). An optional leading `-`
    /// (e.g. `-PT1H`, `-P1M`) parses as a negative period, so the output of
    /// [`Period::to_iso8601`] always round-trips.
    ///
    /// Every arithmetic step is checked. `parse_components` applies no
    /// uniqueness rule, so a string may repeat a unit (`P…D…D`), and only the
    /// per-component multiplies used to be guarded — the accumulation was a bare
    /// `+=`. A single day component can reach the edge of `i64` milliseconds on
    /// its own, so repeating one overflowed: a panic in a debug build, and in a
    /// release build, where the workspace profile leaves `overflow-checks` off,
    /// a silently wrapped period. This is fully public and takes an arbitrary
    /// caller string, including one arriving over gRPC.
    pub fn from_iso8601(s: &str) -> Result<Period> {
        let trimmed = s.trim();
        let invalid =
            || TimeSeriesError::InvalidParameter(format!("invalid ISO-8601 period '{trimmed}'"));
        // An optional leading `-` denotes a negative period (the form
        // `to_iso8601` emits for negative magnitudes); strip it and negate the
        // parsed magnitude at the end so encode/decode round-trip.
        let (negative, unsigned) = match trimmed.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };
        let body = unsigned.strip_prefix('P').ok_or_else(invalid)?;
        let (date_part, time_part) = match body.split_once('T') {
            Some((d, t)) => (d, Some(t)),
            None => (body, None),
        };

        let mut months: i64 = 0;
        let mut fixed_ms: i64 = 0;
        let mut has_calendar = false;
        let mut has_fixed = false;

        for (num, unit) in parse_components(date_part).ok_or_else(invalid)? {
            let int_val = num.parse::<i64>().map_err(|_| invalid())?;
            match unit {
                'Y' => {
                    months = add_component(months, int_val.checked_mul(12), invalid)?;
                    has_calendar = true;
                }
                'M' => {
                    months = add_component(months, Some(int_val), invalid)?;
                    has_calendar = true;
                }
                'W' => {
                    fixed_ms = add_component(fixed_ms, int_val.checked_mul(MS_PER_WEEK), invalid)?;
                    has_fixed = true;
                }
                'D' => {
                    fixed_ms = add_component(fixed_ms, int_val.checked_mul(MS_PER_DAY), invalid)?;
                    has_fixed = true;
                }
                _ => return Err(invalid()),
            }
        }
        if let Some(tp) = time_part {
            for (num, unit) in parse_components(tp).ok_or_else(invalid)? {
                match unit {
                    'H' => {
                        let v = num.parse::<i64>().map_err(|_| invalid())?;
                        fixed_ms = add_component(fixed_ms, v.checked_mul(MS_PER_HOUR), invalid)?;
                        has_fixed = true;
                    }
                    'M' => {
                        let v = num.parse::<i64>().map_err(|_| invalid())?;
                        fixed_ms = add_component(fixed_ms, v.checked_mul(MS_PER_MIN), invalid)?;
                        has_fixed = true;
                    }
                    'S' => {
                        fixed_ms = add_component(fixed_ms, seconds_str_to_ms(&num), invalid)?;
                        has_fixed = true;
                    }
                    _ => return Err(invalid()),
                }
            }
        }

        if has_calendar && has_fixed {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "ISO-8601 period '{trimmed}' mixes calendar (Y/M) and fixed (D/H/M/S) units"
            )));
        }
        if has_calendar {
            let signed = if negative { -months } else { months };
            Ok(Period::Months(
                i32::try_from(signed).map_err(|_| invalid())?,
            ))
        } else if has_fixed {
            let ms = if negative { -fixed_ms } else { fixed_ms };
            Ok(Period::Fixed(Duration::milliseconds(ms)))
        } else {
            Err(invalid())
        }
    }
}

/// Serialize as the ISO-8601 duration string (`"PT1H"`, `"P1M"`, …), the same
/// representation used on disk and across every binding. A custom impl (rather
/// than a derive over the `Fixed`/`Months` enum) keeps the serde form identical
/// to the canonical string encoding, so serialized `Period`s are portable and
/// human-readable.
impl serde::Serialize for Period {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_iso8601())
    }
}

impl<'de> serde::Deserialize<'de> for Period {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(deserializer)?;
        Period::from_iso8601(&s).map_err(serde::de::Error::custom)
    }
}

impl From<Duration> for Period {
    /// A [`chrono::Duration`] is unambiguously a fixed-span period.
    fn from(d: Duration) -> Self {
        Period::Fixed(d)
    }
}

impl PartialEq<Duration> for Period {
    /// A [`Period::Fixed`] equals the [`Duration`] it wraps; a [`Period::Months`]
    /// never equals a raw duration (the calendar/fixed distinction is preserved).
    fn eq(&self, other: &Duration) -> bool {
        matches!(self, Period::Fixed(d) if d == other)
    }
}

impl std::fmt::Display for Period {
    /// Displays the canonical ISO-8601 duration string.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_iso8601())
    }
}

/// Calendar months from `start` to `at` (year/month only; day/time ignored).
fn months_between(start: DateTime<Utc>, at: DateTime<Utc>) -> i64 {
    let year = at.year() as i64 - start.year() as i64;
    let month = at.month() as i64 - start.month() as i64;
    year * 12 + month
}

/// Split an ISO-8601 component run (e.g. `"1Y6M"` or `"1.5S"`) into
/// `(number, unit)` pairs. Returns `None` on malformed input.
/// Accumulate a checked-multiply result into a running total, failing the parse
/// on overflow rather than wrapping. Used for both accumulators: the fixed
/// millisecond count and the calendar month count.
///
/// Takes the caller's `invalid` so an overflow reports the same
/// `invalid ISO-8601 period '<input>'` as every other parse failure, naming the
/// string that caused it.
fn add_component(
    total: i64,
    component: Option<i64>,
    invalid: impl Fn() -> TimeSeriesError,
) -> Result<i64> {
    component
        .and_then(|c| total.checked_add(c))
        .ok_or_else(invalid)
}

fn parse_components(part: &str) -> Option<Vec<(String, char)>> {
    let mut out = Vec::new();
    let mut num = String::new();
    for ch in part.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
        } else if ch.is_ascii_alphabetic() {
            if num.is_empty() {
                return None;
            }
            out.push((std::mem::take(&mut num), ch.to_ascii_uppercase()));
        } else {
            return None;
        }
    }
    // A trailing number with no unit is malformed.
    if num.is_empty() { Some(out) } else { None }
}

/// Convert an ISO-8601 seconds field (possibly fractional, e.g. `"1.5"`) into
/// whole milliseconds. Returns `None` if the fraction is finer than a
/// millisecond or the string is malformed.
fn seconds_str_to_ms(s: &str) -> Option<i64> {
    match s.split_once('.') {
        None => s.parse::<i64>().ok()?.checked_mul(MS_PER_SEC),
        Some((whole, frac)) => {
            if frac.len() > 3 || frac.is_empty() {
                return None;
            }
            let whole_ms = whole.parse::<i64>().ok()?.checked_mul(MS_PER_SEC)?;
            let padded = format!("{frac:0<3}"); // right-pad to milliseconds
            let frac_ms = padded.parse::<i64>().ok()?;
            whole_ms.checked_add(frac_ms)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap()
    }

    #[test]
    fn iso_round_trip_fixed() {
        let cases = [
            (Period::Fixed(Duration::hours(1)), "PT1H"),
            (Period::Fixed(Duration::minutes(15)), "PT15M"),
            (Period::Fixed(Duration::minutes(90)), "PT1H30M"),
            (Period::Fixed(Duration::seconds(30)), "PT30S"),
            (Period::Fixed(Duration::milliseconds(1500)), "PT1.5S"),
            (Period::Fixed(Duration::days(1)), "P1D"),
            (Period::Fixed(Duration::days(7)), "P7D"),
        ];
        for (p, iso) in cases {
            assert_eq!(p.to_iso8601(), iso, "encode {p:?}");
            assert_eq!(Period::from_iso8601(iso).unwrap(), p, "decode {iso}");
        }
    }

    #[test]
    fn iso_round_trip_months() {
        assert_eq!(Period::Months(1).to_iso8601(), "P1M");
        assert_eq!(Period::Months(3).to_iso8601(), "P3M");
        assert_eq!(Period::Months(12).to_iso8601(), "P1Y");
        assert_eq!(Period::Months(24).to_iso8601(), "P2Y");
        assert_eq!(Period::from_iso8601("P1M").unwrap(), Period::Months(1));
        assert_eq!(Period::from_iso8601("P1Y").unwrap(), Period::Months(12));
        // Year and 12 months canonicalize to the same value.
        assert_eq!(
            Period::from_iso8601("P12M").unwrap(),
            Period::from_iso8601("P1Y").unwrap()
        );
        // Week parses as fixed.
        assert_eq!(
            Period::from_iso8601("P1W").unwrap(),
            Period::Fixed(Duration::days(7))
        );
    }

    #[test]
    fn iso_round_trip_negative() {
        // Negative magnitudes use a leading `-` for both kinds and round-trip.
        let cases = [
            (Period::Fixed(Duration::hours(-1)), "-PT1H"),
            (Period::Fixed(Duration::minutes(-90)), "-PT1H30M"),
            (Period::Months(-1), "-P1M"),
            (Period::Months(-12), "-P1Y"),
        ];
        for (p, iso) in cases {
            assert_eq!(p.to_iso8601(), iso, "encode {p:?}");
            assert_eq!(Period::from_iso8601(iso).unwrap(), p, "decode {iso}");
        }
    }

    #[test]
    fn fixed_never_equals_months() {
        // ~1 month of milliseconds must not equal a calendar month.
        let approx = Period::Fixed(Duration::days(30));
        assert_ne!(approx, Period::Months(1));
    }

    #[test]
    fn from_iso_rejects_mixed_and_garbage() {
        assert!(Period::from_iso8601("P1Y1D").is_err()); // calendar + fixed
        assert!(Period::from_iso8601("1H").is_err()); // no leading P
        assert!(Period::from_iso8601("P").is_err()); // empty
        assert!(Period::from_iso8601("PT").is_err()); // empty time
        assert!(Period::from_iso8601("PXY").is_err()); // no number
    }

    #[test]
    fn add_to_calendar() {
        let jan = ts(2024, 1, 31, 0);
        // Jan 31 + 1 month clamps to Feb 29 (2024 is a leap year).
        assert_eq!(Period::Months(1).add_to(jan, 1), Some(ts(2024, 2, 29, 0)));
        assert_eq!(Period::Months(1).add_to(jan, 12), Some(ts(2025, 1, 31, 0)));
        assert_eq!(Period::Months(12).add_to(jan, 1), Some(ts(2025, 1, 31, 0)));
        // Fixed.
        assert_eq!(
            Period::Fixed(Duration::hours(1)).add_to(ts(2024, 1, 1, 0), 5),
            Some(ts(2024, 1, 1, 5))
        );
    }

    #[test]
    fn steps_between_rejects_sub_millisecond_offsets() {
        // `delta_ms` truncates toward zero, so divisibility alone would accept any
        // offset in the open range `(grid point, grid point + 1ms)`. Both period
        // kinds verify the exact landing, so all of these are off-grid.
        let start = ts(2024, 1, 1, 0);
        for period in [
            Period::Fixed(Duration::hours(1)),
            Period::Fixed(Duration::milliseconds(1)),
            Period::Months(1),
        ] {
            let on_grid = period.add_to(start, 1).unwrap();
            assert_eq!(
                period.steps_between(start, on_grid).unwrap(),
                1,
                "{period:?}"
            );

            for offset in [
                Duration::nanoseconds(1),
                Duration::microseconds(1),
                Duration::microseconds(999),
            ] {
                assert!(
                    period.steps_between(start, on_grid + offset).is_err(),
                    "{period:?}: {offset:?} past a grid point must be off-grid"
                );
            }
        }

        // A grid whose *phase* is finer than a millisecond is still addressable at
        // its own points: an `initial_timestamp` keeps nanoseconds even though a
        // period does not.
        let offset_start = start + Duration::nanoseconds(500);
        let hourly = Period::Fixed(Duration::hours(1));
        assert_eq!(
            hourly
                .steps_between(offset_start, offset_start + Duration::hours(3))
                .unwrap(),
            3
        );
        // ...and rounding that phase away puts the bound off the grid.
        assert!(
            hourly
                .steps_between(offset_start, start + Duration::hours(3))
                .is_err()
        );

        // `floor_steps` / `ceil_steps` remain the lenient counterparts, by design.
        assert_eq!(
            hourly.floor_steps(start, start + Duration::hours(1) + Duration::nanoseconds(1)),
            1
        );
        assert_eq!(
            hourly.ceil_steps(start, start + Duration::hours(1) + Duration::nanoseconds(1)),
            2
        );
    }

    #[test]
    fn steps_between_fixed_and_months() {
        let start = ts(2024, 1, 1, 0);
        let res = Period::Fixed(Duration::hours(1));
        assert_eq!(res.steps_between(start, ts(2024, 1, 1, 5)).unwrap(), 5);
        assert!(
            res.steps_between(start, start + Duration::minutes(30))
                .is_err()
        );

        let monthly = Period::Months(1);
        assert_eq!(monthly.steps_between(start, ts(2024, 4, 1, 0)).unwrap(), 3);
        assert_eq!(monthly.steps_between(start, start).unwrap(), 0);
        // Off-grid day-of-month.
        assert!(monthly.steps_between(start, ts(2024, 4, 2, 0)).is_err());
        // Before start.
        assert!(monthly.steps_between(start, ts(2023, 12, 1, 0)).is_err());

        // Day-of-month verification: Jan 31 grid only lands on month-ends.
        let jan31 = ts(2024, 1, 31, 0);
        assert_eq!(monthly.steps_between(jan31, ts(2024, 3, 31, 0)).unwrap(), 2);
        assert!(monthly.steps_between(jan31, ts(2024, 2, 28, 0)).is_err());
    }

    #[test]
    fn divide_into() {
        let hour = Period::Fixed(Duration::hours(1));
        let day = Period::Fixed(Duration::hours(24));
        assert_eq!(hour.divide_into(&day).unwrap(), 24);
        assert!(
            hour.divide_into(&Period::Fixed(Duration::minutes(90)))
                .is_err()
        );

        let month = Period::Months(1);
        let year = Period::Months(12);
        assert_eq!(month.divide_into(&year).unwrap(), 12);
        assert_eq!(Period::Months(3).divide_into(&year).unwrap(), 4);

        // Mixed kinds are rejected.
        assert!(month.divide_into(&hour).is_err());
        assert!(hour.divide_into(&month).is_err());
    }
}
