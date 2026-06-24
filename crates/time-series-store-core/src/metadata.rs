//! SQLite-backed metadata store.
//!
//! Stores [`TimeSeriesMetadata`] records and the (owner_id, type, name,
//! resolution, features) uniqueness invariant.

pub mod schema;

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, Transaction, params};

use crate::error::{Result, TimeSeriesError};
use crate::hash::features_hash;
use crate::types::key::{KeyIdentity, TimeSeriesKey};
use crate::types::metadata::{FeatureValue, Features, OwnerCategory, TimeSeriesMetadata};
use crate::types::time_series::TimeSeriesType;

pub struct MetadataStore {
    conn: Connection,
    read_only: bool,
}

#[derive(Debug, Default, Clone)]
pub struct MetadataFilter {
    pub owner_id: Option<i64>,
    pub owner_category: Option<OwnerCategory>,
    pub owner_type: Option<String>,
    pub time_series_type: Option<TimeSeriesType>,
    pub name: Option<String>,
    pub resolution: Option<Duration>,
    /// Subset match: rows must contain at least these key/value pairs.
    pub features: Option<Features>,
    /// Exact features-set match by precomputed hash. When set, this is pushed
    /// into the SQL WHERE so the `uq_assoc` unique index can pinpoint the row,
    /// avoiding a feature fetch+compare for siblings that share the other key
    /// columns. Distinct from `features` (an in-memory subset filter).
    pub features_hash: Option<[u8; 32]>,
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

    pub fn read_only(&self) -> bool {
        self.read_only
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
        let f_hash = features_hash(&meta.features);
        let initial_ts = meta.initial_timestamp.map(|t| t.to_rfc3339());
        let resolution_ms = meta.resolution.map(duration_to_ms);
        let horizon_ms = meta.horizon.map(duration_to_ms);
        let interval_ms = meta.interval.map(duration_to_ms);
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
              initial_timestamp, resolution_ms, length, horizon_ms, interval_ms, count,
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
            resolution_ms,
            meta.length.map(|l| l as i64),
            horizon_ms,
            interval_ms,
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
                // The unique index covers (owner_id, time_series_type, name,
                // resolution_ms, features_hash). Surface the spec error.
                return Err(TimeSeriesError::DuplicateTimeSeries);
            }
            Err(e) => return Err(e.into()),
        };

        let mut feature_stmt = tx.prepare_cached(
            "INSERT INTO features
             (association_id, key, value_kind, value_int, value_float, value_bool, value_str)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for (k, v) in &meta.features {
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
            feature_stmt.execute(params![id, k, kind, vi, vf, vb, vs])?;
        }

        Ok(id)
    }

    /// Delete an association by primary-key tuple. Returns the number of rows
    /// deleted (0 if no match) and the data_hashes of the removed rows so the
    /// caller can decide whether to drop the underlying array.
    pub fn delete_by_key(tx: &Transaction<'_>, key: &KeyIdentity) -> Result<Vec<[u8; 32]>> {
        let f_hash = features_hash(&key.features);
        let resolution_ms = key.resolution.map(duration_to_ms);
        let mut stmt = tx.prepare(
            "SELECT id, data_hash FROM time_series_associations
             WHERE owner_id = ?1 AND owner_category = ?2 AND time_series_type = ?3 AND name = ?4
               AND ((?5 IS NULL AND resolution_ms IS NULL) OR resolution_ms = ?5)
               AND features_hash = ?6",
        )?;
        let rows: Vec<(i64, Vec<u8>)> = stmt
            .query_map(
                params![
                    key.owner_id,
                    key.owner_category.as_str(),
                    key.time_series_type.as_str(),
                    key.name,
                    resolution_ms,
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
        let updated = tx.execute(
            "UPDATE time_series_associations SET owner_id = ?1
             WHERE owner_id = ?2 AND owner_category = ?3",
            params![new_owner, old_owner, owner_category.as_str()],
        )?;
        Ok(updated)
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
        Ok(hashes)
    }

    /// Count of distinct data_hashes that still have at least one association.
    /// Used by the store layer to decide whether removing an association also
    /// removes the underlying array.
    pub fn references_to(&self, data_hash: &[u8; 32]) -> Result<i64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM time_series_associations WHERE data_hash = ?1",
            params![data_hash.as_slice()],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub fn list(&self, filter: &MetadataFilter) -> Result<Vec<TimeSeriesMetadata>> {
        // Build a SELECT with the always-on columns; layer on optional WHERE
        // clauses using sqlite's parameter binding.
        let mut sql = String::from(
            "SELECT id, owner_id, owner_type, owner_category, time_series_type, name,
                    data_hash, initial_timestamp, resolution_ms, length, horizon_ms,
                    interval_ms, count, timestamps_json, units, percentiles_json,
                    dtype, element_shape, logical_type
             FROM time_series_associations WHERE 1=1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(owner_id) = filter.owner_id {
            sql.push_str(" AND owner_id = ?");
            params_vec.push(Box::new(owner_id));
        }
        if let Some(owner_category) = filter.owner_category {
            sql.push_str(" AND owner_category = ?");
            params_vec.push(Box::new(owner_category.as_str().to_string()));
        }
        if let Some(ref owner_type) = filter.owner_type {
            sql.push_str(" AND owner_type = ?");
            params_vec.push(Box::new(owner_type.clone()));
        }
        if let Some(ts_type) = filter.time_series_type {
            sql.push_str(" AND time_series_type = ?");
            params_vec.push(Box::new(ts_type.as_str().to_string()));
        }
        if let Some(ref name) = filter.name {
            sql.push_str(" AND name = ?");
            params_vec.push(Box::new(name.clone()));
        }
        if let Some(resolution) = filter.resolution {
            sql.push_str(" AND resolution_ms = ?");
            params_vec.push(Box::new(duration_to_ms(resolution)));
        }
        if let Some(ref f_hash) = filter.features_hash {
            sql.push_str(" AND features_hash = ?");
            params_vec.push(Box::new(f_hash.to_vec()));
        }

        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<(i64, MetaRow)> = stmt
            .query_map(param_refs.as_slice(), parse_meta_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Hydrate features for each candidate.
        let mut out = Vec::with_capacity(rows.len());
        for (id, partial) in rows {
            let features = self.fetch_features(id)?;
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
            // Pinpoint the row via the unique index rather than an in-memory
            // subset scan; the exact `retain` below guards against the
            // (astronomically unlikely) hash collision.
            features: None,
            features_hash: Some(features_hash(&key.features)),
            owner_type: None,
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

    pub fn distinct_resolutions(&self, ts_type: Option<TimeSeriesType>) -> Result<Vec<Duration>> {
        let mut sql = String::from(
            "SELECT DISTINCT resolution_ms FROM time_series_associations
             WHERE resolution_ms IS NOT NULL",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(t) = ts_type {
            sql.push_str(" AND time_series_type = ?");
            params_vec.push(Box::new(t.as_str().to_string()));
        }
        sql.push_str(" ORDER BY resolution_ms ASC");
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(ms_to_duration).collect())
    }

    pub fn count(&self) -> Result<i64> {
        let n: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM time_series_associations", [], |r| {
                    r.get(0)
                })?;
        Ok(n)
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

    pub fn has_match(&self, filter: &MetadataFilter) -> Result<bool> {
        Ok(!self.list(filter)?.is_empty())
    }

    fn fetch_features(&self, association_id: i64) -> Result<Features> {
        let mut stmt = self.conn.prepare(
            "SELECT key, value_kind, value_int, value_float, value_bool, value_str
             FROM features WHERE association_id = ?1 ORDER BY key",
        )?;
        let rows = stmt
            .query_map(params![association_id], |row| {
                let key: String = row.get(0)?;
                let kind: String = row.get(1)?;
                let value = match kind.as_str() {
                    "int" => FeatureValue::Int(row.get::<_, i64>(2)?),
                    "float" => FeatureValue::Float(row.get::<_, f64>(3)?),
                    "bool" => FeatureValue::Bool(row.get::<_, i64>(4)? != 0),
                    "str" => FeatureValue::Str(row.get::<_, String>(5)?),
                    _ => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("unknown feature kind: {kind}"),
                            )),
                        ));
                    }
                };
                Ok((key, value))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().collect())
    }
}

fn duration_to_ms(d: Duration) -> i64 {
    d.num_milliseconds()
}

fn ms_to_duration(ms: i64) -> Duration {
    Duration::milliseconds(ms)
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
    resolution: Option<Duration>,
    length: Option<usize>,
    horizon: Option<Duration>,
    interval: Option<Duration>,
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

fn parse_meta_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, MetaRow)> {
    let id: i64 = row.get(0)?;
    let owner_id: i64 = row.get(1)?;
    let owner_type: String = row.get(2)?;
    let owner_category: String = row.get(3)?;
    let time_series_type: String = row.get(4)?;
    let name: String = row.get(5)?;
    let data_hash_bytes: Vec<u8> = row.get(6)?;
    let initial_timestamp: Option<String> = row.get(7)?;
    let resolution_ms: Option<i64> = row.get(8)?;
    let length: Option<i64> = row.get(9)?;
    let horizon_ms: Option<i64> = row.get(10)?;
    let interval_ms: Option<i64> = row.get(11)?;
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

    Ok((
        id,
        MetaRow {
            owner_id,
            owner_type,
            owner_category,
            time_series_type: ts_type,
            name,
            data_hash,
            initial_timestamp,
            resolution: resolution_ms.map(ms_to_duration),
            length: length.map(|l| l as usize),
            horizon: horizon_ms.map(ms_to_duration),
            interval: interval_ms.map(ms_to_duration),
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
