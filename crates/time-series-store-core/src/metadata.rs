//! SQLite-backed metadata store.
//!
//! Stores [`TimeSeriesMetadata`] records and the (owner_id, type, name,
//! resolution, features) uniqueness invariant.

pub mod schema;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::types::period::Period;

use crate::error::{Result, TimeSeriesError};
use crate::hash::features_hash;
use crate::types::key::{KeyIdentity, TimeSeriesKey};
use crate::types::metadata::{FeatureValue, Features, OwnerCategory, TimeSeriesMetadata};
use crate::types::time_series::TimeSeriesType;

pub struct MetadataStore {
    conn: Connection,
    read_only: bool,
}

/// One grouped row of the static-series summary: a distinct
/// `(owner_type, owner_category, type, name, initial_timestamp, resolution,
/// length)` combination and how many associations share it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticSummaryRow {
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub initial_timestamp: Option<DateTime<Utc>>,
    pub resolution: Option<Period>,
    pub time_step_count: Option<i64>,
    pub count: i64,
}

/// One grouped row of the forecast summary: a distinct
/// `(owner_type, owner_category, type, name, initial_timestamp, resolution,
/// horizon, interval, window_count)` combination and how many associations
/// share it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastSummaryRow {
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub initial_timestamp: Option<DateTime<Utc>>,
    pub resolution: Option<Period>,
    pub horizon: Option<Period>,
    pub interval: Option<Period>,
    pub window_count: Option<i64>,
    pub count: i64,
}

fn parse_opt_rfc3339(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match s {
        None => Ok(None),
        Some(s) => Ok(Some(
            DateTime::parse_from_rfc3339(&s)
                .map_err(|e| TimeSeriesError::IntegrityError(format!("bad timestamp: {e}")))?
                .with_timezone(&Utc),
        )),
    }
}

fn parse_category(s: &str) -> Result<OwnerCategory> {
    OwnerCategory::parse(s)
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("bad owner_category {s}")))
}

fn parse_type(s: &str) -> Result<TimeSeriesType> {
    TimeSeriesType::parse(s)
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("bad time_series_type {s}")))
}

#[derive(Debug, Default, Clone)]
pub struct MetadataFilter {
    pub owner_id: Option<i64>,
    pub owner_category: Option<OwnerCategory>,
    pub owner_type: Option<String>,
    pub time_series_type: Option<TimeSeriesType>,
    pub name: Option<String>,
    /// SQLite `GLOB` pattern on the name (case-sensitive; `*`/`?` wildcards).
    /// Combined with `name` as AND when both are set.
    pub name_glob: Option<String>,
    pub resolution: Option<Period>,
    /// Forecast window interval. When set, restricts to rows with exactly this
    /// interval (part of the identity); `None` does not filter on interval.
    pub interval: Option<Period>,
    /// Subset match: rows must contain at least these key/value pairs.
    pub features: Option<Features>,
    /// Exact features-set match by precomputed hash. When set, this is pushed
    /// into the SQL WHERE so the `uq_assoc` unique index can pinpoint the row,
    /// avoiding a feature fetch+compare for siblings that share the other key
    /// columns. Distinct from `features` (an in-memory subset filter).
    pub features_hash: Option<[u8; 32]>,
}

/// Remembers which content-addressed feature sets a batch of inserts has already
/// written, so each distinct set is written once per batch rather than once per
/// row. Scoped to a single transaction: it records what *this* batch wrote, and
/// carries no meaning once that transaction ends.
#[derive(Debug, Default)]
pub struct FeatureSetCache {
    seen: HashSet<[u8; 32]>,
}

/// The full identity of one stored association: everything the uniqueness
/// invariant keys on. Periods stay in their catalog ISO-8601 encoding — this is
/// an equality/hash token, never a value to compute with.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AssociationIdentity {
    pub owner_id: i64,
    pub owner_category: OwnerCategory,
    pub name: String,
    pub resolution: Option<String>,
    pub interval: Option<String>,
    pub features_hash: [u8; 32],
}

/// An association identity with the interval projected away: the "family" of
/// series that describe one variable of one owner at one resolution, across all
/// forecast intervals.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeriesFamily {
    pub owner_id: i64,
    pub owner_category: OwnerCategory,
    pub name: String,
    pub resolution: Option<String>,
    pub features_hash: [u8; 32],
}

impl From<AssociationIdentity> for SeriesFamily {
    fn from(id: AssociationIdentity) -> Self {
        Self {
            owner_id: id.owner_id,
            owner_category: id.owner_category,
            name: id.name,
            resolution: id.resolution,
            features_hash: id.features_hash,
        }
    }
}

impl MetadataFilter {
    /// Render the filter as a `WHERE` clause plus its bound parameters, so the
    /// same predicate can be reused across the row query and the batched
    /// features query without building the SQL twice.
    ///
    /// `features` is not represented here: it is a subset match applied
    /// in memory after hydration, not a SQL predicate.
    fn to_sql(&self) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut sql = String::from("WHERE 1=1");
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(owner_id) = self.owner_id {
            sql.push_str(" AND owner_id = ?");
            params_vec.push(Box::new(owner_id));
        }
        if let Some(owner_category) = self.owner_category {
            sql.push_str(" AND owner_category = ?");
            params_vec.push(Box::new(owner_category.as_str().to_string()));
        }
        if let Some(ref owner_type) = self.owner_type {
            sql.push_str(" AND owner_type = ?");
            params_vec.push(Box::new(owner_type.clone()));
        }
        if let Some(ts_type) = self.time_series_type {
            sql.push_str(" AND time_series_type = ?");
            params_vec.push(Box::new(ts_type.as_str().to_string()));
        }
        if let Some(ref name) = self.name {
            sql.push_str(" AND name = ?");
            params_vec.push(Box::new(name.clone()));
        }
        if let Some(ref pattern) = self.name_glob {
            sql.push_str(" AND name GLOB ?");
            params_vec.push(Box::new(pattern.clone()));
        }
        if let Some(resolution) = self.resolution {
            sql.push_str(" AND resolution = ?");
            params_vec.push(Box::new(period_to_iso(resolution)));
        }
        if let Some(interval) = self.interval {
            sql.push_str(" AND interval = ?");
            params_vec.push(Box::new(period_to_iso(interval)));
        }
        if let Some(ref f_hash) = self.features_hash {
            sql.push_str(" AND features_hash = ?");
            params_vec.push(Box::new(f_hash.to_vec()));
        }
        (sql, params_vec)
    }
}

impl MetadataStore {
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Self {
            conn,
            read_only: false,
        })
    }

    /// Copy the entire metadata database to a new SQLite file at `path`
    /// (used to materialize an in-memory store to disk). SQLite's `VACUUM INTO`
    /// creates the target, which must not already exist.
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        self.conn
            .execute("VACUUM INTO ?1", params![path.to_string_lossy()])?;
        Ok(())
    }

    pub fn open_path(path: &Path, read_only: bool) -> Result<Self> {
        let conn = if read_only {
            let flags =
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI;
            Connection::open_with_flags(path, flags)?
        } else {
            Connection::open(path)?
        };
        Self::init(&conn)?;
        Ok(Self { conn, read_only })
    }

    fn init(conn: &Connection) -> Result<()> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        // Wait, rather than failing immediately with SQLITE_BUSY, when another
        // handle to the same on-disk artifact holds a lock (e.g. a CLI writer
        // and the read-only gRPC server overlapping). Harmless for in-memory and
        // read-only connections, which still acquire SHARED locks.
        //
        // NOTE: we intentionally do NOT switch to WAL / synchronous=NORMAL here.
        // WAL would raise write throughput, but it persists `-wal`/`-shm` sidecar
        // files (complicating the "move the .nc and .sqlite together" artifact
        // contract) and can prevent a read-only connection from opening the
        // database in some deployments. That trade-off deserves its own change.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // `prepare_cached` keys on SQL text, and `MetadataFilter` renders a
        // distinct statement per combination of set predicates (times two, since
        // `list` issues a row query and a features query). rusqlite's default
        // cache holds 16, which a mixed workload can thrash past, silently
        // re-parsing on every call. Room for the realistic shapes is cheap.
        conn.set_prepared_statement_cache_capacity(64);
        // Try to apply schema; on a read-only connection this no-ops because
        // CREATE TABLE IF NOT EXISTS against an empty read-only DB would error,
        // so we only run it when writes are possible.
        if !conn.is_readonly(rusqlite::DatabaseName::Main)? {
            conn.execute_batch(schema::DDL)?;
            // Insert the initial schema version row if absent.
            conn.execute(
                "INSERT INTO schema_version (version)
                 SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM schema_version)",
                [],
            )?;
        }
        Ok(())
    }

    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        Ok(self.conn.transaction()?)
    }

    /// Insert a metadata record + its features inside the supplied transaction.
    /// Returns the association id. Caller is responsible for committing.
    pub fn insert(tx: &Transaction<'_>, meta: &TimeSeriesMetadata) -> Result<i64> {
        Self::insert_batched(tx, meta, &mut FeatureSetCache::default())
    }

    /// [`Self::insert`], but reusing a caller-held [`FeatureSetCache`] across the
    /// rows of one batch.
    ///
    /// Feature sets are content-addressed and shared, so in a batch that inserts
    /// N rows over a handful of distinct sets, all but the first row per set
    /// would issue `INSERT OR IGNORE` statements that write nothing. The cache
    /// remembers which sets this batch has already written and skips the rest —
    /// which is what stops a bulk add, and a transform, from scaling with the
    /// number of features per series.
    pub fn insert_batched(
        tx: &Transaction<'_>,
        meta: &TimeSeriesMetadata,
        cache: &mut FeatureSetCache,
    ) -> Result<i64> {
        let f_hash = features_hash(&meta.features);
        let initial_ts = meta.initial_timestamp.map(|t| t.to_rfc3339());
        let resolution_iso = meta.resolution.map(period_to_iso);
        let horizon_iso = meta.horizon.map(period_to_iso);
        let interval_iso = meta.interval.map(period_to_iso);
        let timestamps_json = match &meta.timestamps {
            Some(ts) => Some(serde_json::to_string(ts)?),
            None => None,
        };
        let percentiles_json = match &meta.percentiles {
            Some(p) => Some(serde_json::to_string(p)?),
            None => None,
        };
        let element_shape_json = serde_json::to_string(&meta.element_shape)?;

        // `prepare_cached` so bulk adds parse each INSERT's SQL once per
        // connection instead of once per row.
        let mut insert_stmt = tx.prepare_cached(
            "INSERT INTO time_series_associations
             (owner_id, owner_type, owner_category, time_series_type, name, data_hash,
              initial_timestamp, resolution, length, horizon, interval, count,
              timestamps_json, units, percentiles_json,
              dtype, element_shape, logical_type, features_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19)",
        )?;
        let result = insert_stmt.execute(params![
            meta.owner_id,
            meta.owner_type,
            meta.owner_category.as_str(),
            meta.time_series_type.as_str(),
            meta.name,
            meta.data_hash.as_slice(),
            initial_ts,
            resolution_iso,
            meta.length.map(|l| l as i64),
            horizon_iso,
            interval_iso,
            meta.count.map(|c| c as i64),
            timestamps_json,
            meta.units,
            percentiles_json,
            meta.dtype.as_str(),
            element_shape_json,
            meta.logical_type,
            f_hash.as_slice(),
        ]);

        let id = match result {
            Ok(_) => tx.last_insert_rowid(),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                // The unique index covers (owner_id, owner_category,
                // time_series_type, name, resolution, interval, features_hash).
                // Surface the spec error.
                return Err(TimeSeriesError::DuplicateTimeSeries);
            }
            Err(e) => return Err(e.into()),
        };

        // `insert` on the cache returns true the first time this batch sees the
        // set; every later row carrying it is a no-op we can skip outright.
        if cache.seen.insert(f_hash) {
            Self::insert_feature_set(tx, &f_hash, &meta.features)?;
        }

        Ok(id)
    }

    /// Record a feature set under its content hash, if it is not already stored.
    ///
    /// `OR IGNORE` makes this a no-op whenever some other association already
    /// wrote this exact set — which is the common case, and the whole point of
    /// content-addressing them: a derived `DeterministicSingleTimeSeries` shares
    /// its source's features, so it writes nothing here.
    ///
    /// Equal hash implies equal set (SHA-256 of the canonical encoding), so an
    /// ignored conflict cannot silently keep a *different* set under the same
    /// hash.
    fn insert_feature_set(
        tx: &Transaction<'_>,
        f_hash: &[u8; 32],
        features: &Features,
    ) -> Result<()> {
        if features.is_empty() {
            return Ok(());
        }
        let mut feature_stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO feature_sets
             (features_hash, key, value_kind, value_int, value_float, value_bool, value_str)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for (k, v) in features {
            let (kind, vi, vf, vb, vs): (
                &str,
                Option<i64>,
                Option<f64>,
                Option<i64>,
                Option<&str>,
            ) = match v {
                FeatureValue::Int(i) => ("int", Some(*i), None, None, None),
                FeatureValue::Float(f) => ("float", None, Some(*f), None, None),
                FeatureValue::Bool(b) => ("bool", None, None, Some(*b as i64), None),
                FeatureValue::Str(s) => ("str", None, None, None, Some(s.as_str())),
            };
            feature_stmt.execute(params![f_hash.as_slice(), k, kind, vi, vf, vb, vs])?;
        }
        Ok(())
    }

    /// Delete feature sets no association references any more, and return how
    /// many rows went. Deleting an association leaves its set behind (sets are
    /// shared, so deletion cannot cascade); this reclaims the ones that are now
    /// unreachable. Called from [`crate::Store::compact`].
    pub fn sweep_orphan_feature_sets(tx: &Transaction<'_>) -> Result<usize> {
        let n = tx.execute(
            "DELETE FROM feature_sets
             WHERE features_hash NOT IN
                   (SELECT DISTINCT features_hash FROM time_series_associations)",
            [],
        )?;
        Ok(n)
    }

    /// Delete an association by primary-key tuple. Returns the number of rows
    /// deleted (0 if no match) and the data_hashes of the removed rows so the
    /// caller can decide whether to drop the underlying array.
    ///
    /// A NULL `interval` in the query matches any interval (rather than only
    /// rows whose stored interval is NULL). Attribute-based removal does not
    /// thread an interval — a forecast's interval is derived from its data, not
    /// supplied by the caller — so a forecast (which always stores a non-null
    /// interval) would otherwise never match. `time_series_type`, `name`,
    /// `resolution`, and `features_hash` still pin the row down; to target a
    /// single interval among otherwise-identical rows, remove by full key
    /// identity (which carries the exact interval).
    pub fn delete_by_key(tx: &Transaction<'_>, key: &KeyIdentity) -> Result<Vec<[u8; 32]>> {
        let f_hash = features_hash(&key.features);
        let resolution_iso = key.resolution.map(period_to_iso);
        let interval_iso = key.interval.map(period_to_iso);
        let mut stmt = tx.prepare(
            "SELECT id, data_hash FROM time_series_associations
             WHERE owner_id = ?1 AND owner_category = ?2 AND time_series_type = ?3 AND name = ?4
               AND ((?5 IS NULL AND resolution IS NULL) OR resolution = ?5)
               AND (?6 IS NULL OR interval = ?6)
               AND features_hash = ?7",
        )?;
        let rows: Vec<(i64, Vec<u8>)> = stmt
            .query_map(
                params![
                    key.owner_id,
                    key.owner_category.as_str(),
                    key.time_series_type.as_str(),
                    key.name,
                    resolution_iso,
                    interval_iso,
                    f_hash.as_slice(),
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut out = Vec::with_capacity(rows.len());
        for (id, hash_bytes) in rows {
            tx.execute(
                "DELETE FROM time_series_associations WHERE id = ?1",
                params![id],
            )?;
            let mut h = [0u8; 32];
            if hash_bytes.len() == 32 {
                h.copy_from_slice(&hash_bytes);
                out.push(h);
            }
        }
        Ok(out)
    }

    /// Delete all associations for the owner `(owner_id, owner_category)`.
    /// Returns the data_hashes of removed rows.
    pub fn delete_by_owner(
        tx: &Transaction<'_>,
        owner_id: i64,
        owner_category: OwnerCategory,
    ) -> Result<Vec<[u8; 32]>> {
        let bytes_list: Vec<Vec<u8>> = collect_data_hashes(
            tx,
            "SELECT data_hash FROM time_series_associations
             WHERE owner_id = ?1 AND owner_category = ?2",
            params![owner_id, owner_category.as_str()],
        )?;
        let hashes = bytes_list
            .into_iter()
            .filter_map(|bytes| bytes_to_hash32(&bytes))
            .collect::<Vec<_>>();
        tx.execute(
            "DELETE FROM time_series_associations WHERE owner_id = ?1 AND owner_category = ?2",
            params![owner_id, owner_category.as_str()],
        )?;
        Ok(hashes)
    }

    /// Reassign every association from `old_owner` to `new_owner` within the
    /// given `owner_category`. Only the owning id changes; type/category and the
    /// underlying arrays are untouched (arrays are content-addressed). Returns
    /// the rows updated.
    pub fn replace_owner(
        tx: &Transaction<'_>,
        old_owner: i64,
        new_owner: i64,
        owner_category: OwnerCategory,
    ) -> Result<usize> {
        // A collision (the new owner already holds an identical association)
        // fires the unique index on the UPDATE; surface the spec error rather
        // than a raw rusqlite error (REVIEW_FOLLOWUPS.md item 5).
        tx.execute(
            "UPDATE time_series_associations SET owner_id = ?1
             WHERE owner_id = ?2 AND owner_category = ?3",
            params![new_owner, old_owner, owner_category.as_str()],
        )
        .map_err(map_unique_violation)
    }

    /// Rename one association identified by `key` to `new_name`, leaving its data
    /// and hash untouched. Returns the number of rows updated (0 if `key` matches
    /// nothing). A collision with an existing series of the new identity maps to
    /// [`TimeSeriesError::DuplicateTimeSeries`].
    pub fn rename(tx: &Transaction<'_>, key: &KeyIdentity, new_name: &str) -> Result<usize> {
        let f_hash = features_hash(&key.features);
        let resolution_iso = key.resolution.map(period_to_iso);
        let interval_iso = key.interval.map(period_to_iso);
        tx.execute(
            "UPDATE time_series_associations SET name = ?1
             WHERE owner_id = ?2 AND owner_category = ?3 AND time_series_type = ?4 AND name = ?5
               AND ((?6 IS NULL AND resolution IS NULL) OR resolution = ?6)
               AND (?7 IS NULL OR interval = ?7)
               AND features_hash = ?8",
            params![
                new_name,
                key.owner_id,
                key.owner_category.as_str(),
                key.time_series_type.as_str(),
                key.name,
                resolution_iso,
                interval_iso,
                f_hash.as_slice(),
            ],
        )
        .map_err(map_unique_violation)
    }

    /// Delete every association in the store. Returns the removed data_hashes.
    pub fn delete_all(tx: &Transaction<'_>) -> Result<Vec<[u8; 32]>> {
        let bytes_list: Vec<Vec<u8>> = collect_data_hashes(
            tx,
            "SELECT data_hash FROM time_series_associations",
            params![],
        )?;
        let hashes = bytes_list
            .into_iter()
            .filter_map(|bytes| bytes_to_hash32(&bytes))
            .collect::<Vec<_>>();
        tx.execute("DELETE FROM time_series_associations", [])?;
        // Clearing the store empties it, so every feature set is unreachable by
        // construction. Drop them here rather than leaving the whole catalog's
        // worth of orphans for a compaction that a cleared store may never get.
        tx.execute("DELETE FROM feature_sets", [])?;
        Ok(hashes)
    }

    pub fn list(&self, filter: &MetadataFilter) -> Result<Vec<TimeSeriesMetadata>> {
        let (where_clause, params_vec) = filter.to_sql();
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();

        let sql = format!(
            "SELECT features_hash, owner_id, owner_type, owner_category, time_series_type, name,
                    data_hash, initial_timestamp, resolution, length, horizon,
                    interval, count, timestamps_json, units, percentiles_json,
                    dtype, element_shape, logical_type
             FROM time_series_associations {where_clause}"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows: Vec<([u8; 32], MetaRow)> = stmt
            .query_map(param_refs.as_slice(), parse_meta_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Hydrate features in one query rather than one per row. Because feature
        // sets are content-addressed, this fetches each DISTINCT set once, no
        // matter how many matched rows share it — listing 50k series that all
        // carry the same two features reads two rows here, not 100k.
        //
        // Re-running the row predicate as a subquery (rather than binding an
        // `IN (...)` list of hashes) keeps this to two statements regardless of
        // match count, and sidesteps SQLite's bound-parameter ceiling on a large
        // store. Rows whose feature set is empty simply get no group.
        let feat_sql = format!(
            "SELECT fs.features_hash, fs.key, fs.value_kind, fs.value_int, fs.value_float,
                    fs.value_bool, fs.value_str
             FROM feature_sets fs
             WHERE fs.features_hash IN
                   (SELECT features_hash FROM time_series_associations {where_clause})"
        );
        let mut feat_stmt = self.conn.prepare_cached(&feat_sql)?;
        let mut by_hash: HashMap<[u8; 32], Features> = HashMap::new();
        let mut feat_rows = feat_stmt.query(param_refs.as_slice())?;
        while let Some(row) = feat_rows.next()? {
            let hash = bytes_to_hash32(&row.get::<_, Vec<u8>>(0)?).ok_or_else(|| {
                TimeSeriesError::IntegrityError("features_hash is not 32 bytes".into())
            })?;
            let (key, value) = parse_feature_row(row)?;
            // `Features` is a BTreeMap, so it orders keys itself; the query does
            // not need an ORDER BY.
            by_hash.entry(hash).or_default().insert(key, value);
        }

        let mut out = Vec::with_capacity(rows.len());
        for (f_hash, partial) in rows {
            // Cloned, not removed: many rows legitimately share one set.
            let features = by_hash.get(&f_hash).cloned().unwrap_or_default();
            // Optional features-subset filter, in-memory.
            if let Some(ref required) = filter.features
                && !is_subset(required, &features)
            {
                continue;
            }
            out.push(partial.into_metadata(features));
        }
        Ok(out)
    }

    /// The identity of every association of `ts_type` — paired with its stored
    /// `horizon` (`None` for non-forecast rows) — read straight from the
    /// associations table with no feature hydration.
    ///
    /// `features_hash` is a stored column, so a caller that only needs to test
    /// identity ("does this series already exist?") can skip both the features
    /// join and the SHA-256 recomputation that [`Self::list`] would do. The
    /// horizon rides along because it is *not* part of the identity: a caller
    /// deciding whether an existing row satisfies a request (e.g. the DST
    /// transform's idempotency check) must compare it separately.
    pub fn list_identities(
        &self,
        ts_type: TimeSeriesType,
    ) -> Result<Vec<(AssociationIdentity, Option<Period>)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT owner_id, owner_category, name, resolution, interval, features_hash, horizon
             FROM time_series_associations WHERE time_series_type = ?1",
        )?;
        let rows = stmt
            .query_map(params![ts_type.as_str()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(
                |(owner_id, cat, name, resolution, interval, hash_blob, horizon)| {
                    let identity = AssociationIdentity {
                        owner_id,
                        owner_category: OwnerCategory::parse(&cat).ok_or_else(|| {
                            TimeSeriesError::IntegrityError(format!(
                                "unknown owner_category: {cat}"
                            ))
                        })?,
                        name,
                        resolution,
                        interval,
                        features_hash: hash_blob.as_slice().try_into().map_err(|_| {
                            TimeSeriesError::IntegrityError("features_hash is not 32 bytes".into())
                        })?,
                    };
                    let horizon = horizon.map(|s| iso_to_period(&s)).transpose()?;
                    Ok((identity, horizon))
                },
            )
            .collect()
    }

    pub fn list_keys_for_owner(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
    ) -> Result<Vec<TimeSeriesKey>> {
        let metas = self.list(&MetadataFilter {
            owner_id: Some(owner_id),
            owner_category: Some(owner_category),
            ..Default::default()
        })?;
        metas.iter().map(TimeSeriesKey::from_metadata).collect()
    }

    pub fn get_by_key(&self, key: &KeyIdentity) -> Result<TimeSeriesMetadata> {
        let mut matches = self.list(&MetadataFilter {
            owner_id: Some(key.owner_id),
            owner_category: Some(key.owner_category),
            time_series_type: Some(key.time_series_type),
            name: Some(key.name.clone()),
            resolution: key.resolution,
            interval: key.interval,
            // Pinpoint the row via the unique index rather than an in-memory
            // subset scan; the exact `retain` below guards against the
            // (astronomically unlikely) hash collision.
            features: None,
            features_hash: Some(features_hash(&key.features)),
            owner_type: None,
            name_glob: None,
        })?;
        matches.retain(|m| m.features == key.features);
        match matches.len() {
            0 => Err(TimeSeriesError::NotFound),
            1 => Ok(matches.pop().unwrap()),
            n => Err(TimeSeriesError::IntegrityError(format!(
                "expected exactly one match for key, found {n}"
            ))),
        }
    }

    pub fn distinct_resolutions(&self, ts_type: Option<TimeSeriesType>) -> Result<Vec<Period>> {
        let mut sql = String::from(
            "SELECT DISTINCT resolution FROM time_series_associations
             WHERE resolution IS NOT NULL",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(t) = ts_type {
            sql.push_str(" AND time_series_type = ?");
            params_vec.push(Box::new(t.as_str().to_string()));
        }
        sql.push_str(" ORDER BY resolution ASC");
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(|s| iso_to_period(&s)).collect()
    }

    /// Distinct forecast `interval`s, optionally scoped to one time series type.
    /// Ordered by the ISO-8601 text (lexical, like [`Self::distinct_resolutions`]):
    /// mixed period kinds have no numeric order, so text order is the stable
    /// choice. Only forecast rows carry an interval, so non-forecast types yield
    /// an empty list.
    pub fn distinct_intervals(&self, ts_type: Option<TimeSeriesType>) -> Result<Vec<Period>> {
        let mut sql = String::from(
            "SELECT DISTINCT interval FROM time_series_associations
             WHERE interval IS NOT NULL",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(t) = ts_type {
            sql.push_str(" AND time_series_type = ?");
            params_vec.push(Box::new(t.as_str().to_string()));
        }
        sql.push_str(" ORDER BY interval ASC");
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(|s| iso_to_period(&s)).collect()
    }

    pub fn count_by_type(&self, ts_type: TimeSeriesType) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM time_series_associations WHERE time_series_type = ?1",
            params![ts_type.as_str()],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Count `SingleTimeSeries` and `DeterministicSingleTimeSeries` associations
    /// referencing `data_hash`, returned as `(sts, dst)`. Other types referencing
    /// the same array (if any) are ignored. One grouped query, no feature fetch.
    pub fn count_array_references(&self, data_hash: &[u8; 32]) -> Result<(i64, i64)> {
        let mut stmt = self.conn.prepare(
            "SELECT time_series_type, COUNT(*) FROM time_series_associations
             WHERE data_hash = ?1 GROUP BY time_series_type",
        )?;
        let rows = stmt.query_map(params![data_hash.as_slice()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut sts = 0i64;
        let mut dst = 0i64;
        for row in rows {
            let (ts_type, n) = row?;
            match TimeSeriesType::parse(&ts_type) {
                Some(TimeSeriesType::SingleTimeSeries) => sts = n,
                Some(TimeSeriesType::DeterministicSingleTimeSeries) => dst = n,
                _ => {}
            }
        }
        Ok((sts, dst))
    }

    pub fn count_distinct_owners(&self) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM
             (SELECT DISTINCT owner_id, owner_category FROM time_series_associations)",
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Association count grouped by time series type, as `(type, count)` pairs.
    /// One grouped query; types the core does not recognize are skipped.
    pub fn counts_by_type(&self) -> Result<Vec<(TimeSeriesType, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT time_series_type, COUNT(*) FROM time_series_associations
             GROUP BY time_series_type",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (ts_type, n) = row?;
            if let Some(ty) = TimeSeriesType::parse(&ts_type) {
                out.push((ty, n));
            }
        }
        Ok(out)
    }

    /// Number of distinct stored arrays (content hashes) referenced by any
    /// association. Series that share an array (de-duplicated by content) count
    /// once. One `COUNT(DISTINCT)` query.
    pub fn count_distinct_arrays(&self) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT data_hash) FROM time_series_associations",
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Number of distinct owner ids in `category` that have any association.
    pub fn count_distinct_owners_in_category(&self, category: OwnerCategory) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT owner_id) FROM time_series_associations
             WHERE owner_category = ?1",
            params![category.as_str()],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Number of distinct stored arrays referenced by associations of any of
    /// `types`. Empty `types` yields 0.
    pub fn count_distinct_arrays_for_types(&self, types: &[TimeSeriesType]) -> Result<i64> {
        if types.is_empty() {
            return Ok(0);
        }
        let placeholders = vec!["?"; types.len()].join(",");
        let sql = format!(
            "SELECT COUNT(DISTINCT data_hash) FROM time_series_associations
             WHERE time_series_type IN ({placeholders})"
        );
        let names: Vec<&str> = types.iter().map(|t| t.as_str()).collect();
        let n: i64 = self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(names), |r| r.get(0))?;
        Ok(n)
    }

    /// Grouped summary of the static series (SingleTimeSeries +
    /// NonSequentialTimeSeries): one row per distinct
    /// `(owner_type, owner_category, type, name, initial_timestamp, resolution,
    /// length)` with the association count. One `GROUP BY` query.
    pub fn static_summary(&self) -> Result<Vec<StaticSummaryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT owner_type, owner_category, time_series_type, name,
                    initial_timestamp, resolution, length, COUNT(*)
             FROM time_series_associations
             WHERE time_series_type IN ('SingleTimeSeries', 'NonSequentialTimeSeries')
             GROUP BY owner_type, owner_category, time_series_type, name,
                      initial_timestamp, resolution, length",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (owner_type, oc, tt, name, its, res, len, count) = row?;
            out.push(StaticSummaryRow {
                owner_type,
                owner_category: parse_category(&oc)?,
                time_series_type: parse_type(&tt)?,
                name,
                initial_timestamp: parse_opt_rfc3339(its)?,
                resolution: res.map(|s| iso_to_period(&s)).transpose()?,
                time_step_count: len,
                count,
            });
        }
        Ok(out)
    }

    /// Grouped summary of forecasts: one row per distinct
    /// `(owner_type, owner_category, type, name, initial_timestamp, resolution,
    /// horizon, interval, window_count)` with the association count. One
    /// `GROUP BY` query.
    pub fn forecast_summary(&self) -> Result<Vec<ForecastSummaryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT owner_type, owner_category, time_series_type, name,
                    initial_timestamp, resolution, horizon, interval, count, COUNT(*)
             FROM time_series_associations
             WHERE time_series_type IN
                   ('Deterministic', 'DeterministicSingleTimeSeries', 'Probabilistic', 'Scenarios')
             GROUP BY owner_type, owner_category, time_series_type, name,
                      initial_timestamp, resolution, horizon, interval, count",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, i64>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (owner_type, oc, tt, name, its, res, hor, iv, wcount, count) = row?;
            out.push(ForecastSummaryRow {
                owner_type,
                owner_category: parse_category(&oc)?,
                time_series_type: parse_type(&tt)?,
                name,
                initial_timestamp: parse_opt_rfc3339(its)?,
                resolution: res.map(|s| iso_to_period(&s)).transpose()?,
                horizon: hor.map(|s| iso_to_period(&s)).transpose()?,
                interval: iv.map(|s| iso_to_period(&s)).transpose()?,
                window_count: wcount,
                count,
            });
        }
        Ok(out)
    }

    /// Distinct `(resolution, initial_timestamp, length)` triples across the
    /// `SingleTimeSeries` associations, ordered by resolution (ISO-8601 text
    /// order, so equal resolutions are adjacent). Used to verify that each
    /// resolution's series share a single static grid; `resolution` optionally
    /// restricts the scan to one resolution. One `DISTINCT` query.
    pub fn distinct_single_grids(
        &self,
        resolution: Option<Period>,
    ) -> Result<Vec<(Period, DateTime<Utc>, i64)>> {
        let res_iso = resolution.map(period_to_iso);
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT resolution, initial_timestamp, length
             FROM time_series_associations
             WHERE time_series_type = ?1 AND (?2 IS NULL OR resolution = ?2)
             ORDER BY resolution",
        )?;
        let rows = stmt.query_map(
            params![TimeSeriesType::SingleTimeSeries.as_str(), res_iso],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (res_str, ts_str, len) = row?;
            let res = iso_to_period(&res_str)?;
            let ts = DateTime::parse_from_rfc3339(&ts_str)
                .map_err(|e| {
                    TimeSeriesError::IntegrityError(format!("bad initial_timestamp: {e}"))
                })?
                .with_timezone(&Utc);
            out.push((res, ts, len));
        }
        Ok(out)
    }

    /// Distinct owner ids in `category` that have an association, optionally
    /// restricted to one time series type and/or resolution.
    pub fn list_owner_ids(
        &self,
        category: OwnerCategory,
        ts_type: Option<TimeSeriesType>,
        resolution: Option<Period>,
    ) -> Result<Vec<i64>> {
        let ts_type_str = ts_type.map(|t| t.as_str());
        let res_iso = resolution.map(period_to_iso);
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT owner_id FROM time_series_associations
             WHERE owner_category = ?1
               AND (?2 IS NULL OR time_series_type = ?2)
               AND (?3 IS NULL OR resolution = ?3)",
        )?;
        let rows = stmt.query_map(params![category.as_str(), ts_type_str, res_iso], |r| {
            r.get::<_, i64>(0)
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/// Parse one `features` row into its key/value pair. The row must select
/// `key, value_kind, value_int, value_float, value_bool, value_str` starting at
/// the column after `association_id`.
fn parse_feature_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, FeatureValue)> {
    let key: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let value = match kind.as_str() {
        "int" => FeatureValue::Int(row.get::<_, i64>(3)?),
        "float" => FeatureValue::Float(row.get::<_, f64>(4)?),
        "bool" => FeatureValue::Bool(row.get::<_, i64>(5)? != 0),
        "str" => FeatureValue::Str(row.get::<_, String>(6)?),
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown feature kind: {kind}"),
                )),
            ));
        }
    };
    Ok((key, value))
}

/// Map a SQLite UNIQUE-index constraint violation to the spec's
/// [`TimeSeriesError::DuplicateTimeSeries`], passing every other error through.
/// Shared by the `INSERT` and `UPDATE` paths where the association uniqueness
/// index can fire (`rename`, `replace_owner`).
fn map_unique_violation(e: rusqlite::Error) -> TimeSeriesError {
    match e {
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            TimeSeriesError::DuplicateTimeSeries
        }
        other => other.into(),
    }
}

/// Canonical ISO-8601 encoding of a period for storage in the catalog.
fn period_to_iso(p: Period) -> String {
    p.to_iso8601()
}

/// Parse a period from its catalog ISO-8601 encoding. A parse failure is an
/// integrity error: the value was written by [`period_to_iso`].
fn iso_to_period(s: &str) -> Result<Period> {
    Period::from_iso8601(s)
        .map_err(|e| TimeSeriesError::IntegrityError(format!("bad period '{s}' in catalog: {e}")))
}

fn is_subset(required: &Features, actual: &Features) -> bool {
    required
        .iter()
        .all(|(k, v)| actual.get(k).is_some_and(|a| a == v))
}

fn bytes_to_hash32(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.len() == 32 {
        let mut h = [0u8; 32];
        h.copy_from_slice(bytes);
        Some(h)
    } else {
        None
    }
}

/// Helper to run a `SELECT data_hash` query and collect raw bytes, isolating
/// the prepared statement's lifetime so the caller's tx isn't borrowed.
fn collect_data_hashes(
    tx: &Transaction<'_>,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Vec<u8>>> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt
        .query_map(params, |row| row.get::<_, Vec<u8>>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

struct MetaRow {
    owner_id: i64,
    owner_type: String,
    owner_category: OwnerCategory,
    time_series_type: TimeSeriesType,
    name: String,
    data_hash: [u8; 32],
    initial_timestamp: Option<DateTime<Utc>>,
    resolution: Option<Period>,
    length: Option<usize>,
    horizon: Option<Period>,
    interval: Option<Period>,
    count: Option<usize>,
    timestamps: Option<Vec<DateTime<Utc>>>,
    units: Option<String>,
    percentiles: Option<Vec<f64>>,
    dtype: crate::types::array::Dtype,
    element_shape: Vec<usize>,
    logical_type: Option<String>,
}

impl MetaRow {
    fn into_metadata(self, features: Features) -> TimeSeriesMetadata {
        TimeSeriesMetadata {
            owner_id: self.owner_id,
            owner_type: self.owner_type,
            owner_category: self.owner_category,
            time_series_type: self.time_series_type,
            name: self.name,
            data_hash: self.data_hash,
            initial_timestamp: self.initial_timestamp,
            resolution: self.resolution,
            length: self.length,
            horizon: self.horizon,
            interval: self.interval,
            count: self.count,
            timestamps: self.timestamps,
            features,
            units: self.units,
            percentiles: self.percentiles,
            dtype: self.dtype,
            element_shape: self.element_shape,
            logical_type: self.logical_type,
        }
    }
}

/// Parse one association row. Column 0 is the `features_hash`, which is how the
/// caller looks the row's feature set up in the content-addressed `feature_sets`
/// table.
fn parse_meta_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<([u8; 32], MetaRow)> {
    let features_hash: Vec<u8> = row.get(0)?;
    let owner_id: i64 = row.get(1)?;
    let owner_type: String = row.get(2)?;
    let owner_category: String = row.get(3)?;
    let time_series_type: String = row.get(4)?;
    let name: String = row.get(5)?;
    let data_hash_bytes: Vec<u8> = row.get(6)?;
    let initial_timestamp: Option<String> = row.get(7)?;
    let resolution_iso: Option<String> = row.get(8)?;
    let length: Option<i64> = row.get(9)?;
    let horizon_iso: Option<String> = row.get(10)?;
    let interval_iso: Option<String> = row.get(11)?;
    let count: Option<i64> = row.get(12)?;
    let timestamps_json: Option<String> = row.get(13)?;
    let units: Option<String> = row.get(14)?;
    let percentiles_json: Option<String> = row.get(15)?;
    let dtype_str: String = row.get(16)?;
    let element_shape_json: Option<String> = row.get(17)?;
    let logical_type: Option<String> = row.get(18)?;

    let owner_category = OwnerCategory::parse(&owner_category).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid owner_category: {owner_category}"),
            )),
        )
    })?;
    let ts_type = TimeSeriesType::parse(&time_series_type).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid time_series_type: {time_series_type}"),
            )),
        )
    })?;
    let mut data_hash = [0u8; 32];
    if data_hash_bytes.len() != 32 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "data_hash must be 32 bytes",
            )),
        ));
    }
    data_hash.copy_from_slice(&data_hash_bytes);

    let initial_timestamp = initial_timestamp
        .map(|s| DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?;

    let timestamps = timestamps_json
        .map(|s| serde_json::from_str::<Vec<DateTime<Utc>>>(&s))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(13, rusqlite::types::Type::Text, Box::new(e))
        })?;

    let percentiles = percentiles_json
        .map(|s| serde_json::from_str::<Vec<f64>>(&s))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(15, rusqlite::types::Type::Text, Box::new(e))
        })?;

    let dtype = crate::types::array::Dtype::parse(&dtype_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            16,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid dtype: {dtype_str}"),
            )),
        )
    })?;
    let element_shape: Vec<usize> = element_shape_json
        .map(|s| serde_json::from_str::<Vec<usize>>(&s))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(17, rusqlite::types::Type::Text, Box::new(e))
        })?
        .unwrap_or_default();

    let parse_period = |col: usize, s: Option<String>| -> rusqlite::Result<Option<Period>> {
        s.map(|s| {
            Period::from_iso8601(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    col,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e.to_string(),
                    )),
                )
            })
        })
        .transpose()
    };
    let resolution = parse_period(8, resolution_iso)?;
    let horizon = parse_period(10, horizon_iso)?;
    let interval = parse_period(11, interval_iso)?;

    let features_hash = bytes_to_hash32(&features_hash).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "features_hash must be 32 bytes",
            )),
        )
    })?;

    Ok((
        features_hash,
        MetaRow {
            owner_id,
            owner_type,
            owner_category,
            time_series_type: ts_type,
            name,
            data_hash,
            initial_timestamp,
            resolution,
            length: length.map(|l| l as usize),
            horizon,
            interval,
            count: count.map(|c| c as usize),
            timestamps,
            units,
            percentiles,
            dtype,
            element_shape,
            logical_type,
        },
    ))
}

// Allow Connection-level lookups through a transaction for reads (used by the
// `Store` layer where a tx is already in-flight for atomicity). Implemented as
// helper free fns so we don't have two parallel Send/Sync wrappers.
pub fn references_to_in_tx(tx: &Transaction<'_>, data_hash: &[u8; 32]) -> Result<i64> {
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM time_series_associations WHERE data_hash = ?1",
        params![data_hash.as_slice()],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Does an association of `conflicting_type` already exist sharing the
/// abstract-deterministic family identity `(owner_id, owner_category, name,
/// resolution, features)`, *ignoring* interval and the requesting type?
///
/// `Deterministic` and `DeterministicSingleTimeSeries` are mutually exclusive
/// for one family: the latter is a synthetic view of a `SingleTimeSeries`, so a
/// caller should never hold both. The catalog's unique index keys on
/// `time_series_type` and so cannot enforce this; the add and transform paths
/// call this inside their transaction to reject the overlap. The match is by
/// `features_hash` (a SHA-256 collision is the only false positive), which is
/// sufficient for a guard.
pub fn forecast_family_conflict(
    tx: &Transaction<'_>,
    owner_id: i64,
    owner_category: OwnerCategory,
    name: &str,
    resolution: Option<Period>,
    features_hash: &[u8; 32],
    conflicting_type: TimeSeriesType,
) -> Result<bool> {
    let resolution_iso = resolution.map(period_to_iso);
    let exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM time_series_associations
             WHERE owner_id = ?1 AND owner_category = ?2 AND time_series_type = ?3 AND name = ?4
               AND ((?5 IS NULL AND resolution IS NULL) OR resolution = ?5)
               AND features_hash = ?6
             LIMIT 1",
            params![
                owner_id,
                owner_category.as_str(),
                conflicting_type.as_str(),
                name,
                resolution_iso,
                features_hash.as_slice(),
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}
