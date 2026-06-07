//! SQLite-backed metadata store.
//!
//! Stores [`TimeSeriesMetadata`] records and the (owner_uuid, type, name,
//! resolution, features) uniqueness invariant.

pub mod schema;

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, Transaction, params};

use crate::error::{Result, TimeSeriesError};
use crate::hash::features_hash;
use crate::types::key::TimeSeriesKey;
use crate::types::metadata::{FeatureValue, Features, OwnerCategory, TimeSeriesMetadata};
use crate::types::time_series::TimeSeriesType;

pub struct MetadataStore {
    conn: Connection,
    read_only: bool,
}

#[derive(Debug, Default, Clone)]
pub struct MetadataFilter {
    pub owner_uuid: Option<String>,
    pub owner_type: Option<String>,
    pub time_series_type: Option<TimeSeriesType>,
    pub name: Option<String>,
    pub resolution: Option<Duration>,
    /// Subset match: rows must contain at least these key/value pairs.
    pub features: Option<Features>,
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
        let resolution_ns = meta.resolution.map(duration_to_ns);
        let horizon_ns = meta.horizon.map(duration_to_ns);
        let interval_ns = meta.interval.map(duration_to_ns);
        let timestamps_json = match &meta.timestamps {
            Some(ts) => Some(serde_json::to_string(ts)?),
            None => None,
        };
        let percentiles_json = match &meta.percentiles {
            Some(p) => Some(serde_json::to_string(p)?),
            None => None,
        };
        let element_shape_json = serde_json::to_string(&meta.element_shape)?;

        let result = tx.execute(
            "INSERT INTO time_series_associations
             (owner_uuid, owner_type, owner_category, time_series_type, name, data_hash,
              initial_timestamp, resolution_ns, length, horizon_ns, interval_ns, count,
              timestamps_json, scaling_factor, units, percentiles_json,
              dtype, element_shape, logical_type, features_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20)",
            params![
                meta.owner_uuid,
                meta.owner_type,
                meta.owner_category.as_str(),
                meta.time_series_type.as_str(),
                meta.name,
                meta.data_hash.as_slice(),
                initial_ts,
                resolution_ns,
                meta.length.map(|l| l as i64),
                horizon_ns,
                interval_ns,
                meta.count.map(|c| c as i64),
                timestamps_json,
                meta.scaling_factor_multiplier,
                meta.units,
                percentiles_json,
                meta.dtype.as_str(),
                element_shape_json,
                meta.logical_type,
                f_hash.as_slice(),
            ],
        );

        let id = match result {
            Ok(_) => tx.last_insert_rowid(),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                // The unique index covers (owner_uuid, time_series_type, name,
                // resolution_ns, features_hash). Surface the spec error.
                return Err(TimeSeriesError::DuplicateTimeSeries);
            }
            Err(e) => return Err(e.into()),
        };

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
            tx.execute(
                "INSERT INTO features
                 (association_id, key, value_kind, value_int, value_float, value_bool, value_str)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id, k, kind, vi, vf, vb, vs],
            )?;
        }

        Ok(id)
    }

    /// Delete an association by primary-key tuple. Returns the number of rows
    /// deleted (0 if no match) and the data_hashes of the removed rows so the
    /// caller can decide whether to drop the underlying array.
    pub fn delete_by_key(tx: &Transaction<'_>, key: &TimeSeriesKey) -> Result<Vec<[u8; 32]>> {
        let f_hash = features_hash(&key.features);
        let resolution_ns = key.resolution.map(duration_to_ns);
        let mut stmt = tx.prepare(
            "SELECT id, data_hash FROM time_series_associations
             WHERE owner_uuid = ?1 AND time_series_type = ?2 AND name = ?3
               AND ((?4 IS NULL AND resolution_ns IS NULL) OR resolution_ns = ?4)
               AND features_hash = ?5",
        )?;
        let rows: Vec<(i64, Vec<u8>)> = stmt
            .query_map(
                params![
                    key.owner_uuid,
                    key.time_series_type.as_str(),
                    key.name,
                    resolution_ns,
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

    /// Delete all associations for `owner_uuid`. Returns the data_hashes of removed rows.
    pub fn delete_by_owner(tx: &Transaction<'_>, owner_uuid: &str) -> Result<Vec<[u8; 32]>> {
        let bytes_list: Vec<Vec<u8>> = collect_data_hashes(
            tx,
            "SELECT data_hash FROM time_series_associations WHERE owner_uuid = ?1",
            params![owner_uuid],
        )?;
        let hashes = bytes_list
            .into_iter()
            .filter_map(|bytes| bytes_to_hash32(&bytes))
            .collect::<Vec<_>>();
        tx.execute(
            "DELETE FROM time_series_associations WHERE owner_uuid = ?1",
            params![owner_uuid],
        )?;
        Ok(hashes)
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
            "SELECT id, owner_uuid, owner_type, owner_category, time_series_type, name,
                    data_hash, initial_timestamp, resolution_ns, length, horizon_ns,
                    interval_ns, count, timestamps_json, scaling_factor, units, percentiles_json,
                    dtype, element_shape, logical_type
             FROM time_series_associations WHERE 1=1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(ref owner_uuid) = filter.owner_uuid {
            sql.push_str(" AND owner_uuid = ?");
            params_vec.push(Box::new(owner_uuid.clone()));
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
            sql.push_str(" AND resolution_ns = ?");
            params_vec.push(Box::new(duration_to_ns(resolution)));
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

    pub fn list_keys_for_owner(&self, owner_uuid: &str) -> Result<Vec<TimeSeriesKey>> {
        let metas = self.list(&MetadataFilter {
            owner_uuid: Some(owner_uuid.to_string()),
            ..Default::default()
        })?;
        Ok(metas
            .into_iter()
            .map(|m| TimeSeriesKey {
                owner_uuid: m.owner_uuid,
                time_series_type: m.time_series_type,
                name: m.name,
                resolution: m.resolution,
                features: m.features,
            })
            .collect())
    }

    pub fn get_by_key(&self, key: &TimeSeriesKey) -> Result<TimeSeriesMetadata> {
        let mut matches = self.list(&MetadataFilter {
            owner_uuid: Some(key.owner_uuid.clone()),
            time_series_type: Some(key.time_series_type),
            name: Some(key.name.clone()),
            resolution: key.resolution,
            features: Some(key.features.clone()),
            owner_type: None,
        })?;
        // Features-subset filter is permissive (superset OK); narrow to exact.
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
            "SELECT DISTINCT resolution_ns FROM time_series_associations
             WHERE resolution_ns IS NOT NULL",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(t) = ts_type {
            sql.push_str(" AND time_series_type = ?");
            params_vec.push(Box::new(t.as_str().to_string()));
        }
        sql.push_str(" ORDER BY resolution_ns ASC");
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().map(ns_to_duration).collect())
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

    pub fn count_distinct_owners(&self) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT owner_uuid) FROM time_series_associations",
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

fn duration_to_ns(d: Duration) -> i64 {
    d.num_nanoseconds()
        .unwrap_or_else(|| d.num_seconds() * 1_000_000_000)
}

fn ns_to_duration(ns: i64) -> Duration {
    Duration::nanoseconds(ns)
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
    owner_uuid: String,
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
    scaling_factor: Option<String>,
    units: Option<String>,
    percentiles: Option<Vec<f64>>,
    dtype: crate::types::array::Dtype,
    element_shape: Vec<usize>,
    logical_type: Option<String>,
}

impl MetaRow {
    fn into_metadata(self, features: Features) -> TimeSeriesMetadata {
        TimeSeriesMetadata {
            owner_uuid: self.owner_uuid,
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
            scaling_factor_multiplier: self.scaling_factor,
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
    let owner_uuid: String = row.get(1)?;
    let owner_type: String = row.get(2)?;
    let owner_category: String = row.get(3)?;
    let time_series_type: String = row.get(4)?;
    let name: String = row.get(5)?;
    let data_hash_bytes: Vec<u8> = row.get(6)?;
    let initial_timestamp: Option<String> = row.get(7)?;
    let resolution_ns: Option<i64> = row.get(8)?;
    let length: Option<i64> = row.get(9)?;
    let horizon_ns: Option<i64> = row.get(10)?;
    let interval_ns: Option<i64> = row.get(11)?;
    let count: Option<i64> = row.get(12)?;
    let timestamps_json: Option<String> = row.get(13)?;
    let scaling_factor: Option<String> = row.get(14)?;
    let units: Option<String> = row.get(15)?;
    let percentiles_json: Option<String> = row.get(16)?;
    let dtype_str: String = row.get(17)?;
    let element_shape_json: Option<String> = row.get(18)?;
    let logical_type: Option<String> = row.get(19)?;

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
            rusqlite::Error::FromSqlConversionFailure(16, rusqlite::types::Type::Text, Box::new(e))
        })?;

    let dtype = crate::types::array::Dtype::parse(&dtype_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            17,
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
            rusqlite::Error::FromSqlConversionFailure(18, rusqlite::types::Type::Text, Box::new(e))
        })?
        .unwrap_or_default();

    Ok((
        id,
        MetaRow {
            owner_uuid,
            owner_type,
            owner_category,
            time_series_type: ts_type,
            name,
            data_hash,
            initial_timestamp,
            resolution: resolution_ns.map(ns_to_duration),
            length: length.map(|l| l as usize),
            horizon: horizon_ns.map(ns_to_duration),
            interval: interval_ns.map(ns_to_duration),
            count: count.map(|c| c as usize),
            timestamps,
            scaling_factor,
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
