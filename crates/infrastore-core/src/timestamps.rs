//! The store's timestamp precision contract, and the conversion between
//! chrono instants and the milliseconds the store records.
//!
//! `NonSequentialTimeSeries` carries its timestamps explicitly, and the store
//! keeps them once per *distinct vector* in the HDF5 file, as an `i64` dataset
//! of Unix milliseconds (see [`crate::storage::hdf5`]). They are values, not
//! bookkeeping — as much the series' data as its array is — so they live beside
//! the arrays, in the half of the artifact that is built for bulk numeric data:
//! chunked, filtered, and readable by any HDF5 tool without this crate.
//!
//! They used to live in the SQLite catalog instead, as a hand-rolled
//! delta-varint blob interned in a `timestamp_sets` table. That encoding existed
//! to keep a *catalog* small, on the assumption that a store held one short time
//! axis; neither half of that holds up. A store can hold many axes, each of them
//! long, and the JSON document round trip that arrived since needs the
//! timestamps to travel with the artifact rather than be locked inside the
//! catalog. Plain `i64` milliseconds compressed by the store's own filter policy
//! are smaller than the varint blob was on a regular grid and simpler
//! everywhere.
//!
//! Milliseconds are the store's precision floor
//! ([`require_millisecond_precision`], which refuses a finer instant and a leap
//! second alike), so the encoding is exact for anything a write accepts, and
//! [`crate::hash::timestamps_hash`] content-addresses a vector by hashing
//! exactly these values.

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
/// that funnels through it), which is what lets the stored form be milliseconds
/// outright: nothing finer can reach it.
///
/// A **leap second** is refused for the same reason and by the same rule, though
/// it takes its own check. Chrono spells one as a sub-second component at or
/// above one second (`23:59:60` is `23:59:59` plus 1,000,000,000 ns), which is a
/// whole number of milliseconds and so passes the modulo — but it is not a unix
/// millisecond count at all: [`to_millis`] folds it onto the *following* second,
/// which every consumer of a unix timestamp does. Folding is what makes it
/// unacceptable here rather than merely lossy. A leap second and the second
/// after it are distinct `DateTime`s that would land on one stored instant, so
/// they share a [`timestamps_hash`](crate::hash::timestamps_hash) — two
/// genuinely different time axes interned as one, each reading back as whichever
/// was stored first. Within one vector it is worse: `[…, 23:59:60, 00:00:00]` is
/// strictly increasing going in and holds a duplicate coming back out.
pub(crate) fn require_millisecond_precision(
    t: DateTime<Utc>,
    label: impl FnOnce() -> String,
) -> std::result::Result<(), String> {
    // Read before the modulo: a leap second's sub-second component is a whole
    // number of milliseconds, so the modulo would pass it.
    if t.timestamp_subsec_nanos() >= 1_000_000_000 {
        let label = label();
        return Err(format!(
            "{label} {t} is a leap second; the store records instants as unix milliseconds, \
             which cannot express one — it would be stored as the following second, and two \
             series a leap second apart would share one stored timestamp. Use the second \
             before or after it."
        ));
    }
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

/// The stored form of a timestamp vector: unix milliseconds, one `i64` each.
///
/// Exact for every instant a write accepts (see
/// [`require_millisecond_precision`], which is what keeps the one instant this
/// *cannot* represent — a leap second, folded here onto the following second —
/// off the write path) and infallible for the rest: chrono's own range is
/// ±262,000 years, four orders of magnitude inside what `i64` milliseconds can
/// count.
pub(crate) fn to_millis(timestamps: &[DateTime<Utc>]) -> Vec<i64> {
    timestamps.iter().map(|t| t.timestamp_millis()).collect()
}

/// The index of the breakpoint **in force at** `at` in a strictly increasing
/// `breakpoints` vector: the greatest breakpoint `<= at`.
///
/// `None` when `at` is strictly before the first breakpoint — where a step
/// function is undefined — and for an empty vector. The caller turns that into
/// an error naming the series, which is why this returns `Option` rather than
/// a message it has no context for.
///
/// The single definition of the `PersistentTimeSeries` lookup. It lives here
/// rather than on the type because the columnar reader resolves the same
/// question against an *interned* breakpoint vector with no series struct in
/// hand — see [`crate::reader::StaticReader`]; two implementations of a
/// hold-last rule would be two chances to get the boundary wrong.
///
/// The boundary is `<=`, not `<`: a read exactly at a breakpoint gets that
/// breakpoint's own value, which is what "right-continuous" means and what
/// makes a persistent read agree with a `NonSequentialTimeSeries` read at every
/// instant both types define.
pub(crate) fn index_in_force_at(breakpoints: &[DateTime<Utc>], at: DateTime<Utc>) -> Option<usize> {
    breakpoints.partition_point(|b| *b <= at).checked_sub(1)
}

/// Rebuild a timestamp vector from its stored milliseconds.
///
/// A value chrono cannot represent is an [`TimeSeriesError::IntegrityError`]:
/// the numbers came out of the store's own file, so one outside the range every
/// write path can produce means that file is damaged.
pub(crate) fn from_millis(millis: &[i64]) -> Result<Vec<DateTime<Utc>>> {
    millis
        .iter()
        .map(|&ms| {
            DateTime::from_timestamp_millis(ms).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!(
                    "stored timestamp {ms} ms is outside the representable range"
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
    }

    fn round_trip(timestamps: &[DateTime<Utc>]) {
        assert_eq!(from_millis(&to_millis(timestamps)).unwrap(), timestamps);
    }

    #[test]
    fn vectors_round_trip_through_milliseconds() {
        round_trip(&[]);
        round_trip(&[t0()]);
        // A regular grid, an irregular one, and a decreasing one: the store
        // validates monotonicity, but the encoding must not be the thing that
        // enforces it.
        round_trip(
            &(0..8760)
                .map(|k| t0() + Duration::hours(k))
                .collect::<Vec<_>>(),
        );
        let gaps = [900, 1800, 3600, 7200, 86_400];
        let mut irregular = vec![t0()];
        for i in 0..500 {
            let last = *irregular.last().unwrap();
            irregular.push(last + Duration::seconds(gaps[i % gaps.len()]));
        }
        round_trip(&irregular);
        irregular.reverse();
        round_trip(&irregular);
        // Negative epoch offsets, and the far ends of chrono's own range, which
        // `i64` milliseconds hold with four orders of magnitude to spare.
        round_trip(
            &(0..10)
                .map(|k| Utc.with_ymd_and_hms(1900, 6, 1, 0, 0, 0).unwrap() + Duration::hours(k))
                .collect::<Vec<_>>(),
        );
        round_trip(&[
            Utc.with_ymd_and_hms(-200_000, 1, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(200_000, 1, 1, 0, 0, 0).unwrap(),
        ]);
    }

    #[test]
    fn sub_millisecond_instants_are_refused_on_the_write_path() {
        let label = || "timestamp".to_string();
        assert!(require_millisecond_precision(t0(), label).is_ok());
        assert!(require_millisecond_precision(t0() + Duration::milliseconds(7), label).is_ok());
        assert!(require_millisecond_precision(t0() + Duration::microseconds(1), label).is_err());
        assert!(require_millisecond_precision(t0() + Duration::nanoseconds(1), label).is_err());
    }

    /// A leap second is a whole number of milliseconds and would sail through
    /// the modulo, but it is not a unix millisecond count: it folds onto the
    /// following second, which the two assertions below show colliding.
    #[test]
    fn a_leap_second_is_refused_because_it_folds_onto_the_next_instant() {
        let leap = chrono::NaiveDate::from_ymd_opt(2016, 12, 31)
            .unwrap()
            .and_hms_milli_opt(23, 59, 59, 1_000)
            .unwrap()
            .and_utc();
        let next = Utc.with_ymd_and_hms(2017, 1, 1, 0, 0, 0).unwrap();
        assert_ne!(leap, next, "distinct instants going in");
        assert_eq!(
            leap.timestamp_millis(),
            next.timestamp_millis(),
            "one stored instant coming out"
        );
        assert!(leap.timestamp_subsec_nanos().is_multiple_of(1_000_000));

        let err = require_millisecond_precision(leap, || "timestamp".to_string())
            .expect_err("a leap second must not reach the store");
        assert!(err.contains("leap second"), "{err}");
        assert!(require_millisecond_precision(next, || "timestamp".to_string()).is_ok());
    }

    #[test]
    fn a_stored_value_outside_chronos_range_is_an_integrity_error() {
        assert!(matches!(
            from_millis(&[i64::MAX]),
            Err(TimeSeriesError::IntegrityError(_))
        ));
    }
}
