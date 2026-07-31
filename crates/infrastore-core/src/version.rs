/// Semver version of the on-disk data format. Bumped when the HDF5 layout,
/// SQLite schema, or hash domain changes in a backward-incompatible way.
///
/// Purely additive SQLite tables do not bump it. The version is checked by
/// strict equality on open, so a bump bricks every existing store; a new table
/// created by the idempotent DDL costs old readers nothing (they ignore it) and
/// is picked up by old stores on their first writable open. The `associations`
/// table landed this way — see the DDL comment in `metadata/schema.rs` for the
/// obligation that comes with it: every read of such a table must tolerate its
/// absence, because a read-only open cannot run DDL.
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
pub const DATA_FORMAT_VERSION: &str = "0.14.0";
