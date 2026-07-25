/// Semver version of the on-disk data format. Bumped when the NetCDF layout,
/// SQLite schema, or hash domain changes in a backward-incompatible way.
///
/// Purely additive SQLite tables do not bump it. The version is checked by
/// strict equality on open, so a bump bricks every existing store; a new table
/// created by the idempotent DDL costs old readers nothing (they ignore it) and
/// is picked up by old stores on their first writable open. The `associations`
/// table landed this way — see the DDL comment in `metadata/schema.rs` for the
/// obligation that comes with it: every read of such a table must tolerate its
/// absence, because a read-only open cannot run DDL.
pub const DATA_FORMAT_VERSION: &str = "0.11.0";
