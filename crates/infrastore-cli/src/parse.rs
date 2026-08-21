//! Small, shared parsers for CLI/descriptor inputs: durations, timestamps,
//! owner categories, time-series-type names, and `key=value` features.

use std::sync::OnceLock;

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use infrastore_core::{ElementType, FeatureValue, OwnerCategory, Period, TimeSeriesType};

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

/// The offset a *zoneless* timestamp is read with, from the global
/// `--assume-timezone`. Unset means a zoneless timestamp is an error.
///
/// A `OnceLock` set once from `main`, matching how the other global flags are
/// held (`confirm::set_assume_yes`, `color`): one command per process, and the
/// alternative is threading a parse setting through every command signature and
/// the whole descriptor-resolution chain.
static ASSUMED_OFFSET: OnceLock<Option<FixedOffset>> = OnceLock::new();

/// Record the global `--assume-timezone`, validating it. Called once, from
/// `main`, before anything parses a timestamp — so a bad zone fails immediately
/// rather than part-way through a CSV.
pub fn set_assumed_timezone(spec: Option<&str>) -> Result<(), String> {
    let offset = match spec {
        None => None,
        Some(s) => Some(parse_utc_offset(s)?),
    };
    let _ = ASSUMED_OFFSET.set(offset);
    Ok(())
}

fn assumed_offset() -> Option<FixedOffset> {
    ASSUMED_OFFSET.get().copied().flatten()
}

/// Parse the `--assume-timezone` value: `UTC` (or `Z`), or a fixed UTC offset
/// spelled `+HH:MM`, `-HH:MM`, `+HHMM`, or `+HH`.
///
/// Deliberately **not** an IANA zone name. A zoneless timestamp in a named zone
/// is not always one instant: an hour that daylight saving skips names none, and
/// the hour it repeats names two, so `America/Denver` would have to either
/// reject rows in the middle of an ingest or silently pick one. Data written
/// zoneless in this domain is almost always local *standard* time — a fixed
/// offset year-round, which is exactly what this takes — and data that really is
/// civil time with DST should carry its offsets in the file, where each row can
/// say which side of the transition it is on.
pub fn parse_utc_offset(s: &str) -> Result<FixedOffset, String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("utc") || t.eq_ignore_ascii_case("z") {
        return Ok(FixedOffset::east_opt(0).expect("zero is a valid offset"));
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
    DateTime::parse_from_rfc3339(&format!("1970-01-01T00:00:00{normalized}"))
        .map(|dt| *dt.offset())
        .map_err(|_| {
            if t.contains('/') {
                format!(
                    "invalid --assume-timezone '{s}': named zones are not accepted, because a \
                     zoneless timestamp in one is not always a single instant (daylight saving \
                     skips one hour and repeats another). Pass the fixed offset the data uses, \
                     e.g. -07:00 for Mountain Standard Time, or UTC."
                )
            } else {
                format!(
                    "invalid --assume-timezone '{s}' (use UTC, or a fixed offset like +05:30 or \
                     -07:00)"
                )
            }
        })
}

/// Parse an RFC3339 timestamp, or a bare integer of epoch milliseconds.
///
/// A *zoneless* timestamp (`2024-01-01T00:00:00`, or the `2024-01-01 00:00:00`
/// that most CSV writers produce) names no instant on its own, so it is accepted
/// only when `--assume-timezone` says which offset to read it with. An offset
/// the input carries itself is never overridden — the flag fills a gap, it does
/// not relabel data that already said what it meant.
pub fn parse_timestamp(s: &str) -> Result<DateTime<Utc>, String> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(ms) = s.parse::<i64>() {
        return Utc
            .timestamp_millis_opt(ms)
            .single()
            .ok_or_else(|| format!("invalid epoch-ms timestamp '{s}'"));
    }
    if let Some(naive) = parse_zoneless(s) {
        let Some(offset) = assumed_offset() else {
            return Err(format!(
                "timestamp '{s}' names no time zone, so it names no instant. Give it an offset \
                 (RFC3339, like 2024-01-01T00:00:00Z), or pass --assume-timezone UTC (or a fixed \
                 offset like -07:00) to read every zoneless timestamp with it."
            ));
        };
        return naive
            .and_local_timezone(offset)
            .single()
            .map(|dt| dt.with_timezone(&Utc))
            .ok_or_else(|| format!("timestamp '{s}' is not a valid instant at offset {offset}"));
    }
    Err(format!(
        "invalid timestamp '{s}' (use RFC3339 like 2024-01-01T00:00:00Z or epoch milliseconds)"
    ))
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
pub type TimeRange = (DateTime<Utc>, DateTime<Utc>);

/// Parse a `START..END` time range, each end an RFC3339 timestamp or epoch-ms.
pub fn parse_time_range(spec: Option<&str>) -> Result<Option<TimeRange>, String> {
    let Some(spec) = spec else {
        return Ok(None);
    };
    let (start, end) = spec
        .split_once("..")
        .ok_or_else(|| format!("invalid --time-range '{spec}' (expected START..END)"))?;
    Ok(Some((parse_timestamp(start)?, parse_timestamp(end)?)))
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
        let east = |h: i32, m: i32| FixedOffset::east_opt(h * 3600 + m * 60).unwrap();
        for (spec, want) in [
            ("UTC", east(0, 0)),
            ("utc", east(0, 0)),
            ("Z", east(0, 0)),
            ("-07:00", east(-7, 0)),
            ("+05:30", east(5, 30)),
            ("-0700", east(-7, 0)),
            ("+0530", east(5, 30)),
            ("-07", east(-7, 0)),
            ("+08", east(8, 0)),
            ("  -07:00  ", east(-7, 0)),
        ] {
            assert_eq!(
                parse_utc_offset(spec).unwrap(),
                want,
                "{spec} did not parse to the offset it names"
            );
        }
    }

    #[test]
    fn a_named_zone_is_refused_with_the_ambiguity_as_the_reason() {
        let err = parse_utc_offset("America/Denver").unwrap_err();
        assert!(err.contains("named zones are not accepted"), "{err}");
        assert!(err.contains("daylight saving"), "{err}");

        // Nothing that merely looks offset-shaped slips through.
        for bad in ["07:00", "-7:00", "-070", "-07:0", "+2400x", "", "-"] {
            assert!(
                parse_utc_offset(bad).is_err(),
                "{bad:?} must not parse as an offset"
            );
        }
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
