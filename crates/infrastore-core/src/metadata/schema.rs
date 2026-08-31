/// SQLite DDL applied on every writable open, not just at creation. Idempotent
/// (`CREATE TABLE IF NOT EXISTS` / `CREATE ... INDEX IF NOT EXISTS`), which is
/// what lets a purely additive table land without a `DATA_FORMAT_VERSION` bump:
/// an older store picks the new table up the first time it is opened for
/// writing. Read-only opens skip the DDL entirely, so any table added this way
/// must be optional on the read path.
///
/// Idempotent is not the same as version-agnostic. `IF NOT EXISTS` suppresses
/// "already exists"; it does not stop SQLite from resolving the statement's
/// column references, so a statement naming a column that a format bump
/// introduced (`idx_component_field`, below) fails outright against an older
/// catalog. Nor will `CREATE TABLE IF NOT EXISTS` *alter* a table that is
/// already there, so a new column or a changed CHECK never reaches an existing
/// store through this DDL at all.
///
/// That is what [`crate::metadata::migrate`] is for: a catalog change the DDL
/// cannot make to an existing table needs a `CATALOG_SCHEMA_REVISION` bump and
/// an append-only `MIGRATIONS` entry, and the ladder runs *before* this DDL on
/// every writable open. This DDL is then the additive catch-up pass: new
/// tables, new indexes -- including the ones a migration's table rebuild
/// dropped -- and the re-created view.
///
/// `Store::open_with_catalog` still evaluates the HDF5 half's
/// `data_format_version` before opening the catalog: a store too old to
/// migrate at all should report that, not a raw SQLite error from inside a
/// later query.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS time_series_associations (
    -- This id is an *external* reference: a consumer stores it in its own
    -- object model (a generator's cost function naming the series that varies
    -- it) and expects it to keep meaning the same row, which AUTOINCREMENT
    -- guarantees by never reissuing one a delete freed.
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id          INTEGER NOT NULL,
    owner_type        TEXT    NOT NULL,
    -- `owner_category` and `time_series_type` are stored as small INTEGER
    -- codes (`OwnerCategory::code` / `TimeSeriesType::code`), not names. Both
    -- sit in the wide composite indexes below, where a 1-byte code instead of a
    -- 9-29 byte string shrinks the type/category-bearing indexes ~35% and the
    -- whole catalog ~29% on a 400k-row store. The codes are an on-disk
    -- contract; see `DATA_FORMAT_VERSION`.
    owner_category    INTEGER NOT NULL CHECK(owner_category IN (0,1)),
    -- The enum owns this domain, not SQLite. `TimeSeriesType::from_code` is the
    -- real gate and runs on every write and every read; a numeric bound here
    -- only turns appending a type into a table rebuild (which is what catalog
    -- revision 2 had to do -- see `metadata::migrate`). Kept as a
    -- non-negativity check so a corrupted or garbage value is still refused.
    time_series_type  INTEGER NOT NULL CHECK(time_series_type >= 0),
    name              TEXT    NOT NULL,
    initial_timestamp TEXT,
    resolution        TEXT,
    length            INTEGER,
    horizon           TEXT,
    interval          TEXT,
    count             INTEGER,
    -- Content hash of this row's explicit timestamp vector. NULL for every type
    -- but NonSequentialTimeSeries, which is the only one that carries one.
    --
    -- The vector itself is NOT here: it is data, stored once per distinct time
    -- axis in the HDF5 file beside the arrays (see `crate::timestamps`). The
    -- catalog holds the hash alone, which is both the locator and the cohort key
    -- the packed HDF5 layout groups irregular arrays by -- series sharing a time
    -- axis are column-packed into one timestamp-major dataset, which is what
    -- lets `StaticReader` sweep them (see `storage/common.rs`).
    timestamps_hash   BLOB,
    units             TEXT,
    -- What kind of physical quantity the values measure, free-form, with QUDT
    -- `QuantityKind` local names as the recommended vocabulary. Deliberately
    -- unconstrained: composite economic quantities an energy modeler needs
    -- ($/MWh, MMBtu/MWh) are exactly where QUDT's coverage thins out, so a
    -- CHECK here would turn a fuel-price series into a schema migration.
    quantity_kind     TEXT,
    -- `UnitSystem::as_str`: 'natural_units' or 'component_base'. NULL means
    -- unspecified, NOT natural units -- every row written before this column
    -- existed is NULL. Stored as text rather than an integer code because,
    -- unlike `owner_category` and `time_series_type`, it sits in no index, and
    -- left without a CHECK so a third basis can land without bumping
    -- `DATA_FORMAT_VERSION`.
    unit_system       TEXT,
    -- `TimeReference::as_storage_string`: 'utc', 'zoneless', a fixed offset
    -- ('-07:00'), or an IANA zone name ('America/Denver'). NULL means
    -- unspecified, which groups with the zoned spellings for query bounds but
    -- is not a claim the timestamps were written as UTC.
    --
    -- One TEXT column holds all four spellings unambiguously because
    -- `TimeReference::validate` refuses a zone name that reads as an offset or
    -- as either literal -- shape validation in the core is what makes that true
    -- rather than merely hoped for. Existence of a zone is deliberately not
    -- checked anywhere: see `TimeReference::validate`.
    --
    -- Deliberately NOT indexed, and this does not change now that
    -- `ListFilter::zoneless` exists. The `idx_component_field` partial-index
    -- pattern is wrong here twice over: `WHERE time_reference IS NOT NULL` would
    -- exclude exactly the NULL rows that filter has to return, and the column is
    -- low-cardinality -- a handful of distinct values across a whole store -- so
    -- it is not selective enough to earn an index. In practice it is combined
    -- with `owner_id` or `name`, which are indexed.
    time_reference    TEXT,
    -- The field on the owning component (or supplemental attribute) whose value
    -- these values are the time-varying form of, e.g. 'max_active_power'. Free
    -- form and never interpreted here: it names a field in the consumer's own
    -- object model, which this store has no view of. Deliberately separate from
    -- `name`, which is part of the row's identity and often carries a
    -- disambiguating suffix; this one records only what the values are for.
    component_field   TEXT,
    percentiles_json  TEXT,
    -- The logical element type in its canonical string form (`ElementType`):
    -- a dtype spelling for plain scalars, else `tuple(N,dtype)` or one of the
    -- function-data kinds. It supersedes the physical `dtype` column: the dtype
    -- of the stored bytes is derived from it.
    element_type      TEXT    NOT NULL DEFAULT 'f64',
    element_shape     TEXT,
    application_data  TEXT,
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
-- predicate below is a full-table scan, and the table's rows are wide enough
-- (application_data) that scans get expensive well before row counts get
-- large. Measured on
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

-- `component_field` filters ("every series that varies max_active_power"), the
-- same shape of predicate `idx_name` serves for `name`, on the same table and
-- the same kind of TEXT column — so it earns an index for the same reason, and
-- is not separately measured.
--
-- PARTIAL, unlike every index above, because this column is the only optional
-- one anything filters on: a store that never sets it (every store written
-- before the column existed, and every consumer that does not use it) would
-- otherwise pay index maintenance on every insert to record one NULL per row
-- and buy nothing. `WHERE component_field IS NOT NULL` makes that case cost
-- exactly zero entries. SQLite still uses the index for the predicate we
-- actually issue: `component_field = ?` cannot be true of a NULL whatever the
-- parameter binds to, so the planner can prove the partial index's condition
-- (asserted by `component_field_filter_uses_its_partial_index`).
--
-- The consequence to know, and the reason `ListFilter::component_field` says
-- so: this index can never serve "the rows that left it unset". Nothing asks
-- for that today; a caller that needs it wants an `IS NULL` predicate and a
-- full index, not this one.
--
-- Unlike the indexes above, this one is NOT additive to an arbitrary existing
-- store: it names a column introduced by the same `DATA_FORMAT_VERSION` bump,
-- so it can only be applied to a catalog already carrying that column. Nothing
-- special is needed to make that hold -- an older store is rejected by the
-- version check before this DDL runs (see the `DDL` doc comment) -- but an
-- index over a newly added column must never be assumed version-free.
CREATE INDEX IF NOT EXISTS idx_component_field ON time_series_associations(component_field)
    WHERE component_field IS NOT NULL;

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
--   * Independent id streams. Each table has its own `sqlite_sequence` row, so
--     an id is only meaningful together with the table it came from. Equal
--     values across two tables are the common case, not a collision — each
--     counter starts at 1 — and mean nothing; only uniqueness *within* a table
--     is guaranteed.

-- Which supplemental attributes are attached to which components. Columns match
-- infrasys' table of the same name, whose logic this replaces (IS3.jl kept an
-- equivalent table under a different name).
--
-- Identity is the (component_id, attribute_id) pair; the type columns are
-- denormalized labels carried for filtering, not part of identity. One
-- component may carry an attribute at most once.
CREATE TABLE IF NOT EXISTS supplemental_attribute_associations (
    -- AUTOINCREMENT for the reason given on `time_series_associations.id`.
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
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
    -- AUTOINCREMENT for the reason given on `time_series_associations.id`.
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
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
--
-- Dropped and recreated rather than `CREATE ... IF NOT EXISTS`, so a store
-- written by an older build picks up the current definition on its next
-- writable open instead of keeping a stale one forever. A view holds no data,
-- so this costs nothing and cannot lose anything.
DROP VIEW IF EXISTS time_series_readable;
CREATE VIEW time_series_readable AS
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
       units, quantity_kind, unit_system, time_reference, component_field,
       element_type, element_shape, application_data,
       lower(hex(data_hash))       AS data_hash,
       lower(hex(features_hash))   AS features_hash,
       -- `hex()` is not NULL-propagating: it renders NULL as the empty string,
       -- so an absent timestamps hash came through the view as `''` with
       -- `typeof` 'text', indistinguishable from a genuinely empty blob and
       -- invisible to `WHERE timestamps_hash IS NULL`. Only
       -- `NonSequentialTimeSeries` rows carry one, so that was almost every row.
       CASE WHEN timestamps_hash IS NULL THEN NULL
            ELSE lower(hex(timestamps_hash)) END AS timestamps_hash
FROM time_series_associations;
"#;
