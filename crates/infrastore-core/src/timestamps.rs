//! Canonical compact encoding of an explicit timestamp vector.
//!
//! `NonSequentialTimeSeries` carries its timestamps explicitly, and the catalog
//! stores them once per *distinct vector* (see the `timestamp_sets` table in
//! [`crate::metadata::schema`]). This module is the encoding those rows hold,
//! and [`crate::hash::timestamps_hash`] content-addresses them by hashing
//! exactly these bytes — so the encoding is part of the on-disk contract and any
//! change to it must bump [`crate::DATA_FORMAT_VERSION`].
//!
//! # Layout
//!
//! ```text
//! [0]      version   u8   (currently 1)
//! [1]      unit      u8   the delta unit, an index into `UNITS`
//! then     count     uvarint
//! if count > 0:
//!   base_secs        svarint  (zigzag) seconds since the Unix epoch
//!   base_nanos       uvarint  sub-second nanoseconds of the first timestamp
//!   delta_1..delta_n svarint  (zigzag) successive differences, in `unit`s
//! ```
//!
//! Deltas rather than absolute values, in the coarsest unit that divides all of
//! them: what makes a "non-sequential" series cheap is that its irregularity is
//! usually coarse — event times on whole seconds or minutes, a regular grid with
//! gaps. An hourly vector then has every delta equal to 1 (one byte each), and a
//! year of it costs ~8.8 KB against ~210 KB for the RFC3339 JSON this replaced.
//! A vector with genuine microsecond jitter degrades to ~4-5 bytes per
//! timestamp, still well under the JSON form.
//!
//! # Precision
//!
//! The *encoding* carries nanosecond resolution over chrono's full range: the
//! base timestamp is stored as its own `(seconds, nanoseconds)` pair and every
//! later timestamp is reconstructed by accumulating nanosecond deltas in `i128`,
//! so neither the epoch offset nor the delta magnitude can overflow. That is
//! deliberately wider than what the store *accepts*
//! ([`require_millisecond_precision`]), so a store written before that rule
//! still decodes exactly.
//!
//! The one thing not preserved is a *leap second* (chrono spells one as a
//! sub-second component at or above one second) at any position after the first:
//! accumulating linear nanoseconds normalizes it into the following second. Leap
//! seconds in the first position round-trip exactly, being stored verbatim.

use chrono::{DateTime, Utc};

use crate::error::{Result, TimeSeriesError};

/// The store's timestamp precision contract: a stored instant must be a whole
/// number of milliseconds.
///
/// This is the same floor a [`Period`](crate::Period) has always had (see the
/// `period.rs` module docs — `is_positive` counts whole milliseconds, and the
/// ISO-8601 encoding emits at most three fractional digits). Applying it to the
/// *instants* as well as the *spans* is what makes a timestamp mean the same
/// thing in every consumer: the C ABI and the Julia binding exchange instants as
/// `i64` unix milliseconds, and Python's `datetime` is microsecond. A finer
/// instant cannot survive those boundaries, and truncating it there is silent —
/// it moves a series onto a different instant in one binding but not another,
/// and for a `NonSequentialTimeSeries` whose timestamps are less than a
/// millisecond apart it collapses two into one, which then fails the
/// strictly-increasing rule on the way back out. Rejecting the write is the only
/// one of those outcomes a caller can act on.
///
/// Callers needing a finer grid should scale the unit, exactly as they must for
/// a sub-millisecond resolution: a 500 µs series is a 500-unit series that
/// records the unit in `units`.
///
/// Enforced on the **write path only** ([`crate::Store::add`] and every path
/// that funnels through it). Reads stay permissive, so an artifact written
/// before this rule keeps reading back exactly as it was written — which is why
/// the rule does not bump [`crate::DATA_FORMAT_VERSION`]: the on-disk format is
/// unchanged, only what a new write accepts.
///
/// A leap second is spelled by chrono as a sub-second component at or above one
/// second; the modulo below reads its *fractional* part, so a leap second on a
/// whole millisecond is accepted like any other instant.
///
/// `label` is a closure rather than a `&str` because one caller runs this per
/// *timestamp* of a `NonSequentialTimeSeries`: building the label eagerly would
/// allocate a `String` for every entry of a vector that may hold millions, all
/// of them thrown away on the overwhelmingly common all-pass path.
pub(crate) fn require_millisecond_precision(
    t: DateTime<Utc>,
    label: impl FnOnce() -> String,
) -> std::result::Result<(), String> {
    if !t.timestamp_subsec_nanos().is_multiple_of(1_000_000) {
        let label = label();
        return Err(format!(
            "{label} {t} is finer than a millisecond; the store records instants to the \
             millisecond, as it does periods. Truncate to a whole millisecond, or scale the \
             unit and record it in `units`."
        ));
    }
    Ok(())
}

/// Encoding version, the first byte of every blob.
const VERSION: u8 = 1;

/// Delta units in nanoseconds, indexed by the unit byte. Ordered ascending; the
/// encoder picks the last one that divides every delta.
const UNITS: [i128; 7] = [
    1,                  // nanosecond
    1_000,              // microsecond
    1_000_000,          // millisecond
    1_000_000_000,      // second
    60_000_000_000,     // minute
    3_600_000_000_000,  // hour
    86_400_000_000_000, // day
];

const NANOS_PER_SEC: i128 = 1_000_000_000;

/// Encode `timestamps` into the canonical blob. Infallible: any sequence
/// encodes, including an empty one, a single timestamp, or a non-monotonic
/// vector (deltas are signed).
pub(crate) fn encode(timestamps: &[DateTime<Utc>]) -> Vec<u8> {
    let linear: Vec<i128> = timestamps.iter().map(linear_nanos).collect();
    let deltas: Vec<i128> = linear.windows(2).map(|w| w[1] - w[0]).collect();

    // The coarsest unit dividing every delta. `gcd` of an empty set is 0, which
    // every unit divides, so a vector of fewer than two timestamps takes the
    // largest unit and encodes no deltas anyway.
    let step = deltas.iter().fold(0i128, |acc, &d| gcd(acc, d.abs()));
    let unit_index = UNITS
        .iter()
        .rposition(|&u| step % u == 0)
        .expect("UNITS[0] is 1, which divides every value");
    let unit = UNITS[unit_index];

    let mut out = Vec::with_capacity(2 + 12 + deltas.len());
    out.push(VERSION);
    out.push(unit_index as u8);
    put_uvarint(&mut out, timestamps.len() as u128);
    if let Some(first) = timestamps.first() {
        put_svarint(&mut out, first.timestamp() as i128);
        put_uvarint(&mut out, u128::from(first.timestamp_subsec_nanos()));
        for delta in &deltas {
            put_svarint(&mut out, delta / unit);
        }
    }
    out
}

/// Decode a blob written by [`encode`]. Every failure is an
/// [`TimeSeriesError::IntegrityError`]: the bytes came out of the store's own
/// catalog, so anything malformed means the catalog is damaged.
pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<DateTime<Utc>>> {
    let mut pos = 0usize;
    let version = *bytes
        .first()
        .ok_or_else(|| bad("timestamp blob is empty".to_string()))?;
    if version != VERSION {
        return Err(bad(format!(
            "unsupported timestamp encoding version {version} (expected {VERSION})"
        )));
    }
    let unit_index = *bytes
        .get(1)
        .ok_or_else(|| bad("timestamp blob is truncated at the unit byte".to_string()))?;
    let unit = *UNITS
        .get(usize::from(unit_index))
        .ok_or_else(|| bad(format!("unknown timestamp unit {unit_index}")))?;
    pos += 2;

    let count = get_uvarint(bytes, &mut pos)? as usize;
    if count == 0 {
        expect_consumed(bytes, pos)?;
        return Ok(Vec::new());
    }

    let base_secs = get_svarint(bytes, &mut pos)?;
    let base_nanos = get_uvarint(bytes, &mut pos)?;
    let base_nanos = u32::try_from(base_nanos).map_err(|_| {
        bad(format!(
            "timestamp sub-second component {base_nanos} is out of range"
        ))
    })?;
    let first = DateTime::from_timestamp(
        i64::try_from(base_secs).map_err(|_| bad("base timestamp is out of range".to_string()))?,
        base_nanos,
    )
    .ok_or_else(|| bad("base timestamp is out of range".to_string()))?;

    // Capacity bounded by what the blob could possibly hold, not by the count it
    // claims. `count` is a varint out of the data, so a corrupt or hostile row
    // could name 2^42 timestamps and this reserved for all of them before the
    // decode loop discovered the blob was eleven bytes long — a measured 48 TiB
    // reservation that macOS granted lazily, cost 132 ms, and threw away one
    // instruction later; past `isize::MAX` it panicked outright instead of
    // returning the `IntegrityError` this module promises for a malformed blob.
    // Every timestamp after the first costs at least one byte, so the remaining
    // length is a hard ceiling on how many can still arrive.
    let mut out = Vec::with_capacity(count.min(bytes.len().saturating_sub(pos).saturating_add(1)));
    out.push(first);
    // The accumulator starts from the base's *linear* nanoseconds, so a
    // leap-second base contributes its extra nanoseconds to every later value
    // exactly as the encoder measured them.
    let mut acc = linear_nanos(&first);
    for _ in 1..count {
        acc += get_svarint(bytes, &mut pos)? * unit;
        let secs = i64::try_from(acc.div_euclid(NANOS_PER_SEC))
            .map_err(|_| bad("decoded timestamp is out of range".to_string()))?;
        let nanos = acc.rem_euclid(NANOS_PER_SEC) as u32;
        out.push(
            DateTime::from_timestamp(secs, nanos)
                .ok_or_else(|| bad("decoded timestamp is out of range".to_string()))?,
        );
    }
    expect_consumed(bytes, pos)?;
    Ok(out)
}

/// A timestamp as plain nanoseconds since the epoch. `i128` because chrono's
/// range (±262,000 years) overflows `i64` nanoseconds by four orders of
/// magnitude.
fn linear_nanos(t: &DateTime<Utc>) -> i128 {
    i128::from(t.timestamp()) * NANOS_PER_SEC + i128::from(t.timestamp_subsec_nanos())
}

fn bad(message: String) -> TimeSeriesError {
    TimeSeriesError::IntegrityError(message)
}

/// Reject trailing bytes: a blob longer than its own contents is a corrupt row,
/// not a forward-compatible extension (the version byte is what carries those).
fn expect_consumed(bytes: &[u8], pos: usize) -> Result<()> {
    if pos != bytes.len() {
        return Err(bad(format!(
            "timestamp blob has {} trailing byte(s)",
            bytes.len() - pos
        )));
    }
    Ok(())
}

fn gcd(a: i128, b: i128) -> i128 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

// ---- LEB128 ---------------------------------------------------------------

fn put_uvarint(out: &mut Vec<u8>, mut value: u128) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn get_uvarint(bytes: &[u8], pos: &mut usize) -> Result<u128> {
    let mut value: u128 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *bytes
            .get(*pos)
            .ok_or_else(|| bad("timestamp blob ends mid-varint".to_string()))?;
        *pos += 1;
        // 128 bits is 18 full groups of 7 plus a 2-bit remainder; anything wider
        // cannot be a value this module wrote.
        if shift >= 128 {
            return Err(bad("timestamp blob holds an oversized varint".to_string()));
        }
        value |= u128::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

/// Zigzag so small negative deltas stay one byte, as small positive ones do.
fn put_svarint(out: &mut Vec<u8>, value: i128) {
    put_uvarint(
        out,
        (value.wrapping_shl(1) as u128) ^ ((value >> 127) as u128),
    );
}

fn get_svarint(bytes: &[u8], pos: &mut usize) -> Result<i128> {
    let raw = get_uvarint(bytes, pos)?;
    Ok(((raw >> 1) as i128) ^ -((raw & 1) as i128))
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
    }

    fn round_trip(timestamps: &[DateTime<Utc>]) -> Vec<u8> {
        let bytes = encode(timestamps);
        assert_eq!(decode(&bytes).unwrap(), timestamps, "round trip");
        bytes
    }

    #[test]
    fn empty_and_singleton_vectors_round_trip() {
        assert!(round_trip(&[]).len() <= 3);
        round_trip(&[t0()]);
    }

    #[test]
    fn a_regular_grid_costs_one_byte_per_timestamp() {
        // The point of the adaptive unit: hourly deltas become 1 unit each, so
        // the whole vector is a run of single-byte varints. The RFC3339 JSON
        // this replaced was 24 bytes per timestamp.
        let hourly: Vec<DateTime<Utc>> = (0..8760).map(|k| t0() + Duration::hours(k)).collect();
        let bytes = round_trip(&hourly);
        assert!(
            bytes.len() < 9_000,
            "8760 hourly timestamps encoded to {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn irregular_and_sub_second_vectors_round_trip() {
        // Mixed gaps, none of them a multiple of the others.
        let gaps = [900, 1800, 3600, 7200, 86_400];
        let mut irregular = vec![t0()];
        for (i, _) in (0..500).enumerate() {
            let last = *irregular.last().unwrap();
            irregular.push(last + Duration::seconds(gaps[i % gaps.len()]));
        }
        round_trip(&irregular);

        // Nanosecond components force the finest unit and must still survive.
        let jittery: Vec<DateTime<Utc>> = (0..100)
            .map(|k| t0() + Duration::seconds(k * 7) + Duration::nanoseconds(k * 13 + 1))
            .collect();
        round_trip(&jittery);
    }

    #[test]
    fn pre_epoch_and_non_monotonic_vectors_round_trip() {
        // Negative epoch offsets, and a decreasing sequence: the store validates
        // monotonicity, but the encoding must not be the thing that enforces it.
        let pre_epoch: Vec<DateTime<Utc>> = (0..10)
            .map(|k| Utc.with_ymd_and_hms(1900, 6, 1, 0, 0, 0).unwrap() + Duration::hours(k))
            .collect();
        round_trip(&pre_epoch);
        let mut descending = pre_epoch.clone();
        descending.reverse();
        round_trip(&descending);
    }

    #[test]
    fn the_extremes_of_the_supported_range_round_trip() {
        // Well outside i64 nanoseconds (chrono's own limit is ~year 262143),
        // which is why the accumulator is i128.
        let far = vec![
            Utc.with_ymd_and_hms(-200_000, 1, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(200_000, 1, 1, 0, 0, 0).unwrap(),
        ];
        round_trip(&far);
    }

    #[test]
    fn equal_vectors_encode_identically_and_different_ones_do_not() {
        // The content-addressing contract: the blob is the hash domain, so
        // encoding must be a function of the values alone.
        let a: Vec<DateTime<Utc>> = (0..5).map(|k| t0() + Duration::hours(k)).collect();
        let b: Vec<DateTime<Utc>> = (0..5).map(|k| t0() + Duration::hours(k)).collect();
        assert_eq!(encode(&a), encode(&b));
        let mut c = a.clone();
        c[3] += Duration::nanoseconds(1);
        assert_ne!(encode(&a), encode(&c));
    }

    #[test]
    fn malformed_blobs_are_integrity_errors() {
        let good = encode(&[t0(), t0() + Duration::hours(1)]);
        for bad_bytes in [
            vec![],                             // empty
            vec![99, 0, 1],                     // unknown version
            vec![VERSION, 200, 1],              // unknown unit
            good[..good.len() - 1].to_vec(),    // truncated
            [good.clone(), vec![0u8]].concat(), // trailing garbage
            vec![VERSION, 0, 0x80],             // varint runs off the end
        ] {
            assert!(
                matches!(decode(&bad_bytes), Err(TimeSeriesError::IntegrityError(_))),
                "expected an integrity error for {bad_bytes:?}"
            );
        }
    }
}
