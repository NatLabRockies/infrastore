//! High-level `Store` composing the storage backend and metadata store.

use std::path::{Path, PathBuf};

use chrono::Duration;

use crate::error::{Result, TimeSeriesError};
use crate::hash::array_hash;
use crate::metadata::{MetadataFilter, MetadataStore, references_to_in_tx};
use crate::storage::{
    CompactionReport, Compression, IntegrityReport, MemoryBackend, NetCdfBackend, StorageBackend,
};
use crate::types::array::TypedArray;
use crate::types::key::TimeSeriesKey;
use crate::types::metadata::{Features, OwnerCategory, TimeSeriesMetadata};
use crate::types::time_series::{
    Deterministic, NonSequentialTimeSeries, Probabilistic, Scenarios, SingleTimeSeries,
    TimeSeriesData, TimeSeriesType, compute_h,
};

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
            features_hash: None,
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
    /// Opaque logical-type label for domain reconstruction (binding-owned).
    pub logical_type: Option<String>,
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
    /// otherwise a NetCDF4 file is created at `path` and a catalog SQLite
    /// file at `<path>.sqlite` holds metadata.
    ///
    /// Uses the default compression policy ([`Compression::default`]). Use
    /// [`Self::create_with_compression`] to choose a different filter.
    pub fn create(path: Option<&Path>, in_memory: bool) -> Result<Self> {
        Self::create_with_compression(path, in_memory, Compression::default())
    }

    /// Like [`Self::create`], but applies `compression` to NetCDF data
    /// variables. The setting is persisted with the store so later appends
    /// reuse it. It is ignored for `in_memory` stores, which never touch disk.
    pub fn create_with_compression(
        path: Option<&Path>,
        in_memory: bool,
        compression: Compression,
    ) -> Result<Self> {
        compression.validate()?;
        if in_memory {
            return Ok(Self {
                backend: Box::new(MemoryBackend::new()),
                metadata: MetadataStore::open_in_memory()?,
                read_only: false,
                netcdf_path: None,
            });
        }
        let nc_path = path.ok_or_else(|| {
            TimeSeriesError::InvalidParameter("path is required when in_memory=false".into())
        })?;
        let sqlite_path = catalog_sqlite_path(nc_path);
        let metadata = MetadataStore::open_path(&sqlite_path, false)?;
        let backend = NetCdfBackend::create(nc_path, compression)?;
        Ok(Self {
            backend: Box::new(backend),
            metadata,
            read_only: false,
            netcdf_path: Some(nc_path.to_path_buf()),
        })
    }

    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        let sqlite_path = catalog_sqlite_path(path);
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

    /// The compression policy applied to newly written arrays. For a store
    /// opened from disk this reflects the policy persisted at creation (restored
    /// from the file); in-memory stores report [`Compression::None`].
    pub fn compression(&self) -> Compression {
        self.backend.compression()
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
            logical_type: None,
        }])
        .map(|mut keys| keys.remove(0))
    }

    /// Bulk insert. All-or-nothing: any error rolls back every association
    /// and array put performed in this call.
    #[tracing::instrument(skip(self, items), fields(count = items.len()))]
    pub fn add_time_series_bulk(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesKey>> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }

        // Stage backend writes so we can roll them back on metadata error.
        let mut staged_hashes: Vec<[u8; 32]> = Vec::with_capacity(items.len());
        let tx = self.metadata.transaction()?;
        let mut keys = Vec::with_capacity(items.len());

        for item in &items {
            let (hash, resolution_ms, packed, meta, key) = match &item.data {
                TimeSeriesData::SingleTimeSeries(single) => {
                    let hash = array_hash(&single.data);
                    (
                        hash,
                        single.resolution.num_milliseconds(),
                        true,
                        TimeSeriesMetadata {
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
                            percentiles: None,
                            dtype: single.data.dtype,
                            element_shape: single.data.element_shape().to_vec(),
                            logical_type: item.logical_type.clone(),
                        },
                        TimeSeriesKey {
                            owner_uuid: item.owner_uuid.clone(),
                            time_series_type: TimeSeriesType::SingleTimeSeries,
                            name: item.name.clone(),
                            resolution: Some(single.resolution),
                            features: item.features.clone(),
                        },
                    )
                }
                TimeSeriesData::NonSequentialTimeSeries(non_sequential) => {
                    validate_non_sequential(non_sequential)?;
                    let hash = array_hash(&non_sequential.data);
                    (
                        hash,
                        0,
                        false,
                        TimeSeriesMetadata {
                            owner_uuid: item.owner_uuid.clone(),
                            owner_type: item.owner_type.clone(),
                            owner_category: item.owner_category,
                            time_series_type: TimeSeriesType::NonSequentialTimeSeries,
                            name: item.name.clone(),
                            data_hash: hash,
                            initial_timestamp: None,
                            resolution: None,
                            length: Some(non_sequential.length),
                            horizon: None,
                            interval: None,
                            count: None,
                            timestamps: Some(non_sequential.timestamps.clone()),
                            features: item.features.clone(),
                            scaling_factor_multiplier: item.scaling_factor_multiplier.clone(),
                            units: item.units.clone(),
                            percentiles: None,
                            dtype: non_sequential.data.dtype,
                            element_shape: non_sequential.data.element_shape().to_vec(),
                            logical_type: item.logical_type.clone(),
                        },
                        TimeSeriesKey {
                            owner_uuid: item.owner_uuid.clone(),
                            time_series_type: TimeSeriesType::NonSequentialTimeSeries,
                            name: item.name.clone(),
                            resolution: None,
                            features: item.features.clone(),
                        },
                    )
                }
                // Dense forecast types are stored as standalone arrays in their
                // native shape. `DeterministicSingleTimeSeries` is not added
                // directly; it is derived from a stored `SingleTimeSeries` via
                // [`Self::transform_single_time_series`].
                TimeSeriesData::Deterministic(det) => (
                    array_hash(&det.data),
                    det.resolution.num_milliseconds(),
                    false,
                    forecast_metadata(
                        item,
                        TimeSeriesType::Deterministic,
                        det.initial_timestamp,
                        det.resolution,
                        det.horizon,
                        det.interval,
                        det.count,
                        &det.data,
                        None,
                    ),
                    forecast_key(item, TimeSeriesType::Deterministic, det.resolution),
                ),
                TimeSeriesData::Probabilistic(prob) => (
                    array_hash(&prob.data),
                    prob.resolution.num_milliseconds(),
                    false,
                    forecast_metadata(
                        item,
                        TimeSeriesType::Probabilistic,
                        prob.initial_timestamp,
                        prob.resolution,
                        prob.horizon,
                        prob.interval,
                        prob.count,
                        &prob.data,
                        Some(prob.percentiles.clone()),
                    ),
                    forecast_key(item, TimeSeriesType::Probabilistic, prob.resolution),
                ),
                TimeSeriesData::Scenarios(scen) => (
                    array_hash(&scen.data),
                    scen.resolution.num_milliseconds(),
                    false,
                    forecast_metadata(
                        item,
                        TimeSeriesType::Scenarios,
                        scen.initial_timestamp,
                        scen.resolution,
                        scen.horizon,
                        scen.interval,
                        scen.count,
                        &scen.data,
                        None,
                    ),
                    forecast_key(item, TimeSeriesType::Scenarios, scen.resolution),
                ),
            };
            let data = match &item.data {
                TimeSeriesData::SingleTimeSeries(single) => &single.data,
                TimeSeriesData::NonSequentialTimeSeries(non_sequential) => &non_sequential.data,
                TimeSeriesData::Deterministic(det) => &det.data,
                TimeSeriesData::Probabilistic(prob) => &prob.data,
                TimeSeriesData::Scenarios(scen) => &scen.data,
            };

            let already_present = self.backend.contains(&hash)?;
            tracing::debug!(
                owner = %item.owner_uuid,
                bytes = data.bytes.len(),
                packed,
                already_present,
                "backend put_array",
            );
            self.backend.put_array(&hash, data, resolution_ms, packed)?;
            if !already_present {
                staged_hashes.push(hash);
            }

            match MetadataStore::insert(&tx, &meta) {
                Ok(_) => {
                    keys.push(key);
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
        tracing::debug!(count = keys.len(), "transaction committed");
        Ok(keys)
    }

    #[tracing::instrument(skip(self, key), fields(owner = %key.owner_uuid, name = %key.name))]
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

    /// Reassign every time series owned by `old_owner` to `new_owner`. The
    /// underlying arrays are content-addressed and shared, so only the
    /// association rows change. Returns the number of associations updated.
    pub fn replace_owner(&mut self, old_owner: &str, new_owner: &str) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.transaction()?;
        let updated = MetadataStore::replace_owner(&tx, old_owner, new_owner)?;
        tx.commit()?;
        Ok(updated)
    }

    #[tracing::instrument(skip(self, key, time_range), fields(owner = %key.owner_uuid, name = %key.name, has_time_range = time_range.is_some()))]
    pub fn get_time_series(
        &self,
        key: &TimeSeriesKey,
        time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    ) -> Result<TimeSeriesData> {
        let meta = self.metadata.get_by_key(key)?;
        tracing::debug!(ts_type = ?meta.time_series_type, "metadata loaded");
        match meta.time_series_type {
            TimeSeriesType::SingleTimeSeries => {
                let initial = meta.initial_timestamp.ok_or_else(|| {
                    TimeSeriesError::IntegrityError(
                        "SingleTimeSeries missing initial_timestamp".into(),
                    )
                })?;
                let resolution = meta.resolution.ok_or_else(|| {
                    TimeSeriesError::IntegrityError("SingleTimeSeries missing resolution".into())
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
                            return Err(TimeSeriesError::InvalidParameter("end < start".into()));
                        }
                        let resolution_ms = resolution.num_milliseconds();
                        if resolution_ms <= 0 {
                            return Err(TimeSeriesError::InvalidParameter(
                                "resolution must be positive".into(),
                            ));
                        }
                        let total_ms = (start - initial).num_milliseconds();
                        let start_idx = (total_ms / resolution_ms).max(0) as usize;
                        let end_total_ms = (end - initial).num_milliseconds();
                        // Ceiling division of a non-negative numerator. Written as
                        // `(n - 1) / d + 1` rather than `(n + d - 1) / d` so a
                        // far-future `end` (where `end_total_ms` is near `i64::MAX`)
                        // cannot overflow the addition. Non-positive numerators map
                        // to index 0.
                        let end_idx = if end_total_ms <= 0 {
                            0
                        } else {
                            ((end_total_ms - 1) / resolution_ms + 1) as usize
                        };
                        let start_idx = start_idx.min(length);
                        let end_idx = end_idx.min(length).max(start_idx);
                        let data = self
                            .backend
                            .get_slice(&meta.data_hash, start_idx..end_idx)?;
                        let new_initial =
                            initial + Duration::milliseconds(start_idx as i64 * resolution_ms);
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
            TimeSeriesType::NonSequentialTimeSeries => {
                let timestamps = meta.timestamps.ok_or_else(|| {
                    TimeSeriesError::IntegrityError(
                        "NonSequentialTimeSeries missing timestamps".into(),
                    )
                })?;
                let length = meta.length.ok_or_else(|| {
                    TimeSeriesError::IntegrityError("NonSequentialTimeSeries missing length".into())
                })?;
                if timestamps.len() != length {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "NonSequentialTimeSeries has {} timestamps but length {length}",
                        timestamps.len()
                    )));
                }

                let (data, timestamps) = match time_range {
                    None => (self.backend.get_array(&meta.data_hash)?, timestamps),
                    Some((start, end)) => {
                        if end < start {
                            return Err(TimeSeriesError::InvalidParameter("end < start".into()));
                        }
                        let start_idx = timestamps.partition_point(|t| *t < start);
                        let end_idx = timestamps.partition_point(|t| *t < end);
                        let data = self
                            .backend
                            .get_slice(&meta.data_hash, start_idx..end_idx)?;
                        (data, timestamps[start_idx..end_idx].to_vec())
                    }
                };
                let series = NonSequentialTimeSeries::new(timestamps, data)
                    .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::NonSequentialTimeSeries(series))
            }
            TimeSeriesType::Deterministic => {
                let arr = self.backend.get_array(&meta.data_hash)?;
                let initial = required_initial(&meta, "Deterministic")?;
                let resolution = required_resolution(&meta, "Deterministic")?;
                let horizon = required_horizon(&meta, "Deterministic")?;
                let interval = required_interval(&meta, "Deterministic")?;
                let count = required_count(&meta, "Deterministic")?;
                let h = compute_h(horizon, resolution).map_err(TimeSeriesError::IntegrityError)?;
                // Validate stored shape: [H, count, *E].
                validate_forecast_shape(&arr, &[h, count], "Deterministic")?;
                let (w0, w1, window_initial) =
                    resolve_windows(initial, resolution, horizon, interval, count, time_range)?;
                let windowed = if w0 == 0 && w1 == count {
                    arr
                } else {
                    slice_count_axis(&arr, 1, w0, w1)
                };
                let det = Deterministic::new(
                    window_initial,
                    resolution,
                    horizon,
                    interval,
                    w1 - w0,
                    windowed,
                )
                .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::Deterministic(det))
            }

            TimeSeriesType::Probabilistic => {
                let arr = self.backend.get_array(&meta.data_hash)?;
                let initial = required_initial(&meta, "Probabilistic")?;
                let resolution = required_resolution(&meta, "Probabilistic")?;
                let horizon = required_horizon(&meta, "Probabilistic")?;
                let interval = required_interval(&meta, "Probabilistic")?;
                let count = required_count(&meta, "Probabilistic")?;
                let percentiles = meta.percentiles.clone().ok_or_else(|| {
                    TimeSeriesError::IntegrityError("Probabilistic missing percentiles".into())
                })?;
                let h = compute_h(horizon, resolution).map_err(TimeSeriesError::IntegrityError)?;
                let p = percentiles.len();
                // Validate stored shape: [P, H, count, *E].
                validate_forecast_shape(&arr, &[p, h, count], "Probabilistic")?;
                let (w0, w1, window_initial) =
                    resolve_windows(initial, resolution, horizon, interval, count, time_range)?;
                let windowed = if w0 == 0 && w1 == count {
                    arr
                } else {
                    slice_count_axis(&arr, 2, w0, w1)
                };
                let prob = Probabilistic::new(
                    window_initial,
                    resolution,
                    horizon,
                    interval,
                    w1 - w0,
                    percentiles,
                    windowed,
                )
                .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::Probabilistic(prob))
            }

            TimeSeriesType::Scenarios => {
                let arr = self.backend.get_array(&meta.data_hash)?;
                let initial = required_initial(&meta, "Scenarios")?;
                let resolution = required_resolution(&meta, "Scenarios")?;
                let horizon = required_horizon(&meta, "Scenarios")?;
                let interval = required_interval(&meta, "Scenarios")?;
                let count = required_count(&meta, "Scenarios")?;
                let h = compute_h(horizon, resolution).map_err(TimeSeriesError::IntegrityError)?;
                // scenario_count = arr.shape[0]; validate remaining dims.
                if arr.shape.len() < 3 {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "Scenarios: stored shape {:?} must have at least 3 dims",
                        arr.shape
                    )));
                }
                let scenario_count = arr.shape[0];
                validate_forecast_shape(&arr, &[scenario_count, h, count], "Scenarios")?;
                let (w0, w1, window_initial) =
                    resolve_windows(initial, resolution, horizon, interval, count, time_range)?;
                let windowed = if w0 == 0 && w1 == count {
                    arr
                } else {
                    slice_count_axis(&arr, 2, w0, w1)
                };
                let scen = Scenarios::new(
                    window_initial,
                    resolution,
                    horizon,
                    interval,
                    w1 - w0,
                    scenario_count,
                    windowed,
                )
                .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::Scenarios(scen))
            }

            TimeSeriesType::DeterministicSingleTimeSeries => {
                // The stored array is the underlying STS 1-D-like array, shape
                // [total_len, *E]. Synthesize a Deterministic of shape
                // [H, count, *E] by gathering windows.
                let arr = self.backend.get_array(&meta.data_hash)?;
                let initial = required_initial(&meta, "DeterministicSingleTimeSeries")?;
                let resolution = required_resolution(&meta, "DeterministicSingleTimeSeries")?;
                let horizon = required_horizon(&meta, "DeterministicSingleTimeSeries")?;
                let interval = required_interval(&meta, "DeterministicSingleTimeSeries")?;
                let count = required_count(&meta, "DeterministicSingleTimeSeries")?;
                let h = compute_h(horizon, resolution).map_err(TimeSeriesError::IntegrityError)?;
                let interval_ms = interval.num_milliseconds();
                let res_ms = resolution.num_milliseconds();
                if interval_ms % res_ms != 0 {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "DeterministicSingleTimeSeries: interval ({interval_ms} ms) \
                         is not evenly divisible by resolution ({res_ms} ms)"
                    )));
                }
                let interval_steps = (interval_ms / res_ms) as usize;
                let total_len = arr.length();
                // Validate that all windows fit in the underlying array.
                let required = (count.saturating_sub(1)) * interval_steps + h;
                if required > total_len {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "DeterministicSingleTimeSeries: (count-1)*interval_steps+H = {required} \
                         exceeds total_len = {total_len}"
                    )));
                }
                // Element bytes per underlying step.
                let elem_shape: Vec<usize> = arr.shape[1..].to_vec();
                let elem_bytes: usize = elem_shape.iter().product::<usize>() * arr.dtype.size();
                let elem_factor = if elem_bytes == 0 {
                    arr.dtype.size()
                } else {
                    elem_bytes
                };

                let (w0, w1, window_initial) =
                    resolve_windows(initial, resolution, horizon, interval, count, time_range)?;
                let selected = w1 - w0;

                // Build output array [H, selected, *E].
                let out_shape: Vec<usize> = std::iter::once(h)
                    .chain(std::iter::once(selected))
                    .chain(elem_shape.iter().copied())
                    .collect();
                let out_nelems: usize = out_shape.iter().product();
                let mut out_bytes = vec![0u8; out_nelems * arr.dtype.size()];

                for j in 0..selected {
                    let k = w0 + j; // source window index
                    for s in 0..h {
                        let src_idx = k * interval_steps + s;
                        let src_off = src_idx * elem_factor;
                        // Row-major offset for [s, j] in [H, selected] with elem_factor.
                        let dst_off = (s * selected + j) * elem_factor;
                        out_bytes[dst_off..dst_off + elem_factor]
                            .copy_from_slice(&arr.bytes[src_off..src_off + elem_factor]);
                    }
                }

                let out_arr = TypedArray::new(arr.dtype, out_shape, out_bytes)
                    .map_err(TimeSeriesError::IntegrityError)?;
                let det = Deterministic::new(
                    window_initial,
                    resolution,
                    horizon,
                    interval,
                    selected,
                    out_arr,
                )
                .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::Deterministic(det))
            }
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
    pub fn get_array_by_hash(&self, hash: &[u8; 32]) -> Result<TypedArray> {
        self.backend.get_array(hash)
    }

    pub fn get_time_series_keys(&self, owner_uuid: &str) -> Result<Vec<TimeSeriesKey>> {
        self.metadata.list_keys_for_owner(owner_uuid)
    }

    /// Derive `DeterministicSingleTimeSeries` forecasts from the stored
    /// `SingleTimeSeries` associations, mirroring InfrastructureSystems.jl's
    /// `transform_single_time_series!`.
    ///
    /// Every `SingleTimeSeries` in the store is re-described as a
    /// `DeterministicSingleTimeSeries` that shares the same underlying array (no
    /// data is copied); the forecast windows are synthesized on read. `horizon`
    /// and `interval` define the windowing and must be positive multiples of
    /// each series' resolution; `count` is derived from each series' length as
    /// `(length - horizon_steps) / interval_steps + 1`.
    ///
    /// All-or-nothing: if any series is too short to fit a single horizon window
    /// or has an incompatible `interval`, nothing is committed. Returns the
    /// number of series transformed.
    pub fn transform_single_time_series(
        &mut self,
        horizon: Duration,
        interval: Duration,
    ) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        use crate::metadata::MetadataFilter;
        let sources = self.metadata.list(&MetadataFilter {
            time_series_type: Some(TimeSeriesType::SingleTimeSeries),
            ..Default::default()
        })?;

        // Build every DST metadata row up front so a single ineligible series
        // aborts the whole transform before any write.
        let mut new_metas = Vec::with_capacity(sources.len());
        for src in &sources {
            let resolution = required_resolution(src, "transform_single_time_series")?;
            let total_len = src.length.ok_or_else(|| {
                TimeSeriesError::IntegrityError("SingleTimeSeries missing length".into())
            })?;
            let res_ms = resolution.num_milliseconds();
            let interval_ms = interval.num_milliseconds();
            if res_ms <= 0 {
                return Err(TimeSeriesError::InvalidParameter(
                    "resolution must be positive".into(),
                ));
            }
            if interval_ms <= 0 || interval_ms % res_ms != 0 {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "interval ({interval_ms} ms) must be a positive multiple of \
                     resolution ({res_ms} ms)"
                )));
            }
            let interval_steps = (interval_ms / res_ms) as usize;
            let h = compute_h(horizon, resolution).map_err(TimeSeriesError::InvalidParameter)?;
            if h == 0 || h > total_len {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "horizon ({h} steps) exceeds SingleTimeSeries length ({total_len}) \
                     for '{}'",
                    src.name
                )));
            }
            let count = (total_len - h) / interval_steps + 1;
            new_metas.push(TimeSeriesMetadata {
                time_series_type: TimeSeriesType::DeterministicSingleTimeSeries,
                horizon: Some(horizon),
                interval: Some(interval),
                count: Some(count),
                ..src.clone()
            });
        }

        let tx = self.metadata.transaction()?;
        for meta in &new_metas {
            if let Err(e) = MetadataStore::insert(&tx, meta) {
                drop(tx);
                return Err(e);
            }
        }
        tx.commit()?;
        Ok(new_metas.len())
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

    /// Return the forecast parameters recorded in the store.
    ///
    /// Looks for any metadata row whose type is a forecast type and returns its
    /// `horizon`, `interval`, `count`, and `resolution`. If no forecasts exist,
    /// returns [`ForecastParameters::default()`]. When multiple forecasts are
    /// present, returns the parameters from the first one found (v0 stores a
    /// single coherent forecast configuration; callers that need per-type
    /// parameters should use [`Self::list_time_series`] directly).
    pub fn get_forecast_parameters(&self) -> Result<ForecastParameters> {
        use crate::metadata::MetadataFilter;
        // Check each forecast type in priority order.
        for ts_type in [
            TimeSeriesType::Deterministic,
            TimeSeriesType::DeterministicSingleTimeSeries,
            TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios,
        ] {
            let rows = self.metadata.list(&MetadataFilter {
                time_series_type: Some(ts_type),
                ..Default::default()
            })?;
            if let Some(first) = rows.into_iter().next() {
                return Ok(ForecastParameters {
                    horizon: first.horizon,
                    interval: first.interval,
                    count: first.count,
                    resolution: first.resolution,
                });
            }
        }
        Ok(ForecastParameters::default())
    }

    pub fn get_time_series_counts(&self) -> Result<TimeSeriesCounts> {
        let forecasts = self.metadata.count_by_type(TimeSeriesType::Deterministic)?
            + self
                .metadata
                .count_by_type(TimeSeriesType::DeterministicSingleTimeSeries)?
            + self.metadata.count_by_type(TimeSeriesType::Probabilistic)?
            + self.metadata.count_by_type(TimeSeriesType::Scenarios)?;
        Ok(TimeSeriesCounts {
            components_with_time_series: self.metadata.count_distinct_owners()?,
            static_time_series: self
                .metadata
                .count_by_type(TimeSeriesType::SingleTimeSeries)?
                + self
                    .metadata
                    .count_by_type(TimeSeriesType::NonSequentialTimeSeries)?,
            forecasts,
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

fn validate_non_sequential(series: &NonSequentialTimeSeries) -> Result<()> {
    if series.timestamps.len() != series.data.length() || series.length != series.data.length() {
        return Err(TimeSeriesError::InvalidParameter(
            "timestamp count, length, and data length must match".into(),
        ));
    }
    if series.timestamps.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(TimeSeriesError::InvalidParameter(
            "timestamps must be strictly increasing".into(),
        ));
    }
    Ok(())
}

/// Build the metadata row for a dense forecast (`Deterministic` /
/// `Probabilistic` / `Scenarios`) added via [`Store::add_time_series_bulk`].
/// The array is stored standalone in its native shape; `percentiles` is `Some`
/// only for `Probabilistic`.
#[allow(clippy::too_many_arguments)]
fn forecast_metadata(
    item: &AddRequest,
    time_series_type: TimeSeriesType,
    initial_timestamp: chrono::DateTime<chrono::Utc>,
    resolution: Duration,
    horizon: Duration,
    interval: Duration,
    count: usize,
    data: &TypedArray,
    percentiles: Option<Vec<f64>>,
) -> TimeSeriesMetadata {
    TimeSeriesMetadata {
        owner_uuid: item.owner_uuid.clone(),
        owner_type: item.owner_type.clone(),
        owner_category: item.owner_category,
        time_series_type,
        name: item.name.clone(),
        data_hash: array_hash(data),
        initial_timestamp: Some(initial_timestamp),
        resolution: Some(resolution),
        length: Some(data.length()),
        horizon: Some(horizon),
        interval: Some(interval),
        count: Some(count),
        timestamps: None,
        features: item.features.clone(),
        scaling_factor_multiplier: item.scaling_factor_multiplier.clone(),
        units: item.units.clone(),
        percentiles,
        dtype: data.dtype,
        element_shape: data.element_shape().to_vec(),
        logical_type: item.logical_type.clone(),
    }
}

/// Build the key returned for a dense forecast added via
/// [`Store::add_time_series_bulk`].
fn forecast_key(
    item: &AddRequest,
    time_series_type: TimeSeriesType,
    resolution: Duration,
) -> TimeSeriesKey {
    TimeSeriesKey {
        owner_uuid: item.owner_uuid.clone(),
        time_series_type,
        name: item.name.clone(),
        resolution: Some(resolution),
        features: item.features.clone(),
    }
}

fn catalog_sqlite_path(nc_path: &Path) -> PathBuf {
    let mut p = nc_path.to_path_buf();
    let new_name = match p.file_name().and_then(|n| n.to_str()) {
        Some(name) => format!("{name}.sqlite"),
        None => "metadata.sqlite".to_string(),
    };
    p.set_file_name(new_name);
    p
}

// ---------------------------------------------------------------------------
// Forecast read-path helpers
// ---------------------------------------------------------------------------

/// Slice a contiguous range `[w0, w1)` along `axis` of a row-major array.
///
/// This is a strided gather: axis `a` is not necessarily the leading axis, so
/// the bytes for each "outer" block are not contiguous in the source buffer.
///
/// - `outer = product(shape[0..axis])` — number of outer blocks.
/// - `inner_bytes = product(shape[axis+1..]) * dtype.size()` — bytes per
///   element in the axis-stride.
/// - For each outer block `o`, the source bytes for windows `[w0, w1)` live at
///   `o * axis_len * inner_bytes + w0 * inner_bytes .. + w1 * inner_bytes`.
///
/// The returned array has the same dtype and all the same shape dims except
/// `shape[axis]` which becomes `w1 - w0`.
pub(crate) fn slice_count_axis(arr: &TypedArray, axis: usize, w0: usize, w1: usize) -> TypedArray {
    assert!(
        axis < arr.shape.len(),
        "axis {axis} out of bounds for shape {:?}",
        arr.shape
    );
    assert!(w0 <= w1, "w0 ({w0}) must be <= w1 ({w1})");
    let axis_len = arr.shape[axis];
    assert!(w1 <= axis_len, "w1 ({w1}) > axis_len ({axis_len})");

    let outer: usize = arr.shape[..axis].iter().product();
    let inner_bytes: usize = arr.shape[axis + 1..].iter().product::<usize>() * arr.dtype.size();
    let window_bytes = (w1 - w0) * inner_bytes;

    let mut out_bytes = Vec::with_capacity(outer * window_bytes);
    for o in 0..outer {
        let block_start = o * axis_len * inner_bytes;
        let src_start = block_start + w0 * inner_bytes;
        let src_end = block_start + w1 * inner_bytes;
        out_bytes.extend_from_slice(&arr.bytes[src_start..src_end]);
    }

    let mut new_shape = arr.shape.clone();
    new_shape[axis] = w1 - w0;

    TypedArray {
        dtype: arr.dtype,
        shape: new_shape,
        bytes: out_bytes,
    }
}

/// Resolve the window range `[w0, w1)` from an optional `time_range`.
///
/// Implements the IS.jl rule: `start_time` must be the first timestamp of a
/// window (`initial_timestamp + k·interval`), `end` is exclusive. Returns
/// `(w0, w1, first_window_initial_timestamp)`.
///
/// On success, `w0 <= w1 <= count`. Empty selection returns `(0, 0, initial)`.
fn resolve_windows(
    initial: chrono::DateTime<chrono::Utc>,
    _resolution: Duration,
    _horizon: Duration,
    interval: Duration,
    count: usize,
    time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
) -> Result<(usize, usize, chrono::DateTime<chrono::Utc>)> {
    match time_range {
        None => Ok((0, count, initial)),
        Some((start, end)) => {
            if end < start {
                return Err(TimeSeriesError::InvalidParameter("end < start".into()));
            }
            let interval_ms = interval.num_milliseconds();
            if interval_ms <= 0 {
                return Err(TimeSeriesError::InvalidParameter(
                    "forecast interval must be positive".into(),
                ));
            }
            // Check alignment: (start - initial) must be a non-negative integer
            // multiple of interval.
            let offset_ms = (start - initial).num_milliseconds();
            if offset_ms < 0 || offset_ms % interval_ms != 0 {
                return Err(TimeSeriesError::InvalidParameter(
                    "forecast start_time must align to a window boundary \
                     (initial_timestamp + k·interval)"
                        .into(),
                ));
            }
            let start_k = (offset_ms / interval_ms) as usize;

            // Collect all k in [0, count) whose window start is in [start, end).
            let mut w0 = count; // sentinel: no window selected yet
            let mut w1 = 0usize;
            for k in 0..count {
                let window_start = initial + Duration::milliseconds(k as i64 * interval_ms);
                if window_start >= start && window_start < end {
                    if w0 == count {
                        w0 = k;
                    }
                    w1 = k + 1;
                }
            }

            // Empty selection.
            if w0 == count {
                // Return initial_timestamp aligned to start (the requested start).
                let first_ts = initial + Duration::milliseconds(start_k as i64 * interval_ms);
                return Ok((0, 0, first_ts));
            }

            let first_ts = initial + Duration::milliseconds(w0 as i64 * interval_ms);
            Ok((w0, w1, first_ts))
        }
    }
}

// --- Metadata field accessors that return IntegrityError on None ---

fn required_initial(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<chrono::DateTime<chrono::Utc>> {
    meta.initial_timestamp.ok_or_else(|| {
        TimeSeriesError::IntegrityError(format!("{label} missing initial_timestamp"))
    })
}

fn required_resolution(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<Duration> {
    meta.resolution
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing resolution")))
}

fn required_horizon(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<Duration> {
    meta.horizon
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing horizon")))
}

fn required_interval(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<Duration> {
    meta.interval
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing interval")))
}

fn required_count(meta: &crate::types::metadata::TimeSeriesMetadata, label: &str) -> Result<usize> {
    meta.count
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing count")))
}

/// Validate that the leading shape dims of `arr` match `expected_prefix`,
/// returning an `IntegrityError` if not. Trailing element dims are allowed.
fn validate_forecast_shape(arr: &TypedArray, expected_prefix: &[usize], label: &str) -> Result<()> {
    if arr.shape.len() < expected_prefix.len() {
        return Err(TimeSeriesError::IntegrityError(format!(
            "{label}: stored shape {:?} has fewer dims than expected prefix {expected_prefix:?}",
            arr.shape
        )));
    }
    for (i, (&got, &exp)) in arr.shape.iter().zip(expected_prefix.iter()).enumerate() {
        if got != exp {
            return Err(TimeSeriesError::IntegrityError(format!(
                "{label}: stored shape {:?} mismatch at dim {i}: expected {exp}, got {got}",
                arr.shape
            )));
        }
    }
    Ok(())
}
