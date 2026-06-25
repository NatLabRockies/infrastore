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

CREATE TABLE IF NOT EXISTS features (
    association_id    INTEGER NOT NULL REFERENCES time_series_associations(id) ON DELETE CASCADE,
    key               TEXT    NOT NULL,
    value_kind        TEXT    NOT NULL CHECK(value_kind IN ('int','float','bool','str')),
    value_int         INTEGER,
    value_float       REAL,
    value_bool        INTEGER,
    value_str         TEXT,
    PRIMARY KEY (association_id, key)
);

-- The store's uniqueness invariant is
--   (owner_id, owner_category, time_series_type, name, resolution, features).
-- owner_category is part of the owner identity: component and supplemental-
-- attribute id streams are independent, so the same owner_id can name a
-- component and an attribute; the category keeps their associations distinct.
-- Two indexes are required to enforce and serve it, and BOTH must be kept:
--
--   * uq_assoc indexes resolution as a plain column. It serves the
--     equality/IS NULL lookups in get_by_key/list/delete_by_key (an expression
--     index cannot be used for those), but it does NOT enforce uniqueness when
--     resolution IS NULL, because SQLite treats NULLs as distinct in a
--     UNIQUE index.
--   * uq_assoc_null_resolution closes that gap by COALESCE-ing NULL resolutions
--     to a sentinel, so NULL-resolution types (e.g. NonSequentialTimeSeries)
--     also get the uniqueness guarantee. The sentinel is the empty string, which
--     is never a valid ISO-8601 period and so cannot collide with a real one.
--
-- Resolution is stored as an ISO-8601 duration string (e.g. 'PT1H', 'P1M',
-- 'P1Y') so calendar (irregular) periods are distinguishable from fixed ones.
--
-- Do not "deduplicate" these into one index: dropping uq_assoc loses the query
-- index; dropping uq_assoc_null_resolution loses NULL-resolution uniqueness.
CREATE UNIQUE INDEX IF NOT EXISTS uq_assoc ON time_series_associations
    (owner_id, owner_category, time_series_type, name, resolution, features_hash);
CREATE UNIQUE INDEX IF NOT EXISTS uq_assoc_null_resolution ON time_series_associations
    (owner_id, owner_category, time_series_type, name, COALESCE(resolution, ''), features_hash);

CREATE INDEX IF NOT EXISTS ix_hash       ON time_series_associations(data_hash);
CREATE INDEX IF NOT EXISTS ix_owner      ON time_series_associations(owner_id, owner_category);
CREATE INDEX IF NOT EXISTS ix_resolution ON time_series_associations(resolution);

CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
"#;
