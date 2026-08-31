/// Semver version of the on-disk data format, covering the HDF5 layout, the
/// dtype and timestamp encodings, and the hash domain.
///
/// It does **not** cover the SQLite catalog's shape. That has its own contract,
/// [`CATALOG_SCHEMA_REVISION`](crate::metadata::migrate::CATALOG_SCHEMA_REVISION),
/// and moves independently: a catalog change the idempotent DDL cannot make to
/// an existing table needs a revision bump and a migration, not a bump here.
/// Bumping this constant for a catalog-only change strands every existing store
/// for no reason -- which is exactly what it used to do.
///
/// # Three-tier compatibility
///
/// The version is no longer checked by strict equality. [`compatibility`]
/// sorts a version stamp found on an HDF5 file into one of three tiers:
///
/// * [`Compat::Current`] — the stamp equals this constant. Opened as-is.
/// * [`Compat::Upgradable`] — the stamp is at least [`MIN_UPGRADABLE_VERSION`]
///   and older than this constant. The array layout is compatible, so the file
///   is read as-is; a *writable* open runs the catalog migration ladder and
///   then re-stamps the file to this version. A read-only open of such a store
///   reports [`crate::TimeSeriesError::CatalogMigrationRequired`] from the
///   catalog side, which is the actionable error: open it once for writing.
/// * [`Compat::Incompatible`] — anything older than
///   [`MIN_UPGRADABLE_VERSION`], anything newer than this constant, and
///   anything unparseable (including the `"unspecified"` a file with no stamp
///   reads as). Rejected with [`crate::TimeSeriesError::IncompatibleFormat`],
///   exactly as before.
///
/// # Which constant moves, and when
///
/// Two revisions are tracked, and they answer different questions:
///
/// * `DATA_FORMAT_VERSION` (here) describes the **artifact as a whole** and is
///   stamped on the HDF5 root. Bump it for any change to the array layout, the
///   dtype encoding, the timestamp encoding, or a hash domain — the things no
///   migration can fix — and raise [`MIN_UPGRADABLE_VERSION`] to the new value
///   at the same time, because such a change really does strand older stores.
///   Bump it *without* raising `MIN_UPGRADABLE_VERSION` for a change the
///   catalog migration ladder can apply in place.
/// * [`CATALOG_SCHEMA_REVISION`](crate::metadata::migrate::CATALOG_SCHEMA_REVISION)
///   describes the **SQLite catalog** alone. **Any catalog change the
///   idempotent DDL cannot make to an existing table — a new column, a changed
///   CHECK, a rebuilt table, a backfill — needs a `CATALOG_SCHEMA_REVISION`
///   bump plus an append-only entry in `MIGRATIONS`.** It no longer needs a
///   re-created store.
///
/// A purely additive new *table* or *index* still needs neither: the DDL is
/// idempotent, so an older store picks it up on its first writable open, and
/// old readers ignore it. The obligation that comes with that is unchanged —
/// every read of such a table must tolerate its absence, because a read-only
/// open cannot run DDL. See the DDL comment in `metadata/schema.rs`.
///
/// # History
///
/// The bumps below all predate the migration ladder and are genuinely
/// unmigratable: each one changed the meaning of bytes already on disk, and a
/// store written before it is still rejected on open.
///
/// 0.12.0 changed `owner_category` and `time_series_type` in
/// `time_series_associations` from TEXT names to small INTEGER codes
/// (`OwnerCategory::code` / `TimeSeriesType::code`). Stores written by 0.11.0
/// and earlier hold names in those columns and are rejected on open.
///
/// 0.13.0 replaced the `dtype` column with `element_type`, which spells the
/// *logical* element type (`ElementType`) and derives the physical dtype from
/// it. Stores written by 0.12.0 have no such column and are rejected on open.
///
/// 0.14.0 moved a `NonSequentialTimeSeries`'s timestamps out of the association
/// row. The `timestamps_json` TEXT column became a `timestamps_hash` BLOB
/// resolving into the new content-addressed `timestamp_sets` table, whose blobs
/// hold the compact encoding in `crate::timestamps`; and the arrays of
/// irregular series are now column-packed into `nsts_…` datasets keyed by that
/// same hash, instead of one standalone `arr_…` dataset each. Both halves of a
/// 0.13.0 store are rejected on open.
///
/// 0.15.0 renamed the `ext` column to `application_data` and added the
/// `quantity_kind` and `unit_system` columns, all on
/// `time_series_associations`. Stores written by 0.14.0 and earlier are
/// rejected on open.
///
/// 0.16.0 added the `component_field` column to `time_series_associations`,
/// naming the field on the owning component whose value the series varies over
/// time. Stores written by 0.15.0 and earlier are rejected on open.
///
/// 0.17.0 added the `time_reference` column to `time_series_associations`,
/// recording how a series' timestamps were *spelled* — an instant in UTC, an
/// instant at a fixed offset, an instant in a named IANA zone, or a wall clock
/// naming no instant. It also changes how stored timestamps are *interpreted*:
/// a row marked `zoneless` holds wall clocks the store keeps as if UTC, which
/// an older reader would hand back as instants. Stores written by 0.16.0 and
/// earlier are rejected on open.
///
/// 0.18.0 declared every catalog table's `id` as `INTEGER PRIMARY KEY
/// AUTOINCREMENT`, so a deleted association's id is never reissued and a
/// consumer's stored reference keeps meaning the row it named. A 0.17.0
/// catalog has no `sqlite_sequence` high-water mark and is rejected on open.
///
/// 0.19.0 moved a `NonSequentialTimeSeries`'s timestamps out of the catalog's
/// `timestamp_sets` blobs and into the array file, one `i64` dataset of unix
/// milliseconds per distinct vector at `time_series/timestamps/tsv_{hex_hash}`.
/// Both the layout and the hash domain move — `timestamps_hash` now hashes the
/// milliseconds rather than the delta-varint blob — so stores written by
/// 0.18.0 and earlier are rejected on open.
///
/// Catalog revision 2 is the first catalog change to take no bump at all, and
/// the one the ladder was built for. It widens the `time_series_type` CHECK
/// from `BETWEEN 0 AND 5` to `>= 0`, moving that column's domain off SQLite and
/// onto `TimeSeriesType::from_code`, which already gates every read and every
/// write. Nothing in the HDF5 file changes, so there is nothing to bump: the
/// table rebuild is entirely on the catalog side, and a 0.19.0 store upgrades
/// in place on its first writable open.
pub const DATA_FORMAT_VERSION: &str = "0.19.0";

/// The oldest [`DATA_FORMAT_VERSION`] this build can open and upgrade in place.
///
/// A store stamped between this and [`DATA_FORMAT_VERSION`] is
/// [`Compat::Upgradable`]: its arrays are readable as they stand, and its
/// catalog is brought forward by the migration ladder on the first writable
/// open. Anything older is [`Compat::Incompatible`] and keeps the pre-ladder
/// behavior — the format changes that produced those stamps rewrote bytes the
/// current reader cannot interpret, so there is nothing to migrate.
///
/// Raise this to the new [`DATA_FORMAT_VERSION`] whenever a bump really does
/// strand older stores; leave it alone for a bump the ladder can absorb.
pub const MIN_UPGRADABLE_VERSION: &str = "0.19.0";

/// How a [`DATA_FORMAT_VERSION`] stamp found on a store relates to this build.
/// See the [`DATA_FORMAT_VERSION`] docs for what each tier means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compat {
    /// The stamp equals [`DATA_FORMAT_VERSION`].
    Current,
    /// Older than [`DATA_FORMAT_VERSION`] but at least
    /// [`MIN_UPGRADABLE_VERSION`]: readable now, upgraded on a writable open.
    Upgradable,
    /// Too old, too new, or unparseable. Rejected.
    Incompatible,
}

/// Split a `MAJOR.MINOR.PATCH` string into its three numeric components.
/// `None` for anything that is not exactly that shape — including the
/// `"unspecified"` an unstamped file reads as, which is therefore
/// [`Compat::Incompatible`].
fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Which compatibility tier the on-disk stamp `found` falls into.
pub fn compatibility(found: &str) -> Compat {
    compatibility_within(found, MIN_UPGRADABLE_VERSION, DATA_FORMAT_VERSION)
}

/// [`compatibility`] against explicit bounds rather than the live constants.
///
/// The rule itself, extracted so it can be tested at boundaries the constants
/// cannot currently express. `MIN_UPGRADABLE_VERSION` equals
/// `DATA_FORMAT_VERSION` today, so no stamp on earth classifies as
/// [`Compat::Upgradable`] -- and a rule with an empty middle tier is a rule
/// whose middle tier has never run. The window becomes non-empty at the first
/// bump the ladder absorbs; until then this is where that tier is exercised.
///
/// Not public: callers get the bounds this build actually ships.
pub(crate) fn compatibility_within(found: &str, min: &str, current: &str) -> Compat {
    if found == current {
        return Compat::Current;
    }
    let Some(found) = parse_semver(found) else {
        return Compat::Incompatible;
    };
    let current = parse_semver(current).expect("a MAJOR.MINOR.PATCH literal");
    let floor = parse_semver(min).expect("a MAJOR.MINOR.PATCH literal");
    // A stamp newer than this build is as unreadable as one that is too old:
    // this build has no idea what the newer format changed.
    if found >= floor && found < current {
        Compat::Upgradable
    } else {
        Compat::Incompatible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_its_own_tier() {
        assert_eq!(compatibility(DATA_FORMAT_VERSION), Compat::Current);
    }

    #[test]
    fn the_upgrade_floor_is_upgradable_unless_it_is_current() {
        // Both constants are literals here, so this stays honest as they move:
        // when they are equal there is no upgradable window at all.
        let expected = if MIN_UPGRADABLE_VERSION == DATA_FORMAT_VERSION {
            Compat::Current
        } else {
            Compat::Upgradable
        };
        assert_eq!(compatibility(MIN_UPGRADABLE_VERSION), expected);
    }

    #[test]
    fn older_than_the_floor_and_newer_than_current_are_both_incompatible() {
        assert_eq!(compatibility("0.18.0"), Compat::Incompatible);
        assert_eq!(compatibility("0.1.0"), Compat::Incompatible);
        assert_eq!(compatibility("99.0.0"), Compat::Incompatible);
    }

    #[test]
    fn unparseable_stamps_are_incompatible() {
        assert_eq!(compatibility("unspecified"), Compat::Incompatible);
        assert_eq!(compatibility(""), Compat::Incompatible);
        assert_eq!(compatibility("0.19"), Compat::Incompatible);
        assert_eq!(compatibility("0.19.0.1"), Compat::Incompatible);
        assert_eq!(compatibility("0.19.0-rc1"), Compat::Incompatible);
    }

    /// The middle tier, which the live constants cannot currently produce.
    ///
    /// `MIN_UPGRADABLE_VERSION == DATA_FORMAT_VERSION`, so `compatibility` has
    /// never once returned `Upgradable` in production or in any other test
    /// here. The rule is exercised against an explicit window instead, so the
    /// day the two constants separate they separate onto tested behavior.
    #[test]
    fn the_upgradable_tier_is_the_half_open_window_between_the_bounds() {
        let tier = |v| compatibility_within(v, "1.2.0", "1.5.0");

        // Closed at the floor, open at the top: the floor itself upgrades, and
        // the current version is its own tier rather than the window's end.
        assert_eq!(tier("1.2.0"), Compat::Upgradable);
        assert_eq!(tier("1.3.7"), Compat::Upgradable);
        assert_eq!(tier("1.4.99"), Compat::Upgradable);
        assert_eq!(tier("1.5.0"), Compat::Current);

        // One patch below the floor is out, and so is anything past current.
        assert_eq!(tier("1.1.9"), Compat::Incompatible);
        assert_eq!(tier("0.9.0"), Compat::Incompatible);
        assert_eq!(tier("1.5.1"), Compat::Incompatible);
        assert_eq!(tier("2.0.0"), Compat::Incompatible);

        // Unparseable is refused whatever the window.
        assert_eq!(tier("unspecified"), Compat::Incompatible);
    }

    /// An empty window admits nothing to the middle tier -- the state today,
    /// pinned so that "no test covers Upgradable" stays a deliberate fact
    /// rather than an oversight someone has to rediscover.
    #[test]
    fn an_empty_window_has_no_upgradable_tier() {
        assert_eq!(
            compatibility_within("1.5.0", "1.5.0", "1.5.0"),
            Compat::Current
        );
        assert_eq!(
            compatibility_within("1.4.0", "1.5.0", "1.5.0"),
            Compat::Incompatible
        );
    }

    #[test]
    fn minor_and_patch_compare_numerically_not_lexically() {
        // "0.9.0" sorts after "0.18.0" as text; as versions it is far older.
        assert_eq!(compatibility("0.9.0"), Compat::Incompatible);
    }
}
