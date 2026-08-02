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
    -- `owner_category` and `time_series_type` are stored as small INTEGER
    -- codes (`OwnerCategory::code` / `TimeSeriesType::code`), not names. Both
    -- sit in the wide composite indexes below, where a 1-byte code instead of a
    -- 9-29 byte string shrinks the type/category-bearing indexes ~35% and the
    -- whole catalog ~29% on a 400k-row store. The codes are an on-disk
    -- contract; see `DATA_FORMAT_VERSION`.
    owner_category    INTEGER NOT NULL CHECK(owner_category IN (0,1)),
    time_series_type  INTEGER NOT NULL CHECK(time_series_type BETWEEN 0 AND 5),
    name              TEXT    NOT NULL,
    initial_timestamp TEXT,
    resolution        TEXT,
    length            INTEGER,
    horizon           TEXT,
    interval          TEXT,
    count             INTEGER,
    -- Content hash of this row's explicit timestamp vector, resolved through the
    -- `timestamp_sets` table below. NULL for every type but
    -- NonSequentialTimeSeries, which is the only one that carries one.
    timestamps_hash   BLOB,
    units             TEXT,
    percentiles_json  TEXT,
    -- The logical element type in its canonical string form (`ElementType`):
    -- a dtype spelling for plain scalars, else `tuple(N,dtype)` or one of the
    -- function-data kinds. It supersedes the physical `dtype` column: the dtype
    -- of the stored bytes is derived from it.
    element_type      TEXT    NOT NULL DEFAULT 'f64',
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

-- The explicit timestamp vector of a `NonSequentialTimeSeries`, content-
-- addressed by the SHA-256 of its canonical encoding, exactly as feature sets
-- are above. The association table carries only the hash.
--
-- Three things this buys, in descending order of how much they matter:
--
--   * The vector is stored ONCE per distinct time axis. Irregular series in a
--     power-systems model overwhelmingly share one — event times, an outage
--     schedule, a market timeline — so a thousand components sampled at the same
--     instants hold one copy between them rather than a thousand.
--   * The association table stays narrow. The vector used to live inline as an
--     RFC3339 JSON array (24 bytes per timestamp, ~210 KB for a year of hourly
--     data), which pushed those rows into SQLite overflow pages and made every
--     scan-shaped catalog query — `list`, the filters, the summaries — read and
--     parse megabytes it had no use for. `list` now hydrates each DISTINCT
--     vector once, the way it already does for features.
--   * It is the cohort key the packed HDF5 layout groups irregular arrays by.
--     Series that share a time axis are column-packed into one timestamp-major
--     dataset, which is what lets `StaticReader` sweep them (see
--     `storage/common.rs`).
--
-- `data` is the encoding from `crate::timestamps` — deltas as varints in the
-- coarsest unit that divides them, ~1 byte per timestamp for a regular grid.
-- It is not human-readable, deliberately: the catalog is a machine artifact and
-- the readable projection below covers hand inspection of the rest.
--
-- Like `feature_sets`, there is deliberately NO foreign key and NO cascade:
-- rows are shared, so deleting one association must not delete a vector another
-- still uses. `Store::compact` sweeps the ones nothing references any more.
CREATE TABLE IF NOT EXISTS timestamp_sets (
    timestamps_hash   BLOB    NOT NULL PRIMARY KEY,
    data              BLOB    NOT NULL
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
-- predicate below is a full-table scan, and the table's rows are wide enough
-- (ext) that scans get expensive well before row counts get large. Measured on
-- a 405k-row catalog (100k owners):
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

-- Pairs this catalog with exactly one HDF5 file, whose `catalog_generation`
-- root attribute holds the same value. `Store::open` compares the two and
-- rejects a mismatch, which is what turns an interrupted `persist_to` — the two
-- files are renamed into place one after the other and cannot be swapped
-- atomically — into a loud error instead of a store that quietly disagrees with
-- itself. Also catches one half being copied without the other.
--
-- Added additively (see the `DDL` doc comment): a store written before the stamp
-- existed has no row here and no root attribute, and reads as unstamped rather
-- than as a mismatch. Holds zero or one row; reads must tolerate the table being
-- absent, because a read-only open cannot run this DDL.
CREATE TABLE IF NOT EXISTS catalog_identity (generation TEXT NOT NULL);

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

-- A readable projection of the association table, for humans opening the
-- catalog in `sqlite3` directly. The two content-address hashes are BLOBs, and
-- sqlite3 renders a BLOB as raw bytes in its default `list` mode and in
-- `.mode box`/`.mode json` -- which corrupts the terminal and, in box mode, the
-- table borders themselves. This view hands back the same rows with both hashes
-- hex-encoded.
--
-- The encoding is `lower(hex(...))`, not bare `hex(...)`: SQLite's `hex()`
-- returns uppercase, while `hash_hex` in `crate::hash` -- the spelling every
-- binding, the CLI, and every error message uses -- is lowercase. Lowercasing
-- here means a hash copied out of this view pastes straight into a CLI
-- `--data-hash` argument and compares equal to one printed by any binding.
--
-- The hashes stay BLOB in the base table deliberately. Hex TEXT would cost
-- ~32% more catalog space (measured: the two columns sit in the table, in
-- `idx_hash`, and in BOTH unique indexes), add hex decoding to the per-row
-- parse path, and -- worst for the very use case this view serves -- make
-- lookups case-sensitive, so a hash copied from `hex()` would silently match
-- zero rows. A BLOB literal (`X'..'`) is case-free.
--
-- Additive, like the association tables above: the DDL is idempotent, so an
-- existing store gains the view on its first writable open and no
-- DATA_FORMAT_VERSION bump is needed. Nothing in this crate reads the view --
-- it exists purely for outside inspection -- so a read-only open of an older
-- store that lacks it is harmless.
-- Hand-inspection view: decodes the two integer discriminants and both content
-- hashes back to readable text. Nothing in the library reads it.
CREATE VIEW IF NOT EXISTS time_series_readable AS
SELECT id, owner_id, owner_type,
       CASE owner_category WHEN 0 THEN 'Component'
                           WHEN 1 THEN 'SupplementalAttribute'
                           ELSE 'unknown(' || owner_category || ')' END AS owner_category,
       CASE time_series_type WHEN 0 THEN 'SingleTimeSeries'
                             WHEN 1 THEN 'NonSequentialTimeSeries'
                             WHEN 2 THEN 'Deterministic'
                             WHEN 3 THEN 'DeterministicSingleTimeSeries'
                             WHEN 4 THEN 'Probabilistic'
                             WHEN 5 THEN 'Scenarios'
                             ELSE 'unknown(' || time_series_type || ')' END AS time_series_type,
       name,
       initial_timestamp, resolution, length, horizon, interval, count,
       units, element_type, element_shape, ext,
       lower(hex(data_hash))       AS data_hash,
       lower(hex(features_hash))   AS features_hash,
       lower(hex(timestamps_hash)) AS timestamps_hash
FROM time_series_associations;
"#;
