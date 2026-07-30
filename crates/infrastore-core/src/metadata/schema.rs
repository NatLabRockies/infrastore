/// SQLite DDL applied on every writable open, not just at creation. Idempotent
/// (`CREATE TABLE IF NOT EXISTS` / `CREATE ... INDEX IF NOT EXISTS`), which is
/// what lets a purely additive table land without a `DATA_FORMAT_VERSION` bump:
/// an older store picks the new table up the first time it is opened for
/// writing. Read-only opens skip the DDL entirely, so any table added this way
/// must be optional on the read path.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS time_series_associations (
    id                INTEGER PRIMARY KEY,
    owner_id          INTEGER NOT NULL,
    owner_type        TEXT    NOT NULL,
    owner_category    TEXT    NOT NULL CHECK(owner_category IN ('Component','SupplementalAttribute')),
    time_series_type  TEXT    NOT NULL,
    name              TEXT    NOT NULL,
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
    ext      TEXT,
    -- Content-address hashes are grouped at the end of the row. Column order is
    -- cosmetic: every INSERT/SELECT in metadata.rs names its columns explicitly,
    -- so nothing depends on ordinal position.
    data_hash         BLOB    NOT NULL,
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
-- leaves it unreachable, mirroring the HDF5 side's unreachable standalone
-- variables. `Store::compact` sweeps unreachable sets.
CREATE TABLE IF NOT EXISTS feature_sets (
    key               TEXT    NOT NULL,
    value_kind        TEXT    NOT NULL CHECK(value_kind IN ('int','float','bool','str')),
    value_int         INTEGER,
    value_float       REAL,
    value_bool        INTEGER,
    value_str         TEXT,
    features_hash     BLOB    NOT NULL,
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
--   * uq_ts_assoc indexes resolution and interval as plain columns. It serves
--     the equality/IS NULL lookups in get_by_key/list/delete_by_key (an
--     expression index cannot be used for those), but it does NOT enforce
--     uniqueness when resolution or interval IS NULL, because SQLite treats
--     NULLs as distinct in a UNIQUE index.
--   * uq_ts_assoc_coalesced closes that gap by COALESCE-ing NULL resolutions and
--     intervals to a sentinel, so NULL-resolution/NULL-interval types (e.g.
--     NonSequentialTimeSeries, and any static series) also get the uniqueness
--     guarantee. The sentinel is the empty string, which is never a valid
--     ISO-8601 period and so cannot collide with a real one.
--
-- Resolution and interval are stored as ISO-8601 duration strings (e.g. 'PT1H',
-- 'P1M', 'P1Y') so calendar (irregular) periods are distinguishable from fixed
-- ones.
--
-- Do not "deduplicate" these into one index: dropping uq_ts_assoc loses the
-- query index; dropping uq_ts_assoc_coalesced loses NULL-resolution/
-- NULL-interval uniqueness.
--
-- These two carried the shorter names uq_assoc / uq_assoc_coalesced until the
-- association tables below arrived and made a bare "assoc" ambiguous. The DROPs
-- rename them in place on an existing store: without them, `CREATE ... IF NOT
-- EXISTS` would add the new pair and leave the old pair behind, so every insert
-- would maintain four indexes instead of two. Both are no-ops on a fresh store,
-- and re-creating a UNIQUE index cannot fail here because the index being
-- dropped enforced the very same constraint. Index names are not part of the
-- on-disk contract, so this needs no DATA_FORMAT_VERSION bump; an older build
-- opening the store would simply re-create the old names alongside.
DROP INDEX IF EXISTS uq_assoc;
DROP INDEX IF EXISTS uq_assoc_coalesced;
CREATE UNIQUE INDEX IF NOT EXISTS uq_ts_assoc ON time_series_associations
    (owner_id, owner_category, time_series_type, name, resolution, interval, features_hash);
CREATE UNIQUE INDEX IF NOT EXISTS uq_ts_assoc_coalesced ON time_series_associations
    (owner_id, owner_category, time_series_type, name,
     COALESCE(resolution, ''), COALESCE(interval, ''), features_hash);

CREATE INDEX IF NOT EXISTS idx_hash       ON time_series_associations(data_hash);
CREATE INDEX IF NOT EXISTS idx_owner      ON time_series_associations(owner_id, owner_category);
CREATE INDEX IF NOT EXISTS idx_resolution ON time_series_associations(resolution);

-- Secondary indexes for the filter/discovery surface. Without these, every
-- predicate below is a full-table scan, and the table's rows are wide (ext,
-- timestamps_json), so scans get expensive well before row counts get large.
-- Measured on a 405k-row catalog (100k owners):
--
--   * idx_ts_type       count_by_type / counts_by_type / list() with a type
--                       predicate: 3-10x. Serves every stats and summary call
--                       that scopes by time_series_type.
--   * idx_name          exact-name filters 12x; name GLOB with a literal
--                       prefix 6x (BINARY collation lets GLOB range-seek).
--                       Leading-wildcard patterns still scan, but over the
--                       narrow index instead of the wide table.
--   * idx_owner_type    owner_type filters 34x; makes DISTINCT owner_type a
--                       covering scan.
--   * idx_category_owner  category-scoped owner enumeration and counts 4-8x.
--                       idx_owner leads with owner_id, so it cannot serve a
--                       category-only predicate; leading with the category
--                       (then owner_id, keeping DISTINCT owner_id covered)
--                       does. Near-zero insert cost (two small columns).
--   * idx_interval      distinct_intervals becomes a covering range seek
--                       (was a full scan + temp b-tree); the interval
--                       counterpart of idx_resolution.
--
-- Deliberately NOT added, per the same measurements:
--
--   * (features_hash) alone — would only speed the orphan-set sweep, which runs
--     at compact time, and a 32-byte BLOB index is comparatively expensive to
--     maintain on every insert.
--   * (time_series_type, data_hash) — 23x on count_distinct_arrays_for_types,
--     but +62% metadata-insert cost and it baits the planner into a
--     non-covering seek that doubles list_identities' latency. The plain
--     idx_ts_type is the better trade.
--
-- Known planner trade-off accepted here: with idx_ts_type / idx_owner_type
-- present, list_identities and static_summary pick index-assisted plans that
-- are ~10-20% slower than their previous covering full scans. Both are
-- infrequent reporting calls; the wins above are on the hot filter paths.
--
-- Like every index here, these are additive: an existing store gains them on
-- its first writable open (one-time build cost proportional to catalog size).
CREATE INDEX IF NOT EXISTS idx_ts_type        ON time_series_associations(time_series_type);
CREATE INDEX IF NOT EXISTS idx_name           ON time_series_associations(name);
CREATE INDEX IF NOT EXISTS idx_owner_type     ON time_series_associations(owner_type);
CREATE INDEX IF NOT EXISTS idx_category_owner ON time_series_associations(owner_category, owner_id);
CREATE INDEX IF NOT EXISTS idx_interval       ON time_series_associations(interval);

CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);

-- The two association tables below record relationships between catalog
-- entities, independent of time series. They are deliberately separate rather
-- than one generic endpoint table: attaching an attribute to a component and
-- wiring a component to another component are different relationships, and
-- naming the columns after them keeps every query and error message
-- self-describing. Both are the same *shape* — a pair of (id, type) endpoints —
-- so the Rust side renders their SQL from one table descriptor.
--
-- Properties shared by both, and by neither the caller's nor SQLite's default:
--
--   * NO foreign keys and NO cascade. The endpoints live in the consumer's own
--     object graph, so this store never observes a component or an attribute
--     being deleted and a cascade could never fire. Consumers call the matching
--     `remove_*` explicitly instead.
--   * Independent of `time_series_associations`. Removing a time series never
--     touches these rows and vice versa; a consumer wanting both effects makes
--     both calls.
--   * Added additively, without bumping DATA_FORMAT_VERSION. The DDL is
--     idempotent, so an older store gains both tables on its first writable
--     open and older code ignores them. A read-only open of an older store
--     cannot run DDL, so every read of these tables tolerates the table being
--     absent (see `MetadataStore::has_supplemental_attribute_table` and
--     `has_parent_child_table`).

-- Which supplemental attributes are attached to which components. Columns match
-- infrasys' table of the same name, whose logic this replaces (IS3.jl kept an
-- equivalent table under a different name).
--
-- Identity is the (component_id, attribute_id) pair; the type columns are
-- denormalized labels carried for filtering, not part of identity. One
-- component may carry an attribute at most once.
CREATE TABLE IF NOT EXISTS supplemental_attribute_associations (
    id             INTEGER PRIMARY KEY,
    component_id   INTEGER NOT NULL,
    component_type TEXT    NOT NULL,
    attribute_id   INTEGER NOT NULL,
    attribute_type TEXT    NOT NULL
);

-- uq_sa_assoc doubles as the by-component query index; the reverse direction
-- ("which components carry this attribute") needs its own.
CREATE UNIQUE INDEX IF NOT EXISTS uq_sa_assoc
    ON supplemental_attribute_associations(component_id, attribute_id);
CREATE INDEX IF NOT EXISTS idx_sa_assoc_attribute
    ON supplemental_attribute_associations(attribute_id, component_id, component_type);

-- Directed parent/child edges between components — e.g. a generator (parent)
-- connected to a bus (child). Both endpoints are always components, which is
-- why there is no category column: a supplemental attribute cannot appear here
-- by construction.
--
-- Identity is the (parent_id, child_id) pair. One pair is related at most once;
-- there is no relationship-kind column, so a second kind of edge between the
-- same two components would need one added (and the unique index widened).
CREATE TABLE IF NOT EXISTS parent_child_associations (
    id          INTEGER PRIMARY KEY,
    parent_id   INTEGER NOT NULL,
    parent_type TEXT    NOT NULL,
    child_id    INTEGER NOT NULL,
    child_type  TEXT    NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_parent_child
    ON parent_child_associations(parent_id, child_id);
CREATE INDEX IF NOT EXISTS idx_parent_child_child
    ON parent_child_associations(child_id, parent_id, parent_type);
"#;
