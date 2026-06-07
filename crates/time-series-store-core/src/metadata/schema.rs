/// SQLite DDL applied at store creation. Idempotent (`CREATE TABLE IF NOT EXISTS`).
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS time_series_associations (
    id                INTEGER PRIMARY KEY,
    owner_uuid        TEXT    NOT NULL,
    owner_type        TEXT    NOT NULL,
    owner_category    TEXT    NOT NULL CHECK(owner_category IN ('Component','SupplementalAttribute')),
    time_series_type  TEXT    NOT NULL,
    name              TEXT    NOT NULL,
    data_hash         BLOB    NOT NULL,
    initial_timestamp TEXT,
    resolution_ns     INTEGER,
    length            INTEGER,
    horizon_ns        INTEGER,
    interval_ns       INTEGER,
    count             INTEGER,
    timestamps_json   TEXT,
    scaling_factor    TEXT,
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

CREATE UNIQUE INDEX IF NOT EXISTS uq_assoc ON time_series_associations
    (owner_uuid, time_series_type, name, resolution_ns, features_hash);
CREATE UNIQUE INDEX IF NOT EXISTS uq_assoc_null_resolution ON time_series_associations
    (owner_uuid, time_series_type, name, COALESCE(resolution_ns, -9223372036854775808), features_hash);

CREATE INDEX IF NOT EXISTS ix_hash       ON time_series_associations(data_hash);
CREATE INDEX IF NOT EXISTS ix_owner      ON time_series_associations(owner_uuid);
CREATE INDEX IF NOT EXISTS ix_resolution ON time_series_associations(resolution_ns);

CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
"#;
