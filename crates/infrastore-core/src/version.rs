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
/// 0.15.0 renamed the `ext` column to `application_data` and added the
/// `quantity_kind` and `unit_system` columns, all on
/// `time_series_associations`. New *columns* on an existing table are not the
/// additive case described above: the DDL is `CREATE TABLE IF NOT EXISTS`, so a
/// 0.14.0 store re-opened for writing keeps its old column set and every
/// statement naming the new columns fails. Stores written by 0.14.0 and earlier
/// are rejected on open.
///
/// 0.16.0 added the `component_field` column to `time_series_associations`,
/// naming the field on the owning component whose value the series varies over
/// time. Same reasoning as 0.15.0: a new *column* on an existing table is not
/// the additive case, because `CREATE TABLE IF NOT EXISTS` leaves a 0.15.0
/// store's column set alone and every statement naming the new column would
/// then fail. Stores written by 0.15.0 and earlier are rejected on open.
///
/// 0.17.0 added the `time_reference` column to `time_series_associations`,
/// recording how a series' timestamps were *spelled* — an instant in UTC, an
/// instant at a fixed offset, an instant in a named IANA zone, or a wall clock
/// naming no instant. Same reasoning as 0.16.0 for why a new column is not the
/// additive case. It also changes how stored timestamps are *interpreted*: a
/// row marked `zoneless` holds wall clocks the store keeps as if UTC, which an
/// older reader would hand back as instants. Stores written by 0.16.0 and
/// earlier are rejected on open.
///
/// 0.18.0 added the `association_id` column to `time_series_associations`: a
/// derived surrogate id (`hash::association_id`) over the `uq_ts_assoc` tuple,
/// enforced UNIQUE by its own index. Same reasoning as 0.15.0-0.17.0 for why a
/// new NOT NULL column is not the additive case -- `CREATE TABLE IF NOT
/// EXISTS` leaves a 0.17.0 store's column set alone, so every INSERT naming
/// the column fails against it. It also adds a hash domain: the encoding
/// `hash::association_id` computes is part of the on-disk contract, so a
/// future change to that encoding is itself a format bump, not merely a code
/// change. Stores written by 0.17.0 and earlier are rejected on open.
///
/// 0.19.0 keeps that column but changes what fills it: the id is now minted
/// from the new `association_id_sequence` table instead of hashed from the
/// identity tuple. This is a format break in both directions, and the column
/// set alone does not express it. A 0.18.0 store's ids are hashes over a domain
/// that no longer exists, and it carries no sequence row, so a 0.19.0 writer
/// reading one would mint from 1 and collide with rows already holding
/// arbitrary 53-bit values; a 0.18.0 reader opening a 0.19.0 store would find
/// ids that do not reproduce from the tuple and reject every row as corrupt.
/// The hash domain is retired with it -- there is no derivation left whose
/// encoding a later change could break. Stores written by 0.18.0 and earlier
/// are rejected on open.
pub const DATA_FORMAT_VERSION: &str = "0.19.0";
