//! Small, shared parsers for CLI/descriptor inputs: durations, timestamps,
//! owner categories, time-series-type names, and `key=value` features.

use chrono::{DateTime, Duration, TimeZone, Utc};
use time_series_store_core::{Dtype, FeatureValue, OwnerCategory, Period, TimeSeriesType};

/// Parse a period: an ISO-8601 duration (`PT1H`, `P1M`, `P1Y`) for calendar or
/// fixed periods, or the legacy human form (`1h`, `15min`, `7d`) which is always
/// a fixed-span [`Period::Fixed`].
pub fn parse_period(s: &str) -> Result<Period, String> {
    let s = s.trim();
    // ISO-8601 duration designators are uppercase `P`; the legacy human form
    // (`1h`, `7d`) never starts with `p`, so only `P` routes to the ISO parser.
    if s.starts_with('P') {
        Period::from_iso8601(s).map_err(|e| e.to_string())
    } else {
        Ok(Period::Fixed(parse_duration(s)?))
    }
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

/// Parse a human duration such as `1h`, `15min`, `500ms`, `7d`, or a bare
/// integer (interpreted as milliseconds). Supported units: `ms|s|min|h|d`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".to_string());
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(s.len());
    let (num, unit) = (s[..split].trim(), s[split..].trim());
    let n: i64 = num.parse().map_err(|_| format!("invalid duration '{s}'"))?;
    Ok(match unit {
        "" | "ms" => Duration::milliseconds(n),
        "s" => Duration::seconds(n),
        "min" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        other => {
            return Err(format!(
                "unknown duration unit '{other}' in '{s}' (use ms|s|min|h|d)"
            ));
        }
    })
}

/// Parse an RFC3339 timestamp, or a bare integer of epoch milliseconds.
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
    Err(format!(
        "invalid timestamp '{s}' (use RFC3339 like 2024-01-01T00:00:00Z or epoch milliseconds)"
    ))
}

/// Parse an owner category, accepting `component` / `supplemental_attribute`
/// (case-insensitive, with or without underscores).
pub fn parse_owner_category(s: &str) -> Result<OwnerCategory, String> {
    match s.to_ascii_lowercase().replace('_', "").as_str() {
        "component" => Ok(OwnerCategory::Component),
        "supplementalattribute" => Ok(OwnerCategory::SupplementalAttribute),
        _ => Err(format!(
            "invalid owner_category '{s}' (use 'component' or 'supplemental_attribute')"
        )),
    }
}

/// Parse a dtype name (`f64|f32|i64|i32|u64|bool`).
pub fn parse_dtype(s: &str) -> Result<Dtype, String> {
    Dtype::parse(s.trim())
        .ok_or_else(|| format!("invalid dtype '{s}' (use f64|f32|i64|i32|u64|bool)"))
}

/// Parse a time-series-type, accepting both short (`single`, `non_sequential`)
/// and full (`SingleTimeSeries`) spellings.
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
                "invalid time series type '{s}' (use single|non_sequential|deterministic|probabilistic|scenarios)"
            ));
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_round_trip() {
        assert_eq!(parse_duration("1h").unwrap(), Duration::hours(1));
        assert_eq!(parse_duration("15min").unwrap(), Duration::minutes(15));
        assert_eq!(
            parse_duration("500ms").unwrap(),
            Duration::milliseconds(500)
        );
        assert_eq!(parse_duration("7d").unwrap(), Duration::days(7));
        assert_eq!(parse_duration("250").unwrap(), Duration::milliseconds(250));
    }

    #[test]
    fn periods_round_trip() {
        use time_series_store_core::Period;
        assert_eq!(
            parse_period("1h").unwrap(),
            Period::Fixed(Duration::hours(1))
        );
        assert_eq!(
            parse_period("PT1H").unwrap(),
            Period::Fixed(Duration::hours(1))
        );
        assert_eq!(parse_period("P1M").unwrap(), Period::Months(1));
        assert_eq!(parse_period("P1Y").unwrap(), Period::Months(12));
        assert_eq!(format_period(Period::Fixed(Duration::hours(1))), "PT1H");
        assert_eq!(format_period(Period::Months(1)), "P1M");
        assert_eq!(format_period(Period::Months(12)), "P1Y");
    }

    #[test]
    fn bad_duration_unit_errors() {
        assert!(parse_duration("1w").is_err());
        assert!(parse_duration("abc").is_err());
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
