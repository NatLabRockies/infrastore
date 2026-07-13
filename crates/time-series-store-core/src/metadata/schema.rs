/// SQLite DDL applied at store creation. Idempotent (`CREATE TABLE IF NOT EXISTS`).
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS time_series_associations (
    id                INTEGER PRIMARY KEY,
    owner_id          INTEGER NOT NULL,
    owner_type        TEXT    NOT NULL,
    owner_category    TEXT    NOT NULL CHECK(owner_category IN ('Component','SupplementalAttribute')),
    time_series_type  TEXT    NOT NULL,
    name              TEXT    NOT NULL,
    data_hash         BLOB    NOT NULL,
    initial_timestamp TEXT,
    resolution        TEXT,
    length            INTEGER,
    horizon           TEXT,
    interval          TEXT,
    count             INTEGER,
    timestamps_json   TEXT,
    units             TEXT,
    percentiles_json  TEXT,
    dtype             TEXT    NOT NULL DEFAULT 'f64',
    element_shape     TEXT,
    logical_type      TEXT,
    features_hash     BLOB    NOT NULL
);

-- Feature sets are content-addressed by the SHA-256 of the feature map, exactly
-- as arrays are content-addressed by the hash of their bytes. A feature set is
-- stored ONCE and shared by every association whose `features_hash` matches; the
-- association table carries that hash already, so no join column is needed.
--
-- This is what keeps a derived view cheap: a DeterministicSingleTimeSeries has
-- the same features as the SingleTimeSeries it is derived from, so
-- `transform_single_time_series` writes no feature rows at all. It also collapses
-- the common real-world case where thousands of components share one feature set
-- (often the empty set, or a single scenario tag).
--
-- There is deliberately NO foreign key to time_series_associations and NO
-- cascade: rows here are shared, so deleting one association must not delete a
-- set another association still uses. Deleting the last user of a set instead
-- leaves it unreachable, mirroring the NetCDF side's unreachable standalone
-- variables. `Store::compact` sweeps unreachable sets.
CREATE TABLE IF NOT EXISTS feature_sets (
    features_hash     BLOB    NOT NULL,
    key               TEXT    NOT NULL,
    value_kind        TEXT    NOT NULL CHECK(value_kind IN ('int','float','bool','str')),
    value_int         INTEGER,
    value_float       REAL,
    value_bool        INTEGER,
    value_str         TEXT,
    PRIMARY KEY (features_hash, key)
);

-- The store's uniqueness invariant is
--   (owner_id, owner_category, time_series_type, name, resolution, interval,
--    features).
-- owner_category is part of the owner identity: component and supplemental-
-- attribute id streams are independent, so the same owner_id can name a
-- component and an attribute; the category keeps their associations distinct.
-- `interval` is part of the identity (matching InfrastructureSystems.jl): two
-- forecasts of one variable at the same resolution but different intervals
-- (e.g. a day-ahead and a real-time forecast) are distinct series. It is NULL
-- for static types (Single/NonSequential), which never carry an interval.
-- Two indexes are required to enforce and serve it, and BOTH must be kept:
--
--   * uq_assoc indexes resolution and interval as plain columns. It serves the
--     equality/IS NULL lookups in get_by_key/list/delete_by_key (an expression
--     index cannot be used for those), but it does NOT enforce uniqueness when
--     resolution or interval IS NULL, because SQLite treats NULLs as distinct in
--     a UNIQUE index.
--   * uq_assoc_coalesced closes that gap by COALESCE-ing NULL resolutions and
--     intervals to a sentinel, so NULL-resolution/NULL-interval types (e.g.
--     NonSequentialTimeSeries, and any static series) also get the uniqueness
--     guarantee. The sentinel is the empty string, which is never a valid
--     ISO-8601 period and so cannot collide with a real one.
--
-- Resolution and interval are stored as ISO-8601 duration strings (e.g. 'PT1H',
-- 'P1M', 'P1Y') so calendar (irregular) periods are distinguishable from fixed
-- ones.
--
-- Do not "deduplicate" these into one index: dropping uq_assoc loses the query
-- index; dropping uq_assoc_coalesced loses NULL-resolution/NULL-interval
-- uniqueness.
CREATE UNIQUE INDEX IF NOT EXISTS uq_assoc ON time_series_associations
    (owner_id, owner_category, time_series_type, name, resolution, interval, features_hash);
CREATE UNIQUE INDEX IF NOT EXISTS uq_assoc_coalesced ON time_series_associations
    (owner_id, owner_category, time_series_type, name,
     COALESCE(resolution, ''), COALESCE(interval, ''), features_hash);

CREATE INDEX IF NOT EXISTS ix_hash       ON time_series_associations(data_hash);
CREATE INDEX IF NOT EXISTS ix_owner      ON time_series_associations(owner_id, owner_category);
CREATE INDEX IF NOT EXISTS ix_resolution ON time_series_associations(resolution);

CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
"#;
