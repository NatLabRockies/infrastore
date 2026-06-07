//! High-level `Store` composing the storage backend and metadata store.

use std::path::{Path, PathBuf};

use chrono::Duration;

use crate::error::{Result, TimeSeriesError};
use crate::hash::array_hash;
use crate::metadata::{references_to_in_tx, MetadataFilter, MetadataStore};
use crate::storage::{
    CompactionReport, IntegrityReport, MemoryBackend, NetCdfBackend, StorageBackend,
};
use crate::types::key::TimeSeriesKey;
use crate::types::metadata::{
    Features, OwnerCategory, TimeSeriesMetadata,
};
use crate::types::time_series::{SingleTimeSeries, TimeSeriesData, TimeSeriesType};

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub owner_uuid: Option<String>,
    pub owner_type: Option<String>,
    pub time_series_type: Option<TimeSeriesType>,
    pub name: Option<String>,
    pub resolution: Option<Duration>,
    pub features: Option<Features>,
}

impl ListFilter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn owner_uuid(mut self, uuid: impl Into<String>) -> Self {
        self.owner_uuid = Some(uuid.into());
        self
    }
    pub fn owner_type(mut self, t: impl Into<String>) -> Self {
        self.owner_type = Some(t.into());
        self
    }
    pub fn time_series_type(mut self, t: TimeSeriesType) -> Self {
        self.time_series_type = Some(t);
        self
    }
    pub fn name(mut self, n: impl Into<String>) -> Self {
        self.name = Some(n.into());
        self
    }
    pub fn resolution(mut self, r: Duration) -> Self {
        self.resolution = Some(r);
        self
    }
    pub fn features(mut self, f: Features) -> Self {
        self.features = Some(f);
        self
    }
}

impl From<ListFilter> for MetadataFilter {
    fn from(value: ListFilter) -> Self {
        MetadataFilter {
            owner_uuid: value.owner_uuid,
            owner_type: value.owner_type,
            time_series_type: value.time_series_type,
            name: value.name,
            resolution: value.resolution,
            features: value.features,
        }
    }
}

/// Single item in a bulk add.
#[derive(Debug, Clone)]
pub struct AddRequest {
    pub owner_uuid: String,
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub name: String,
    pub data: TimeSeriesData,
    pub features: Features,
    pub units: Option<String>,
    pub scaling_factor_multiplier: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct TimeSeriesCounts {
    pub components_with_time_series: i64,
    pub static_time_series: i64,
    pub forecasts: i64,
}

#[derive(Debug, Default, Clone)]
pub struct ForecastParameters {
    pub horizon: Option<Duration>,
    pub interval: Option<Duration>,
    pub count: Option<usize>,
    pub resolution: Option<Duration>,
}

pub struct Store {
    backend: Box<dyn StorageBackend>,
    metadata: MetadataStore,
    read_only: bool,
    /// Filesystem path for the NetCDF file (None if `in_memory`).
    #[allow(dead_code)]
    netcdf_path: Option<PathBuf>,
}

impl Store {
    /// Create a new store. With `in_memory=true`, no filesystem I/O occurs;
    /// otherwise a NetCDF4 file is created at `path` and a sidecar SQLite
    /// file at `<path>.sqlite` holds metadata.
    pub fn create(path: Option<&Path>, in_memory: bool) -> Result<Self> {
        if in_memory {
            return Ok(Self {
                backend: Box::new(MemoryBackend::new()),
                metadata: MetadataStore::open_in_memory()?,
                read_only: false,
                netcdf_path: None,
            });
        }
        let nc_path = path.ok_or_else(|| {
            TimeSeriesError::InvalidParameter(
                "path is required when in_memory=false".into(),
            )
        })?;
        let sqlite_path = sidecar_sqlite_path(nc_path);
        let metadata = MetadataStore::open_path(&sqlite_path, false)?;
        let backend = NetCdfBackend::create(nc_path)?;
        Ok(Self {
            backend: Box::new(backend),
            metadata,
            read_only: false,
            netcdf_path: Some(nc_path.to_path_buf()),
        })
    }

    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        let sqlite_path = sidecar_sqlite_path(path);
        let metadata = MetadataStore::open_path(&sqlite_path, read_only)?;
        // For v0, `read_only` only locks down the metadata side. The NetCDF
        // backend opens in append mode regardless; write attempts are rejected
        // earlier in the `Store::add_*` / `remove_*` path.
        let backend = NetCdfBackend::open(path)?;
        Ok(Self {
            backend: Box::new(backend),
            metadata,
            read_only,
            netcdf_path: Some(path.to_path_buf()),
        })
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Mirrors the spec's `add_time_series` signature; the public surface is
    /// intentionally wide here. Use [`AddRequest`] + [`Self::add_time_series_bulk`]
    /// for ergonomic call sites.
    #[allow(clippy::too_many_arguments)]
    pub fn add_time_series(
        &mut self,
        owner_uuid: &str,
        owner_type: &str,
        owner_category: OwnerCategory,
        name: &str,
        data: TimeSeriesData,
        features: Features,
        units: Option<String>,
        scaling_factor_multiplier: Option<String>,
    ) -> Result<TimeSeriesKey> {
        self.add_time_series_bulk(vec![AddRequest {
            owner_uuid: owner_uuid.to_string(),
            owner_type: owner_type.to_string(),
            owner_category,
            name: name.to_string(),
            data,
            features,
            units,
            scaling_factor_multiplier,
        }])
        .map(|mut keys| keys.remove(0))
    }

    /// Bulk insert. All-or-nothing: any error rolls back every association
    /// and array put performed in this call.
    pub fn add_time_series_bulk(
        &mut self,
        items: Vec<AddRequest>,
    ) -> Result<Vec<TimeSeriesKey>> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }

        // Stage backend writes so we can roll them back on metadata error.
        let mut staged_hashes: Vec<[u8; 32]> = Vec::with_capacity(items.len());
        let tx = self.metadata.transaction()?;
        let mut keys = Vec::with_capacity(items.len());

        for item in items.iter() {
            let TimeSeriesData::SingleTimeSeries(single) = &item.data;

            let hash = array_hash(&single.data);
            let resolution_seconds = single.resolution.num_seconds();
            // put_array is idempotent on hash — safe to call before tx commit.
            let already_present = self.backend.contains(&hash)?;
            self.backend
                .put_array(&hash, &single.data, single.length, resolution_seconds)?;
            if !already_present {
                staged_hashes.push(hash);
            }

            let meta = TimeSeriesMetadata {
                owner_uuid: item.owner_uuid.clone(),
                owner_type: item.owner_type.clone(),
                owner_category: item.owner_category,
                time_series_type: TimeSeriesType::SingleTimeSeries,
                name: item.name.clone(),
                data_hash: hash,
                initial_timestamp: Some(single.initial_timestamp),
                resolution: Some(single.resolution),
                length: Some(single.length),
                horizon: None,
                interval: None,
                count: None,
                timestamps: None,
                features: item.features.clone(),
                scaling_factor_multiplier: item.scaling_factor_multiplier.clone(),
                units: item.units.clone(),
            };

            match MetadataStore::insert(&tx, &meta) {
                Ok(_) => {
                    keys.push(TimeSeriesKey {
                        owner_uuid: item.owner_uuid.clone(),
                        time_series_type: TimeSeriesType::SingleTimeSeries,
                        name: item.name.clone(),
                        resolution: Some(single.resolution),
                        features: item.features.clone(),
                    });
                }
                Err(e) => {
                    // Rollback metadata via Drop; also undo any array puts we
                    // staged in this call so the store returns to its prior state.
                    drop(tx);
                    for staged in &staged_hashes {
                        let _ = self.backend.remove_array(staged);
                    }
                    return Err(e);
                }
            }
        }

        tx.commit()?;
        Ok(keys)
    }

    pub fn remove_time_series(&mut self, key: &TimeSeriesKey) -> Result<()> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.transaction()?;
        let removed_hashes = MetadataStore::delete_by_key(&tx, key)?;
        if removed_hashes.is_empty() {
            return Err(TimeSeriesError::NotFound);
        }
        // For each removed association, drop the underlying array iff no other
        // association still references it.
        let mut to_drop = Vec::new();
        for h in &removed_hashes {
            if references_to_in_tx(&tx, h)? == 0 {
                to_drop.push(*h);
            }
        }
        tx.commit()?;
        for h in to_drop {
            self.backend.remove_array(&h)?;
        }
        Ok(())
    }

    /// Remove every time series for `owner_uuid`. Returns the count removed.
    pub fn clear_time_series(&mut self, owner_uuid: Option<&str>) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.transaction()?;
        let removed = match owner_uuid {
            Some(uuid) => MetadataStore::delete_by_owner(&tx, uuid)?,
            None => MetadataStore::delete_all(&tx)?,
        };
        let count = removed.len();
        let mut to_drop = Vec::new();
        for h in &removed {
            if references_to_in_tx(&tx, h)? == 0 {
                to_drop.push(*h);
            }
        }
        tx.commit()?;
        for h in to_drop {
            self.backend.remove_array(&h)?;
        }
        Ok(count)
    }

    pub fn get_time_series(
        &self,
        key: &TimeSeriesKey,
        time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    ) -> Result<TimeSeriesData> {
        let meta = self.metadata.get_by_key(key)?;
        match meta.time_series_type {
            TimeSeriesType::SingleTimeSeries => {
                let initial = meta.initial_timestamp.ok_or_else(|| {
                    TimeSeriesError::IntegrityError(
                        "SingleTimeSeries missing initial_timestamp".into(),
                    )
                })?;
                let resolution = meta.resolution.ok_or_else(|| {
                    TimeSeriesError::IntegrityError(
                        "SingleTimeSeries missing resolution".into(),
                    )
                })?;
                let length = meta.length.ok_or_else(|| {
                    TimeSeriesError::IntegrityError("SingleTimeSeries missing length".into())
                })?;

                let (data, sliced_initial, sliced_length) = match time_range {
                    None => {
                        let data = self.backend.get_array(&meta.data_hash)?;
                        (data, initial, length)
                    }
                    Some((start, end)) => {
                        if end < start {
                            return Err(TimeSeriesError::InvalidParameter(
                                "end < start".into(),
                            ));
                        }
                        let resolution_ns = resolution.num_nanoseconds().ok_or_else(|| {
                            TimeSeriesError::InvalidParameter(
                                "resolution overflows i64 nanoseconds".into(),
                            )
                        })?;
                        let total_ns = (start - initial).num_nanoseconds().ok_or_else(|| {
                            TimeSeriesError::InvalidParameter(
                                "time range overflows i64 nanoseconds".into(),
                            )
                        })?;
                        let start_idx = (total_ns / resolution_ns).max(0) as usize;
                        let end_total_ns =
                            (end - initial).num_nanoseconds().ok_or_else(|| {
                                TimeSeriesError::InvalidParameter(
                                    "time range overflows i64 nanoseconds".into(),
                                )
                            })?;
                        let end_idx = ((end_total_ns + resolution_ns - 1) / resolution_ns)
                            .max(0) as usize;
                        let start_idx = start_idx.min(length);
                        let end_idx = end_idx.min(length).max(start_idx);
                        let data = self
                            .backend
                            .get_slice(&meta.data_hash, start_idx..end_idx)?;
                        let new_initial = initial
                            + Duration::nanoseconds(start_idx as i64 * resolution_ns);
                        (data, new_initial, end_idx - start_idx)
                    }
                };

                Ok(TimeSeriesData::SingleTimeSeries(SingleTimeSeries {
                    initial_timestamp: sliced_initial,
                    resolution,
                    length: sliced_length,
                    data,
                }))
            }
            other => Err(TimeSeriesError::InvalidParameter(format!(
                "time series type {} not supported in v0",
                other.as_str()
            ))),
        }
    }

    pub fn list_time_series(&self, filter: ListFilter) -> Result<Vec<TimeSeriesMetadata>> {
        self.metadata.list(&filter.into())
    }

    /// Look up the full metadata record for a key. Errors with `NotFound` if no
    /// association matches. Used by external bindings (e.g. the Julia
    /// `RustTimeSeriesStore`) to reconstruct a typed metadata object on read.
    pub fn get_metadata(&self, key: &TimeSeriesKey) -> Result<TimeSeriesMetadata> {
        self.metadata.get_by_key(key)
    }

    /// Fetch the full stored array for a content hash. The metadata-owning
    /// binding resolves a key to its `data_hash`, then calls this to read the
    /// underlying values.
    pub fn get_array_by_hash(
        &self,
        hash: &[u8; 32],
    ) -> Result<ndarray::ArrayD<f64>> {
        self.backend.get_array(hash)
    }

    pub fn get_time_series_keys(&self, owner_uuid: &str) -> Result<Vec<TimeSeriesKey>> {
        self.metadata.list_keys_for_owner(owner_uuid)
    }

    pub fn has_time_series(&self, key: &TimeSeriesKey) -> Result<bool> {
        match self.metadata.get_by_key(key) {
            Ok(_) => Ok(true),
            Err(TimeSeriesError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn get_resolutions(
        &self,
        time_series_type: Option<TimeSeriesType>,
    ) -> Result<Vec<Duration>> {
        self.metadata.distinct_resolutions(time_series_type)
    }

    pub fn get_forecast_parameters(&self) -> Result<ForecastParameters> {
        // No forecast types in v0 — always empty.
        Ok(ForecastParameters::default())
    }

    pub fn get_time_series_counts(&self) -> Result<TimeSeriesCounts> {
        Ok(TimeSeriesCounts {
            components_with_time_series: self.metadata.count_distinct_owners()?,
            static_time_series: self.metadata.count_by_type(TimeSeriesType::SingleTimeSeries)?,
            forecasts: 0,
        })
    }

    pub fn compact(&mut self) -> Result<CompactionReport> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        self.backend.compact()
    }

    pub fn verify_integrity(&self) -> Result<IntegrityReport> {
        self.backend.verify()
    }

    pub fn flush(&mut self) -> Result<()> {
        self.backend.flush()
    }
}

fn sidecar_sqlite_path(nc_path: &Path) -> PathBuf {
    let mut p = nc_path.to_path_buf();
    let new_name = match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => format!("{name}.sqlite"),
        None => "metadata.sqlite".to_string(),
    };
    p.set_file_name(new_name);
    p
}
