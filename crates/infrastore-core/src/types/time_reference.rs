//! How a series' timestamps were *spelled* on the way in.
//!
//! The store records instants. [`TimeReference`] records the spelling those
//! instants arrived in, so a series round-trips as it was written instead of
//! being relabelled UTC at every boundary.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Result, TimeSeriesError};

/// The longest IANA zone name is well under this; the bound exists so a stray
/// blob cannot become a zone name, not to police the database.
const MAX_ZONE_NAME_LEN: usize = 64;

/// The storage spelling of [`TimeReference::Utc`]. Lowercase, which is what
/// keeps it distinguishable from the IANA zone `UTC` — `ZoneInfo("UTC")`
/// records `Zone("UTC")`, not `Utc`, and the two must not collapse.
pub const UTC_LITERAL: &str = "utc";
/// The storage spelling of [`TimeReference::Zoneless`].
pub const ZONELESS_LITERAL: &str = "zoneless";

/// How a time series' timestamps were written.
///
/// Three of the four variants are *zoned* — they name an instant — and
/// [`Self::Zoneless`] is not. Most rules in the store split on that binary
/// ([`Self::is_zoneless`]) rather than on the four variants.
///
/// # A spelling, not a grid
///
/// A reference records how timestamps were *written*. It does not change how
/// the grid is *stepped*: `resolution` and `interval` are durations, so an
/// hourly series has hourly **instants** whatever its reference says. Rendering
/// an hourly `Zone("America/Denver")` series across the November fall-back gives
/// `01:00-06:00`, `01:00-07:00`, `02:00-07:00` — two identical wall clocks, two
/// distinct instants, correctly ordered.
///
/// A *local-clock* grid — hourly by the clock, so a 23-hour day in March and a
/// 25-hour one in November — is a different thing, and is inexpressible in
/// `SingleTimeSeries` and the dense forecasts: their grid is a
/// [`crate::Period`], a fixed count of milliseconds. Use
/// [`crate::NonSequentialTimeSeries`], which carries an explicit instant per
/// value, so the caller derives those days and the data records them.
///
/// # Months step on the UTC calendar
///
/// [`crate::Period::Months`] is calendar arithmetic and has to be told *which*
/// calendar. It uses the stored UTC one, and the reference does not redirect it;
/// a zoned series whose resolution or interval is a month period is warned about
/// on write. Local-frame stepping would be the local → instant direction, which
/// the core deliberately never runs (see below), and would let the reference
/// decide which instants a series contains.
///
/// # Why a named zone is safe here
///
/// The ambiguity a named zone is feared for lives in the **local → instant**
/// direction, and this crate never runs it. On input that direction has already
/// happened in the caller's own datetime library; on output the store runs only
/// **instant → local**, which is total and single-valued. So the core holds a
/// zone name opaquely and never resolves it — no tz database, and none of the
/// three the bindings already ship gets a fourth appointed over it. See
/// [`Self::validate`] for the shape check that *is* applied.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TimeReference {
    /// An instant, written as UTC.
    Utc,
    /// An instant, written at a fixed offset from UTC, in **minutes east**.
    ///
    /// A whole series stamped with one offset renders every timestamp at that
    /// offset forever, transitions included — which is right for data that was
    /// genuinely written that way and wrong for a local series that crosses a
    /// DST boundary. That case wants [`Self::Zone`].
    FixedOffset(i32),
    /// An instant, written in a named IANA zone. Held opaquely: the core records
    /// the name and never resolves it.
    ///
    /// Rendering a stored instant in a named zone is tz-database-dependent, so a
    /// retroactive change to a jurisdiction's rules moves the displayed local
    /// time. The store records the instant; the label is a rendering hint.
    Zone(String),
    /// A wall clock. Names no instant; the store holds it as if UTC.
    Zoneless,
}

impl TimeReference {
    /// Whether this reference names an instant. `false` only for
    /// [`Self::Zoneless`].
    pub fn is_zoned(&self) -> bool {
        !matches!(self, TimeReference::Zoneless)
    }

    /// Whether this reference is [`Self::Zoneless`]. The partition every
    /// coherence rule in the store splits on.
    pub fn is_zoneless(&self) -> bool {
        matches!(self, TimeReference::Zoneless)
    }

    /// Whether an *unset* reference (`None`) or `self` accepts a zoned query
    /// bound. `None` groups with the zoned variants: an unspecified spelling is
    /// not a floating third case.
    pub fn accepts_zoned_bound(reference: Option<&TimeReference>) -> bool {
        !matches!(reference, Some(TimeReference::Zoneless))
    }

    /// The catalog / wire spelling: `"utc"`, `"zoneless"`, `"-07:00"`, or the
    /// zone name verbatim.
    ///
    /// One TEXT column holds all four unambiguously because [`Self::validate`]
    /// refuses a zone name that could be read as either literal or as an offset.
    pub fn as_storage_string(&self) -> String {
        match self {
            TimeReference::Utc => UTC_LITERAL.to_string(),
            TimeReference::Zoneless => ZONELESS_LITERAL.to_string(),
            TimeReference::FixedOffset(minutes) => format_offset(*minutes),
            TimeReference::Zone(name) => name.clone(),
        }
    }

    /// Inverse of [`Self::as_storage_string`].
    ///
    /// The literals are matched first, then the offset grammar, then the zone
    /// name — the same order [`Self::validate`] rules out, so parse and validate
    /// cannot disagree about which variant a string names.
    pub fn parse(s: &str) -> Result<Self> {
        if s == UTC_LITERAL {
            return Ok(TimeReference::Utc);
        }
        if s == ZONELESS_LITERAL {
            return Ok(TimeReference::Zoneless);
        }
        if let Some(minutes) = parse_offset(s) {
            return Ok(TimeReference::FixedOffset(minutes));
        }
        let zone = TimeReference::Zone(s.to_string());
        zone.validate()?;
        Ok(zone)
    }

    /// Check what the core can check without a tz database.
    ///
    /// A [`Self::FixedOffset`] must be a real offset (strictly within a day of
    /// UTC). A [`Self::Zone`] name must be non-empty, bounded in length, match
    /// the IANA name grammar, and — the load-bearing part — must not read as an
    /// offset or as either storage literal, which is what lets one TEXT column
    /// hold all four spellings.
    ///
    /// Existence is deliberately *not* checked: `America/Dever` passes here. The
    /// core would need a tz database to catch it, and adding one would appoint a
    /// fourth database gatekeeper over the three the bindings already ship,
    /// coupling legitimate data (a zone IANA added last month) to this crate's
    /// release cadence. The layers that *have* a database audit the name
    /// instead — the CLI via `chrono-tz`, Python via `zoneinfo`, Julia via
    /// `TimeZones` — and store it either way. The CLI warns when a descriptor
    /// declares a name it does not recognize; `infrastore store-info` marks any
    /// such name in an existing store, which is what makes a typo findable in
    /// one command rather than at some later read in some other language.
    pub fn validate(&self) -> Result<()> {
        let invalid = |msg: String| Err(TimeSeriesError::InvalidParameter(msg));
        match self {
            TimeReference::Utc | TimeReference::Zoneless => Ok(()),
            TimeReference::FixedOffset(minutes) => {
                // `unsigned_abs`, not `abs`: `i32::MIN` has no positive
                // counterpart, so `abs()` panics on it in debug and wraps back
                // to `i32::MIN` in release -- where the comparison below then
                // reads as *in range* and admits the one value furthest from
                // being a real offset. A native caller can name it directly.
                if minutes.unsigned_abs() >= 24 * 60 {
                    return invalid(format!(
                        "time reference offset {minutes} minutes is not a real UTC offset; \
                         it must be strictly within a day of UTC"
                    ));
                }
                Ok(())
            }
            TimeReference::Zone(name) => {
                if name.is_empty() {
                    return invalid("time reference zone name is empty".into());
                }
                if name.len() > MAX_ZONE_NAME_LEN {
                    return invalid(format!(
                        "time reference zone name is {} bytes, over the {MAX_ZONE_NAME_LEN}-byte \
                         limit; no IANA name is anywhere near that long",
                        name.len()
                    ));
                }
                if name == UTC_LITERAL || name == ZONELESS_LITERAL {
                    return invalid(format!(
                        "{name:?} is the storage spelling of a non-zone time reference, so it \
                         cannot also be a zone name; the IANA zone is spelled \"UTC\""
                    ));
                }
                if parse_offset(name).is_some() {
                    return invalid(format!(
                        "time reference zone name {name:?} reads as a fixed UTC offset; \
                         declare it as one instead"
                    ));
                }
                if !is_iana_shaped(name) {
                    return invalid(format!(
                        "time reference zone name {name:?} is not shaped like an IANA name \
                         (slash-separated components of letters, digits, '_', '+' or '-', \
                         each starting with a letter), e.g. \"America/Denver\""
                    ));
                }
                Ok(())
            }
        }
    }
}

/// `±HH:MM`, the spelling RFC 3339 uses and [`parse_offset`] reads back.
fn format_offset(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
}

/// Read `±HH:MM` (and the `±HHMM` / `±HH` spellings a human types) as minutes
/// east. `None` if the string is not an offset at all — which is also how
/// [`TimeReference::validate`] proves a zone name is not one in disguise.
fn parse_offset(s: &str) -> Option<i32> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    // The arms below split `rest` at *byte* indices, so nothing but ASCII may
    // reach them: a multi-byte character makes `rest.len()` disagree with the
    // character count, and `&rest[..2]` then panics on a boundary inside it
    // (`+aéb` is four bytes). Restricting to the digits and the separator
    // an offset can contain also keeps a sign out of the components, so
    // `-+700` is not an offset in disguise.
    if !rest.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return None;
    }
    let (hours, minutes) = match rest.len() {
        5 if rest.as_bytes()[2] == b':' => (&rest[..2], &rest[3..]),
        4 => (&rest[..2], &rest[2..]),
        2 => (rest, "0"),
        _ => return None,
    };
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

/// The IANA zone-name grammar, loosely: slash-separated components of letters,
/// digits, `_`, `+` and `-`, each starting with a letter. Wide enough for the
/// awkward real names (`Etc/GMT+5`, `America/Argentina/Buenos_Aires`, `W-SU`,
/// `EST5EDT`) and narrow enough that nothing else in the column can pass.
fn is_iana_shaped(name: &str) -> bool {
    let mut components = 0usize;
    for component in name.split('/') {
        components += 1;
        if components > 3 {
            return false;
        }
        let mut chars = component.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() => {}
            _ => return false,
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')) {
            return false;
        }
    }
    true
}

impl fmt::Display for TimeReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_storage_string())
    }
}

impl FromStr for TimeReference {
    type Err = TimeSeriesError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Serde rides on the storage spelling rather than the derived variant shape, so
/// a value round-tripped through JSON and back into SQLite, the proto, the C
/// ABI, Python, Julia, or a CLI descriptor is the same string everywhere.
impl Serialize for TimeReference {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_storage_string())
    }
}

impl<'de> Deserialize<'de> for TimeReference {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        TimeReference::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A read-side slice request: two instants plus how the caller *spelled* them.
///
/// Unlike `resolution` / `horizon` / `interval`, a range is never stored and is
/// not part of a series' identity — it is passed per call and applied to the
/// grid. It does not require grid alignment: `start` is floored and `end`
/// ceil-ed onto the series' own grid.
///
/// The `zoneless` flag is what lets the core apply decision 8 — *bounds must
/// match the series' reference, no coercion*. A naive bound against a zoned
/// series, or an aware bound against a [`TimeReference::Zoneless`] one, is a
/// category error with no defined mapping to fall back on, so it is refused
/// rather than reinterpreted. An aware bound need not match the series' own
/// offset: `2024-01-01T00:00-07:00` and `2024-01-01T07:00Z` are the same
/// instant, and slicing is instant arithmetic.
///
/// Bounds stay unconstrained in *precision* — a sub-millisecond bound is legal
/// even though stored instants are not. Precision and zone are asymmetric on
/// purpose: a sub-millisecond bound names a real instant, an unzoned one does
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Whether the caller wrote these bounds without a zone. `false` for a
    /// native Rust caller, whose `DateTime<Utc>` is zoned by construction; the
    /// bindings set it from the spelling they were handed.
    pub zoneless: bool,
}

impl TimeRange {
    /// A zoned range — the native Rust spelling, since `DateTime<Utc>` names an
    /// instant on its own.
    pub fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        Self {
            start,
            end,
            zoneless: false,
        }
    }

    /// A range whose bounds were written as wall clocks, for a
    /// [`TimeReference::Zoneless`] series. The instants are the wall clocks read
    /// as if UTC, exactly as the series' own timestamps were.
    pub fn zoneless(start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        Self {
            start,
            end,
            zoneless: true,
        }
    }

    /// A range whose spelling a binding inferred from its caller.
    pub fn spelled(start: DateTime<Utc>, end: DateTime<Utc>, zoneless: bool) -> Self {
        Self {
            start,
            end,
            zoneless,
        }
    }

    /// The `(start, end)` instants, for arithmetic that does not care how they
    /// were spelled — which is all of the slice arithmetic.
    pub fn bounds(&self) -> (DateTime<Utc>, DateTime<Utc>) {
        (self.start, self.end)
    }

    /// Refuse a bound whose spelling the series cannot answer. `what` names the
    /// series in the message.
    pub fn check_against(&self, reference: Option<&TimeReference>, what: &str) -> Result<()> {
        check_bound_spelling(self.zoneless, reference, what)
    }
}

/// The bound-spelling rule, stated once.
///
/// It depends only on *how* a bound was spelled and never on which instant it
/// names, so every bound type shares this one copy rather than restating it:
/// [`TimeRange`] for the ranged reads, and [`crate::Instants`] for the vector a
/// projection is evaluated at. Two copies would be two chances to disagree
/// about a category error.
pub(crate) fn check_bound_spelling(
    zoneless: bool,
    reference: Option<&TimeReference>,
    what: &str,
) -> Result<()> {
    match (zoneless, TimeReference::accepts_zoned_bound(reference)) {
        (false, true) | (true, false) => Ok(()),
        (true, true) => Err(TimeSeriesError::InvalidParameter(format!(
            "the query bounds carry no zone, but {what} records instants \
             (time_reference {}); a wall clock does not name one, and the store \
             will not guess a zone for it",
            describe(reference)
        ))),
        (false, false) => Err(TimeSeriesError::InvalidParameter(format!(
            "the query bounds name an instant, but {what} is zoneless \
             (time_reference \"zoneless\"); its timestamps are wall clocks, so there is \
             no defined mapping from an instant onto them"
        ))),
    }
}

impl From<(DateTime<Utc>, DateTime<Utc>)> for TimeRange {
    fn from((start, end): (DateTime<Utc>, DateTime<Utc>)) -> Self {
        Self::new(start, end)
    }
}

/// How an optional reference reads in an error message.
fn describe(reference: Option<&TimeReference>) -> String {
    match reference {
        Some(r) => format!("{:?}", r.as_storage_string()),
        None => "unspecified".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_spellings_round_trip() {
        for reference in [
            TimeReference::Utc,
            TimeReference::Zoneless,
            TimeReference::FixedOffset(-420),
            TimeReference::FixedOffset(0),
            TimeReference::FixedOffset(330),
            TimeReference::Zone("America/Denver".into()),
            TimeReference::Zone("UTC".into()),
            TimeReference::Zone("Etc/GMT+5".into()),
            TimeReference::Zone("America/Argentina/Buenos_Aires".into()),
        ] {
            let s = reference.as_storage_string();
            assert_eq!(
                TimeReference::parse(&s).unwrap(),
                reference,
                "round trip via {s:?}"
            );
            let json = serde_json::to_string(&reference).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            assert_eq!(
                serde_json::from_str::<TimeReference>(&json).unwrap(),
                reference
            );
        }
    }

    #[test]
    fn fixed_offset_zero_is_not_utc() {
        // Both render the same wall clock forever; the distinction is the whole
        // point of recording a spelling.
        assert_eq!(TimeReference::FixedOffset(0).as_storage_string(), "+00:00");
        assert_ne!(
            TimeReference::parse("+00:00").unwrap(),
            TimeReference::Utc,
            "an explicit +00:00 is a fixed offset, not the UTC literal"
        );
    }

    #[test]
    fn the_iana_zone_utc_is_not_the_utc_literal() {
        assert_eq!(
            TimeReference::parse("UTC").unwrap(),
            TimeReference::Zone("UTC".into())
        );
        assert_eq!(TimeReference::parse("utc").unwrap(), TimeReference::Utc);
    }

    #[test]
    fn offsets_parse_in_the_spellings_a_human_types() {
        assert_eq!(parse_offset("-07:00"), Some(-420));
        assert_eq!(parse_offset("-0700"), Some(-420));
        assert_eq!(parse_offset("-07"), Some(-420));
        assert_eq!(parse_offset("+05:30"), Some(330));
        assert_eq!(parse_offset("07:00"), None, "an offset needs a sign");
        assert_eq!(parse_offset("+24:00"), None);
        assert_eq!(parse_offset("+07:60"), None);
    }

    #[test]
    fn a_non_ascii_offset_shaped_string_is_rejected_not_a_panic() {
        // `rest` is measured in bytes, so a multi-byte character can make a
        // string look like one of the fixed-width offset spellings while its
        // split points fall inside a character. Every one of these used to
        // panic on a char boundary; all of them reach the store as untrusted
        // text (a CLI flag, a descriptor, a wire string, a zone name).
        for s in ["+aéb", "-aéb", "+éé", "-é", "+aé:b", "+00:é", "-éé:00"] {
            assert_eq!(parse_offset(s), None, "{s:?} is not an offset");
            // The same strings reach `parse_offset` a second time through
            // `validate`, which uses it to rule out a zone name in disguise.
            let _ = TimeReference::parse(s).map(|r| r.validate());
        }
    }

    #[test]
    fn every_i32_offset_is_judged_without_overflowing() {
        // `i32::MIN` is the one value `abs()` cannot represent: it panics on it
        // in debug and wraps back to itself in release, where a signed
        // comparison then reads it as *within* a day of UTC and lets the least
        // plausible offset there is through validation. A native caller can
        // build this directly, so it has to be rejected like any other.
        for minutes in [i32::MIN, i32::MIN + 1, i32::MAX, -1440, 1440] {
            let err = TimeReference::FixedOffset(minutes)
                .validate()
                .expect_err("{minutes} is not within a day of UTC");
            assert!(
                err.to_string().contains("not a real UTC offset"),
                "{minutes}: {err}"
            );
        }

        // The boundary itself stays exclusive on both sides, unchanged.
        for minutes in [-1439, -420, 0, 330, 1439] {
            TimeReference::FixedOffset(minutes)
                .validate()
                .unwrap_or_else(|e| panic!("{minutes} should be a real offset: {e}"));
        }
    }

    #[test]
    fn a_zone_name_cannot_impersonate_another_spelling() {
        for name in ["utc", "zoneless", "-07:00", "+0530"] {
            let err = TimeReference::Zone(name.into()).validate().unwrap_err();
            assert!(
                matches!(err, TimeSeriesError::InvalidParameter(_)),
                "{name} should be refused as a zone name, got {err}"
            );
        }
    }

    #[test]
    fn zone_shape_is_checked_but_existence_is_not() {
        // Shape passes, existence is somebody else's job.
        TimeReference::Zone("America/Dever".into())
            .validate()
            .unwrap();
        for bad in [
            "",
            "/Denver",
            "America//Denver",
            "America/Den ver",
            "1America",
        ] {
            assert!(
                TimeReference::Zone(bad.into()).validate().is_err(),
                "{bad:?} should not be shaped like an IANA name"
            );
        }
        let long = "A".repeat(MAX_ZONE_NAME_LEN + 1);
        assert!(TimeReference::Zone(long).validate().is_err());
    }

    #[test]
    fn unspecified_groups_with_the_zoned_variants_for_bounds() {
        assert!(TimeReference::accepts_zoned_bound(None));
        assert!(TimeReference::accepts_zoned_bound(Some(
            &TimeReference::Utc
        )));
        assert!(TimeReference::accepts_zoned_bound(Some(
            &TimeReference::Zone("America/Denver".into())
        )));
        assert!(!TimeReference::accepts_zoned_bound(Some(
            &TimeReference::Zoneless
        )));
    }
}
