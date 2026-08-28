/// Semver version of the on-disk data format. Checked by strict equality on
/// open, so a bump rejects every store written by an earlier version.
///
/// Bump it for any incompatible change to the HDF5 layout, the SQLite schema,
/// or a hash domain, and record what changed in
/// `docs/src/reference/file-format.md`. A purely additive *table* does not need
/// a bump — the idempotent DDL creates it and older readers ignore it — but a
/// new *column* does, because `CREATE TABLE IF NOT EXISTS` leaves an existing
/// table's columns alone and every statement naming the new one would then
/// fail.
pub const DATA_FORMAT_VERSION: &str = "0.19.0";
