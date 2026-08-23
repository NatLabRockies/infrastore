//! Small, shared parsers for CLI/descriptor inputs: durations, timestamps,
//! owner categories, time-series-type names, and `key=value` features.

use std::sync::OnceLock;

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use infrastore_core::{
    ElementType, FeatureValue, OwnerCategory, Period, TimeReference, TimeSeriesType,
};

/// Representative ISO-8601 durations, for the error message a caller sees when
/// theirs did not parse. Covers both kinds: fixed spans and calendar ones.
const DURATION_EXAMPLES: &str = "PT1H, PT15M, PT30S, P1D, P1M, P1Y";

/// Parse a period from an ISO-8601 duration: `PT1H`, `PT15M`, `P1D` for fixed
/// spans, `P1M` / `P1Y` for calendar ones.
///
/// ISO-8601 is the only accepted spelling. The CLI used to take a human form
/// as well (`1h`, `15min`, `7d`), but nothing the CLI *printed* was ever spelled
/// that way — every rendered period goes through [`format_period`], which is
/// ISO-8601 — so a duration copied out of `list`, `info`, or `export -f json`
/// could not be pasted back into the descriptor it came from. The human form
/// also made a bare integer mean *milliseconds*, so `--resolution 24` quietly
/// meant 24ms rather than the day a reader of a power-systems tool assumes.
///
/// The retired grammar survives in [`legacy_suggestion`] as an error hint, so
/// hitting this is a one-line fix rather than a puzzle.
pub fn parse_period(s: &str) -> Result<Period, String> {
    let s = s.trim();
    Period::from_iso8601(s).map_err(|_| match legacy_suggestion(s) {
        Some(iso) => {
            format!("invalid duration '{s}': durations are ISO-8601 — did you mean '{iso}'?")
        }
        None => format!("invalid duration '{s}' (use an ISO-8601 duration: {DURATION_EXAMPLES})"),
    })
}

/// The ISO-8601 spelling of a duration written in the retired human form (`1h`,
/// `15min`, `500ms`, `7d`, or a bare integer of milliseconds), for the error
/// hint above. `None` when the input is not in that form either, in which case
/// there is nothing specific to suggest.
fn legacy_suggestion(s: &str) -> Option<String> {
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(s.len());
    let n: i64 = s[..split].trim().parse().ok()?;
    let d = match s[split..].trim() {
        "" | "ms" => Duration::milliseconds(n),
        "s" => Duration::seconds(n),
        "min" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        _ => return None,
    };
    Some(Period::Fixed(d).to_iso8601())
}

/// Render a [`Period`] as its canonical ISO-8601 duration string.
pub fn format_period(p: Period) -> String {
    p.to_iso8601()
}

/// `H = horizon / resolution` for periods, requiring an exact positive integer
/// (and matching calendar/fixed kinds).
pub fn period_horizon_steps(horizon: Period, resolution: Period) -> Result<usize, String> {
    resolution.divide_into(&horizon).map_err(|e| e.to_string())
}

/// How the CLI reads a *zoneless* timestamp, from the global
/// `--assume-timezone` / `--zoneless`. Unset means a zoneless timestamp is an
/// error.
///
/// A `OnceLock` set once from `main`, matching how the other global flags are
/// held (`confirm::set_assume_yes`, `color`): one command per process, and the
/// alternative is threading a parse setting through every command signature and
/// the whole descriptor-resolution chain.
static TIME_SPEC: OnceLock<Option<TimeSpec>> = OnceLock::new();

/// What `--assume-timezone` (or `--zoneless`) says a zoneless timestamp means.
///
/// The CLI is the one place in the system that runs the **local → instant**
/// direction. Every other binding is handed an already-resolved datetime object
/// by its own datetime library; the CLI is handed text, so it has to do the
/// resolution itself — which is why it is also the only layer that needs a tz
/// database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSpec {
    /// `--zoneless`: the timestamps *are* wall clocks. No conversion at all —
    /// the fields are read as they stand, and the series records
    /// [`TimeReference::Zoneless`].
    Zoneless,
    /// `--assume-timezone UTC`.
    Utc,
    /// `--assume-timezone -07:00`: one offset for every row.
    Offset(FixedOffset),
    /// `--assume-timezone America/Denver`: resolved per row against the tz
    /// database, which is what makes the two failure cases below reportable.
    Zone(chrono_tz::Tz),
}

impl TimeSpec {
    /// The spelling a timestamp read under this spec records.
    fn reference(self) -> TimeReference {
        match self {
            TimeSpec::Zoneless => TimeReference::Zoneless,
            TimeSpec::Utc => TimeReference::Utc,
            TimeSpec::Offset(offset) => TimeReference::FixedOffset(offset.local_minus_utc() / 60),
            TimeSpec::Zone(tz) => TimeReference::Zone(tz.name().to_string()),
        }
    }
}

fn assumed_spec() -> Option<TimeSpec> {
    TIME_SPEC.get().copied().flatten()
}

/// Parse the `--assume-timezone` value, three ways: `UTC` (or `Z`), a fixed UTC
/// offset spelled `+HH:MM` / `-HH:MM` / `+HHMM` / `+HH`, or an IANA zone name.
///
/// A named zone used to be refused here, on the grounds that a zoneless
/// timestamp in one is not always a single instant — daylight saving skips one
/// hour and repeats another — so the CLI would have to "either reject rows in
/// the middle of an ingest or silently pick one". Rejecting loudly, per row,
/// with both candidates named, is the acceptable half of that pair; silently
/// picking is still not, and [`parse_timestamp_with_reference`] does the former.
/// So the rationale is superseded, and this is now a three-way parse.
///
/// Preferring a named zone over a fixed offset matters for any series that
/// crosses a transition: a year of Denver data stamped `-07:00` renders every
/// timestamp after March an hour wrong, while `America/Denver` renders all of
/// them correctly.
pub fn parse_time_spec(s: &str) -> Result<TimeSpec, String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("utc") || t.eq_ignore_ascii_case("z") {
        return Ok(TimeSpec::Utc);
    }
    // Reuse chrono's own offset grammar by parsing a timestamp that is nothing
    // but the offset -- after widening the two *basic* ISO-8601 spellings onto
    // the extended one it takes. `parse_from_rfc3339` accepts `±HH:MM` alone,
    // while `date +%z` prints `-0700` and a whole-hour offset is usually written
    // `-07`; both are what someone reaches for, and neither would otherwise
    // parse.
    let digits = |rest: &str| rest.chars().all(|c| c.is_ascii_digit());
    let normalized = match t.len() {
        3 if t.starts_with(['+', '-']) && digits(&t[1..]) => format!("{t}:00"),
        5 if t.starts_with(['+', '-']) && digits(&t[1..]) => {
            format!("{}:{}", &t[..3], &t[3..])
        }
        _ => t.to_string(),
    };
    if normalized.starts_with(['+', '-'])
        && let Ok(dt) = DateTime::parse_from_rfc3339(&format!("1970-01-01T00:00:00{normalized}"))
    {
        return Ok(TimeSpec::Offset(*dt.offset()));
    }
    t.parse::<chrono_tz::Tz>().map(TimeSpec::Zone).map_err(|_| {
        format!(
            "invalid --assume-timezone '{s}' (use UTC, a fixed offset like +05:30 or -07:00, or \
             an IANA zone name like America/Denver)"
        )
    })
}

/// Record the global `--assume-timezone` / `--zoneless`, validating it. Called
/// once, from `main`, before anything parses a timestamp — so a bad zone fails
/// immediately rather than part-way through a CSV.
///
/// A second call is an error rather than a silent no-op. Discarding the failed
/// `set` would leave the first spec in place while the caller believes it
/// installed the second: in `main` that is unreachable, but the unit tests below
/// run in this same process and assert on how a zoneless timestamp parses, so a
/// swallowed second set is exactly the shape that turns them order-dependent
/// with no visible cause.
pub fn set_assumed_timezone(spec: Option<&str>, zoneless: bool) -> Result<(), String> {
    let resolved = match (spec, zoneless) {
        (Some(_), true) => {
            return Err(
                "--zoneless and --assume-timezone say different things about the same \
                 timestamps: one records them as wall clocks naming no instant, the other \
                 resolves them to instants. Pass one."
                    .to_string(),
            );
        }
        (Some(s), false) => Some(parse_time_spec(s)?),
        (None, true) => Some(TimeSpec::Zoneless),
        (None, false) => None,
    };
    TIME_SPEC
        .set(resolved)
        .map_err(|_| "the assumed timezone has already been set".to_string())?;
    Ok(())
}

/// Parse an RFC3339 timestamp, or a bare integer of epoch milliseconds, keeping
/// only the instant. See [`parse_timestamp_with_reference`], which is the same
/// parse with the spelling retained.
pub fn parse_timestamp(s: &str) -> Result<DateTime<Utc>, String> {
    parse_timestamp_with_reference(s).map(|(instant, _)| instant)
}

/// Parse a timestamp, returning the instant it names **and how it was spelled**.
///
/// Four inputs, four spellings:
///
/// * RFC3339 with `Z` → [`TimeReference::Utc`];
/// * RFC3339 with an offset → [`TimeReference::FixedOffset`], preserved rather
///   than consumed — the offset was in the file, and the store now has somewhere
///   to put it;
/// * epoch milliseconds → [`TimeReference::Utc`] (an epoch count names an
///   instant and nothing else);
/// * a *zoneless* timestamp (`2024-01-01T00:00:00`, or the
///   `2024-01-01 00:00:00` most CSV writers produce) → whatever
///   `--assume-timezone` / `--zoneless` says, and an error when neither was
///   given, because on its own it names no instant.
///
/// An offset the input carries itself is never overridden — the flag fills a
/// gap, it does not relabel data that already said what it meant.
///
/// Under `--assume-timezone <IANA name>` this is where local → instant actually
/// runs, and `chrono-tz` answers in three values. `Single` is ingested; the
/// repeated fall-back hour and the skipped spring-forward hour are **errors**,
/// naming the row's text and (for the fold) both candidate instants. Failing the
/// row loudly is the acceptable half of "reject rows mid-ingest or silently pick
/// one"; silently picking is not.
pub fn parse_timestamp_with_reference(s: &str) -> Result<(DateTime<Utc>, TimeReference), String> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        let offset_minutes = dt.offset().local_minus_utc() / 60;
        // `Z` and `+00:00` are the same instant and a different claim. chrono
        // does not keep which one it read, so the text is asked directly.
        let reference = if offset_minutes == 0 && s.ends_with(['Z', 'z']) {
            TimeReference::Utc
        } else {
            TimeReference::FixedOffset(offset_minutes)
        };
        return Ok((dt.with_timezone(&Utc), reference));
    }
    if let Ok(ms) = s.parse::<i64>() {
        return Utc
            .timestamp_millis_opt(ms)
            .single()
            .map(|dt| (dt, TimeReference::Utc))
            .ok_or_else(|| format!("invalid epoch-ms timestamp '{s}'"));
    }
    if let Some(naive) = parse_zoneless(s) {
        let Some(spec) = assumed_spec() else {
            return Err(format!(
                "timestamp '{s}' names no time zone, so it names no instant. Give it an offset \
                 (RFC3339, like 2024-01-01T00:00:00Z), pass --assume-timezone UTC (a fixed \
                 offset like -07:00, or an IANA name like America/Denver) to read every \
                 zoneless timestamp with it, or pass --zoneless to store them as the wall \
                 clocks they are."
            ));
        };
        let reference = spec.reference();
        let instant = match spec {
            // No conversion: a wall clock is stored as its own fields, exactly
            // as every other binding holds a zoneless timestamp. Reading it
            // through the machine's local zone -- which is what any "convert"
            // step would do -- would make the same file ingest differently in
            // Denver than in CI.
            TimeSpec::Zoneless => naive.and_utc(),
            TimeSpec::Utc => naive.and_utc(),
            TimeSpec::Offset(offset) => naive
                .and_local_timezone(offset)
                .single()
                .map(|dt| dt.with_timezone(&Utc))
                .ok_or_else(|| {
                    format!("timestamp '{s}' is not a valid instant at offset {offset}")
                })?,
            TimeSpec::Zone(tz) => resolve_in_zone(naive, tz, s)?,
        };
        return Ok((instant, reference));
    }
    Err(format!(
        "invalid timestamp '{s}' (use RFC3339 like 2024-01-01T00:00:00Z or epoch milliseconds)"
    ))
}

/// The local → instant direction for a named zone, with both of its partial
/// answers turned into errors that name the row.
///
/// This is the whole reason a named zone is accepted at all: `chrono-tz` reports
/// the gap and the fold rather than guessing, so the CLI can refuse the row
/// instead of storing one of two instants and never saying which.
fn resolve_in_zone(
    naive: NaiveDateTime,
    tz: chrono_tz::Tz,
    raw: &str,
) -> Result<DateTime<Utc>, String> {
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
        chrono::LocalResult::Ambiguous(first, second) => Err(format!(
            "timestamp '{raw}' is ambiguous in {tz}: daylight saving repeats that wall clock, \
             so it names two instants ({} and {}). The file has to say which — give the row an \
             explicit offset, or re-read the column with --assume-timezone {} or {}.",
            first.with_timezone(&Utc).to_rfc3339(),
            second.with_timezone(&Utc).to_rfc3339(),
            first.offset().fix(),
            second.offset().fix(),
        )),
        chrono::LocalResult::None => Err(format!(
            "timestamp '{raw}' does not exist in {tz}: daylight saving skips that wall clock, so \
             it names no instant. Check the row, or re-read the column with --assume-timezone \
             set to the offset the data actually uses."
        )),
    }
}

/// The zoneless spellings [`parse_timestamp`] recognizes: ISO-8601 with a `T` or
/// the space separator CSV writers use, seconds and fractional seconds optional,
/// and a bare date (which is midnight).
fn parse_zoneless(s: &str) -> Option<NaiveDateTime> {
    const DATETIME_FORMATS: [&str; 6] = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
    ];
    for format in DATETIME_FORMATS {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, format) {
            return Some(dt);
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// A half-open `START..END` time range, as `get`, `grid`, and `export` spell it.
pub type TimeRange = infrastore_core::TimeRange;

/// Parse a `START..END` time range, each end an RFC3339 timestamp or epoch-ms.
///
/// The bounds carry their *spelling* through, not just their instant: the store
/// refuses a wall-clock bound against a series that records instants, and an
/// instant bound against a zoneless one, rather than coercing either. Both ends
/// have to agree — a range is one request.
pub fn parse_time_range(spec: Option<&str>) -> Result<Option<TimeRange>, String> {
    let Some(spec) = spec else {
        return Ok(None);
    };
    let (start, end) = spec
        .split_once("..")
        .ok_or_else(|| format!("invalid --time-range '{spec}' (expected START..END)"))?;
    let (start_instant, start_reference) = parse_timestamp_with_reference(start)?;
    let (end_instant, end_reference) = parse_timestamp_with_reference(end)?;
    if start_reference.is_zoneless() != end_reference.is_zoneless() {
        return Err(format!(
            "the two ends of --time-range '{spec}' are spelled differently: one names an \
             instant and the other is a bare wall clock. Spell both the way the series is."
        ));
    }
    Ok(Some(TimeRange::spelled(
        start_instant,
        end_instant,
        start_reference.is_zoneless(),
    )))
}

/// Parse an owner category. `Component` / `SupplementalAttribute` are the
/// canonical spellings — what the CLI prints, and what `template` now writes —
/// but matching is case-insensitive and ignores underscores, so the
/// `supplemental_attribute` form that flags and older descriptors use keeps
/// working.
pub fn parse_owner_category(s: &str) -> Result<OwnerCategory, String> {
    match s.to_ascii_lowercase().replace('_', "").as_str() {
        "component" => Ok(OwnerCategory::Component),
        "supplementalattribute" => Ok(OwnerCategory::SupplementalAttribute),
        _ => Err(format!(
            "invalid owner_category '{s}' (use 'Component' or 'SupplementalAttribute')"
        )),
    }
}

/// Parse an `element_type` in its canonical string form. This is the only
/// element-typing input the descriptor takes: the physical dtype the CSV cells
/// are parsed as is `ElementType::physical_dtype`.
pub fn parse_element_type(s: &str) -> Result<ElementType, String> {
    s.trim().parse::<ElementType>().map_err(|e| e.to_string())
}

/// The canonical `--type` / descriptor `type` spellings, in the order the help
/// and error text list them. These are the names the core prints
/// (`TimeSeriesType::as_str`) and the ones `template` writes. Kept in one place
/// so the flag help, the error message, and [`parse_ts_type`] cannot drift
/// apart.
pub const TS_TYPE_NAMES: &str = "SingleTimeSeries|NonSequentialTimeSeries|Deterministic|\
                                 DeterministicSingleTimeSeries|Probabilistic|Scenarios";

/// The lowercase shorthands [`parse_ts_type`] also accepts, for the "and these
/// work too" half of the help and error text.
pub const TS_TYPE_SHORT_NAMES: &str = "single|non_sequential|deterministic|deterministic_single|\
                                       probabilistic|scenarios";

/// Parse a time-series type, accepting both the canonical spelling
/// (`SingleTimeSeries`) and the short one (`single`). Matching is
/// case-insensitive and ignores underscores.
pub fn parse_ts_type(s: &str) -> Result<TimeSeriesType, String> {
    Ok(match s.to_ascii_lowercase().replace('_', "").as_str() {
        "single" | "singletimeseries" => TimeSeriesType::SingleTimeSeries,
        "nonsequential" | "nonsequentialtimeseries" => TimeSeriesType::NonSequentialTimeSeries,
        "deterministic" => TimeSeriesType::Deterministic,
        "deterministicsingle" | "deterministicsingletimeseries" => {
            TimeSeriesType::DeterministicSingleTimeSeries
        }
        "probabilistic" => TimeSeriesType::Probabilistic,
        "scenarios" => TimeSeriesType::Scenarios,
        _ => {
            return Err(format!(
                "invalid time series type '{s}' (use {TS_TYPE_NAMES}; \
                 the short forms {TS_TYPE_SHORT_NAMES} are accepted too)"
            ));
        }
    })
}

/// Validate and normalize a content-hash prefix for `--data-hash`.
///
/// Accepts 1-64 hex characters in either case and returns them lowercased, so a
/// hash pasted from SQLite's `hex()` (which returns uppercase) matches one
/// printed by the CLI or stored in the `time_series_readable` view (both
/// lowercase). A prefix is enough — full 64-character hashes are unwieldy to
/// type and the short form the tables print is the natural thing to copy.
pub fn parse_hash_prefix(s: &str) -> Result<String, String> {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 {
        return Err(format!(
            "invalid --data-hash '{s}' (expected 1-64 hex characters)"
        ));
    }
    if let Some(bad) = s.chars().find(|c| !c.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid --data-hash '{s}': '{bad}' is not a hex character"
        ));
    }
    Ok(s.to_ascii_lowercase())
}

/// Parse a `key=value` feature pair, inferring the value type as int, float,
/// bool, or (fallback) string.
pub fn parse_feature_kv(pair: &str) -> Result<(String, FeatureValue), String> {
    let (key, value) = pair
        .split_once('=')
        .ok_or_else(|| format!("invalid feature '{pair}' (expected key=value)"))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("invalid feature '{pair}' (empty key)"));
    }
    Ok((key.to_string(), infer_feature_value(value.trim())))
}

/// Infer a [`FeatureValue`] from a raw string: int, then float, then bool,
/// otherwise a string.
pub fn infer_feature_value(s: &str) -> FeatureValue {
    if let Ok(i) = s.parse::<i64>() {
        return FeatureValue::Int(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return FeatureValue::Float(f);
    }
    match s.to_ascii_lowercase().as_str() {
        "true" => return FeatureValue::Bool(true),
        "false" => return FeatureValue::Bool(false),
        _ => {}
    }
    FeatureValue::Str(s.to_string())
}

/// Convert a JSON scalar into a [`FeatureValue`] for descriptor `features`.
pub fn feature_from_json(key: &str, v: &serde_json::Value) -> Result<FeatureValue, String> {
    Ok(match v {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                FeatureValue::Int(i)
            } else if let Some(f) = n.as_f64() {
                FeatureValue::Float(f)
            } else {
                return Err(format!(
                    "feature '{key}' has a number that cannot be represented as i64 or f64"
                ));
            }
        }
        serde_json::Value::Bool(b) => FeatureValue::Bool(*b),
        serde_json::Value::String(s) => FeatureValue::Str(s.clone()),
        other => {
            let type_name = match other {
                serde_json::Value::Null => "null",
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Object(_) => "object",
                _ => unreachable!(),
            };
            return Err(format!(
                "feature '{key}' has unsupported type {type_name}; use int, float, bool, or string"
            ));
        }
    })
}

/// Parse a compression spec: `none`, `deflate`, or `deflate:LEVEL` (0-9).
/// `shuffle` is threaded from its own flag.
pub fn parse_compression(
    spec: &str,
    shuffle: bool,
) -> Result<infrastore_core::Compression, String> {
    use infrastore_core::Compression;
    match spec.to_ascii_lowercase().as_str() {
        "none" => Ok(Compression::None),
        "deflate" => Ok(Compression::Deflate { level: 3, shuffle }),
        other => {
            let level = other
                .strip_prefix("deflate:")
                .and_then(|l| l.parse::<u8>().ok())
                .ok_or_else(|| {
                    format!("invalid --compression '{spec}' (use none, deflate, or deflate:LEVEL)")
                })?;
            Ok(Compression::Deflate { level, shuffle })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every offset spelling the doc comment promises must actually parse.
    ///
    /// The extended form is all `DateTime::parse_from_rfc3339` takes on its own;
    /// the two basic forms are widened onto it first, and `date +%z` prints one
    /// of them.
    #[test]
    fn every_documented_offset_spelling_parses() {
        let east =
            |h: i32, m: i32| TimeSpec::Offset(FixedOffset::east_opt(h * 3600 + m * 60).unwrap());
        for (spec, want) in [
            ("UTC", TimeSpec::Utc),
            ("utc", TimeSpec::Utc),
            ("Z", TimeSpec::Utc),
            ("-07:00", east(-7, 0)),
            ("+05:30", east(5, 30)),
            ("-0700", east(-7, 0)),
            ("+0530", east(5, 30)),
            ("-07", east(-7, 0)),
            ("+08", east(8, 0)),
            ("  -07:00  ", east(-7, 0)),
        ] {
            assert_eq!(
                parse_time_spec(spec).unwrap(),
                want,
                "{spec} did not parse to what it names"
            );
        }
    }

    #[test]
    fn a_named_zone_is_now_accepted_as_itself() {
        // The refusal this replaced said a named zone would have to "either
        // reject rows in the middle of an ingest or silently pick one".
        // Rejecting loudly, per row, with both candidates named, is the
        // acceptable half of that pair — see `resolve_in_zone` — so the zone is
        // now taken as a zone rather than collapsed onto one offset.
        assert_eq!(
            parse_time_spec("America/Denver").unwrap(),
            TimeSpec::Zone(chrono_tz::America::Denver)
        );
        assert_eq!(
            TimeSpec::Zone(chrono_tz::America::Denver).reference(),
            TimeReference::Zone("America/Denver".into())
        );

        // Nothing that merely looks offset- or zone-shaped slips through.
        for bad in [
            "07:00",
            "-7:00",
            "-070",
            "-07:0",
            "+2400x",
            "",
            "-",
            "Mars/Olympus",
        ] {
            assert!(
                parse_time_spec(bad).is_err(),
                "{bad:?} must not parse as a time zone"
            );
        }
    }

    #[test]
    fn a_timestamps_own_spelling_is_preserved_not_consumed() {
        // The offset was in the file; the store now has somewhere to put it.
        let (instant, reference) = parse_timestamp_with_reference("2024-01-01T00:00:00-07:00")
            .expect("an RFC3339 timestamp with an offset parses");
        assert_eq!(instant.to_rfc3339(), "2024-01-01T07:00:00+00:00");
        assert_eq!(reference, TimeReference::FixedOffset(-420));

        // `Z` and `+00:00` are the same instant and a different claim.
        assert_eq!(
            parse_timestamp_with_reference("2024-01-01T00:00:00Z")
                .unwrap()
                .1,
            TimeReference::Utc
        );
        assert_eq!(
            parse_timestamp_with_reference("2024-01-01T00:00:00+00:00")
                .unwrap()
                .1,
            TimeReference::FixedOffset(0)
        );

        // An epoch count names an instant and nothing else.
        assert_eq!(
            parse_timestamp_with_reference("0").unwrap().1,
            TimeReference::Utc
        );
    }

    #[test]
    fn periods_round_trip() {
        use infrastore_core::Period;
        assert_eq!(
            parse_period("PT1H").unwrap(),
            Period::Fixed(Duration::hours(1))
        );
        assert_eq!(
            parse_period("PT15M").unwrap(),
            Period::Fixed(Duration::minutes(15))
        );
        assert_eq!(parse_period("P1M").unwrap(), Period::Months(1));
        assert_eq!(parse_period("P1Y").unwrap(), Period::Months(12));
        assert_eq!(format_period(Period::Fixed(Duration::hours(1))), "PT1H");
        assert_eq!(format_period(Period::Months(1)), "P1M");
        assert_eq!(format_period(Period::Months(12)), "P1Y");
    }

    /// Every period the CLI renders must be one the CLI can read back. This is
    /// the property the human duration form broke, and the reason it is gone.
    #[test]
    fn every_rendered_period_parses_back() {
        use infrastore_core::Period;
        for p in [
            Period::Fixed(Duration::hours(1)),
            Period::Fixed(Duration::minutes(15)),
            Period::Fixed(Duration::seconds(30)),
            Period::Fixed(Duration::milliseconds(500)),
            Period::Fixed(Duration::days(7)),
            Period::Months(1),
            Period::Months(3),
            Period::Months(12),
        ] {
            let rendered = format_period(p);
            assert_eq!(parse_period(&rendered).unwrap(), p, "round trip {rendered}");
        }
    }

    /// The human form is rejected, but with the ISO-8601 translation attached —
    /// the whole point of keeping the retired grammar around as a hint.
    #[test]
    fn the_retired_human_form_is_rejected_with_a_suggestion() {
        for (human, iso) in [
            ("1h", "PT1H"),
            ("15min", "PT15M"),
            ("500ms", "PT0.5S"),
            ("7d", "P7D"),
            // A bare integer used to mean milliseconds, which is exactly the
            // reading a suggestion should make explicit rather than silent.
            ("24", "PT0.024S"),
        ] {
            let err = parse_period(human).expect_err("human form is no longer accepted");
            assert!(
                err.contains(iso),
                "error for '{human}' should suggest '{iso}': {err}"
            );
        }
    }

    #[test]
    fn unparseable_durations_error_without_a_suggestion() {
        let err = parse_period("abc").unwrap_err();
        assert!(err.contains("ISO-8601"), "{err}");
        assert!(!err.contains("did you mean"), "{err}");
        assert!(parse_period("1w").is_err());
        assert!(parse_period("").is_err());
    }

    #[test]
    fn timestamps() {
        let dt = parse_timestamp("2024-01-01T00:00:00Z").unwrap();
        assert_eq!(dt, Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap());
        assert_eq!(
            parse_timestamp("0").unwrap(),
            Utc.timestamp_millis_opt(0).single().unwrap()
        );
        assert!(parse_timestamp("not-a-time").is_err());
    }

    #[test]
    fn features_infer_types() {
        assert_eq!(
            parse_feature_kv("year=2030").unwrap(),
            ("year".to_string(), FeatureValue::Int(2030))
        );
        assert_eq!(
            parse_feature_kv("scale=0.5").unwrap(),
            ("scale".to_string(), FeatureValue::Float(0.5))
        );
        assert_eq!(
            parse_feature_kv("on=true").unwrap(),
            ("on".to_string(), FeatureValue::Bool(true))
        );
        assert_eq!(
            parse_feature_kv("region=west").unwrap(),
            ("region".to_string(), FeatureValue::Str("west".to_string()))
        );
        assert!(parse_feature_kv("noeq").is_err());
    }

    #[test]
    fn ts_type_aliases() {
        assert_eq!(
            parse_ts_type("single").unwrap(),
            TimeSeriesType::SingleTimeSeries
        );
        assert_eq!(
            parse_ts_type("NonSequentialTimeSeries").unwrap(),
            TimeSeriesType::NonSequentialTimeSeries
        );
        assert!(parse_ts_type("bogus").is_err());
    }
}
