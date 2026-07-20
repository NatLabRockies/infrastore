//! High-level `Store` composing the storage backend and metadata store.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::{Result, TimeSeriesError};
use crate::hash::array_hash;
use crate::metadata::{
    AssociationIdentity, FeatureSetCache, MetadataFilter, MetadataStore, SeriesFamily,
    references_to_in_tx,
};
use crate::reader::{ForecastReader, StaticReader, WindowRead};
use crate::storage::{
    CompactionReport, Compression, IntegrityReport, MemoryBackend, NetCdfBackend, StorageBackend,
};
use crate::types::array::{Dtype, TypedArray};
use crate::types::key::{
    ForecastTimeSeriesKey, KeyIdentity, NonSequentialTimeSeriesKey, SingleTimeSeriesKey,
    TimeSeriesKey,
};
use crate::types::metadata::{Features, OwnerCategory, TimeSeriesMetadata};
use crate::types::period::Period;
use crate::types::time_series::{
    Deterministic, NonSequentialTimeSeries, Probabilistic, Scenarios, SingleTimeSeries,
    TimeSeriesData, TimeSeriesType, compute_h,
};

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub owner_id: Option<i64>,
    pub owner_category: Option<OwnerCategory>,
    pub owner_type: Option<String>,
    pub time_series_type: Option<TimeSeriesType>,
    pub name: Option<String>,
    pub resolution: Option<Period>,
    pub interval: Option<Period>,
    pub features: Option<Features>,
}

impl ListFilter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn owner_id(mut self, id: i64) -> Self {
        self.owner_id = Some(id);
        self
    }
    pub fn owner_category(mut self, c: OwnerCategory) -> Self {
        self.owner_category = Some(c);
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
    pub fn resolution(mut self, r: impl Into<Period>) -> Self {
        self.resolution = Some(r.into());
        self
    }
    pub fn interval(mut self, i: impl Into<Period>) -> Self {
        self.interval = Some(i.into());
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
            owner_id: value.owner_id,
            owner_category: value.owner_category,
            owner_type: value.owner_type,
            time_series_type: value.time_series_type,
            name: value.name,
            resolution: value.resolution,
            interval: value.interval,
            features: value.features,
            features_hash: None,
        }
    }
}

/// Single item in a bulk add.
#[derive(Debug, Clone)]
pub struct AddRequest {
    pub owner_id: i64,
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub data: TimeSeriesData,
    pub features: Features,
    pub units: Option<String>,
    /// Opaque logical-type label for domain reconstruction (binding-owned).
    pub logical_type: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct TimeSeriesCounts {
    pub components_with_time_series: i64,
    pub static_time_series: i64,
    pub forecasts: i64,
}

/// Owner- and array-oriented counts (distinct owners per category, distinct
/// stored arrays per kind). Unlike [`TimeSeriesCounts`] the time-series counts
/// here are de-duplicated by array content, and owners are split by category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeSeriesCountsDetailed {
    pub components_with_time_series: i64,
    pub supplemental_attributes_with_time_series: i64,
    pub static_time_series_count: i64,
    pub forecast_count: i64,
}

/// One resolution's shared static grid, as reported by
/// [`Store::check_static_consistency`]: every `SingleTimeSeries` at
/// `resolution` shares this `(initial_timestamp, length)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticConsistency {
    pub resolution: Period,
    pub initial_timestamp: chrono::DateTime<chrono::Utc>,
    pub length: usize,
}

#[derive(Debug, Default, Clone)]
pub struct ForecastParameters {
    pub horizon: Option<Period>,
    pub interval: Option<Period>,
    pub count: Option<usize>,
    pub resolution: Option<Period>,
    pub initial_timestamp: Option<chrono::DateTime<chrono::Utc>>,
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
        // A read-only store opens both halves read-only: the NetCDF side needs
        // no write permission (works on read-only media, shared HDF5 lock) and
        // its write paths error with `ReadOnlyStore` as a backstop behind the
        // `Store::add_*` / `remove_*` guards.
        let backend = NetCdfBackend::open(path, read_only)?;
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
    pub fn add_time_series(
        &mut self,
        owner_id: i64,
        owner_type: &str,
        owner_category: OwnerCategory,
        data: TimeSeriesData,
        features: Features,
        units: Option<String>,
    ) -> Result<TimeSeriesKey> {
        self.add_per_column(vec![AddRequest {
            owner_id,
            owner_type: owner_type.to_string(),
            owner_category,
            data,
            features,
            units,
            logical_type: None,
        }])
        .map(|mut keys| keys.remove(0))
    }

    /// Bulk insert. All-or-nothing: any error rolls back every association and
    /// array put performed in this call.
    ///
    /// This is a managed batch, so it takes the block-write path
    /// ([`Self::bulk_add`] internals): packed series are packed into batch-sized
    /// datasets that fill whole chunks. A one-at-a-time un-managed loop should use
    /// [`Self::add_time_series`], which packs incrementally into shared datasets.
    #[tracing::instrument(skip(self, items), fields(count = items.len()))]
    pub fn add_time_series_bulk(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesKey>> {
        self.flush_bulk_add(items)
    }

    /// Per-column insert used by single [`Self::add_time_series`] calls: each
    /// packed array is dropped into the first free slot of a shared, default-width
    /// dataset (created on demand, spilling once full). This keeps incremental
    /// un-managed adds space-efficient and still grouped for read-by-timestamp,
    /// at the cost of a per-column read-modify-write under the timestamp-major
    /// chunking. All-or-nothing, like [`Self::add_time_series_bulk`].
    #[tracing::instrument(skip(self, items), fields(count = items.len()))]
    fn add_per_column(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesKey>> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }

        // Stage backend writes so we can roll them back on metadata error.
        let mut staged_hashes: Vec<[u8; 32]> = Vec::with_capacity(items.len());
        let tx = self.metadata.transaction()?;
        let mut keys = Vec::with_capacity(items.len());
        // Feature sets are shared, and a batch typically spans only a handful of
        // distinct ones; write each once rather than once per item.
        let mut feature_sets = FeatureSetCache::default();

        for item in &items {
            let RequestParts {
                hash,
                resolution,
                packed,
                meta,
                key,
            } = build_request_parts(item)?;
            let data = request_array(item);

            let already_present = self.backend.contains(&hash)?;
            tracing::debug!(
                owner = item.owner_id,
                bytes = data.bytes.len(),
                packed,
                already_present,
                "backend put_array",
            );
            self.backend.put_array(&hash, data, resolution, packed)?;
            if !already_present {
                staged_hashes.push(hash);
            }

            match insert_association(&tx, &meta, &mut feature_sets) {
                Ok(()) => {
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

    /// Begin a buffered bulk add. Requests pushed onto the returned [`BulkAdd`]
    /// are accumulated in memory and written together by [`BulkAdd::commit`],
    /// which packs each shape group into batch-sized datasets so the timestamp-
    /// major chunks are filled whole rather than one slow column at a time.
    /// Dropping the guard without committing discards the buffer (writes nothing).
    pub fn bulk_add(&mut self) -> BulkAdd<'_> {
        BulkAdd {
            store: self,
            items: Vec::new(),
            committed: false,
        }
    }

    /// Flush a buffered bulk add: write every array — packed types as batch-sized
    /// blocks (one or more datasets per shape group, chunks filled whole),
    /// standalone types individually — then insert all associations in one
    /// transaction. All-or-nothing: any metadata error rolls the transaction back
    /// and removes every array staged in this call.
    #[tracing::instrument(skip(self, items), fields(count = items.len()))]
    fn flush_bulk_add(&mut self, items: Vec<AddRequest>) -> Result<Vec<TimeSeriesKey>> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }

        // Derive parts (validates + hashes) for every item, aligned to `items`.
        let parts: Vec<RequestParts> = items
            .iter()
            .map(build_request_parts)
            .collect::<Result<_>>()?;
        let mut staged_hashes: Vec<[u8; 32]> = Vec::new();

        // Group packed inputs by (dtype, element_shape, length, resolution); each
        // group is written as one or more batch-sized blocks. Standalone inputs
        // (irregular series and dense forecasts) keep the per-array path.
        let mut packed_groups: HashMap<(Dtype, Vec<usize>, usize, Period), Vec<usize>> =
            HashMap::new();
        for (i, p) in parts.iter().enumerate() {
            let array = request_array(&items[i]);
            if p.packed {
                packed_groups
                    .entry((
                        array.dtype,
                        array.element_shape().to_vec(),
                        array.length(),
                        p.resolution,
                    ))
                    .or_default()
                    .push(i);
            } else {
                let already = self.backend.contains(&p.hash)?;
                self.backend
                    .put_array(&p.hash, array, p.resolution, false)?;
                if !already {
                    staged_hashes.push(p.hash);
                }
            }
        }
        for (group, idxs) in &packed_groups {
            let hashes: Vec<[u8; 32]> = idxs.iter().map(|&i| parts[i].hash).collect();
            let arrays: Vec<&TypedArray> = idxs.iter().map(|&i| request_array(&items[i])).collect();
            let written = self.backend.put_packed_block(&hashes, &arrays, group.3)?;
            for (j, &i) in idxs.iter().enumerate() {
                if written[j] {
                    staged_hashes.push(parts[i].hash);
                }
            }
        }

        // Insert associations in input order; roll the whole batch back on error.
        let tx = self.metadata.transaction()?;
        let mut feature_sets = FeatureSetCache::default();
        for p in &parts {
            if let Err(e) = insert_association(&tx, &p.meta, &mut feature_sets) {
                drop(tx);
                for staged in &staged_hashes {
                    let _ = self.backend.remove_array(staged);
                }
                return Err(e);
            }
        }
        tx.commit()?;
        tracing::debug!(count = parts.len(), "bulk-add transaction committed");
        Ok(parts.into_iter().map(|p| p.key).collect())
    }

    #[tracing::instrument(skip(self, key), fields(owner = key.owner_id, name = %key.name))]
    pub fn remove_time_series(&mut self, key: &KeyIdentity) -> Result<()> {
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

    /// Remove every time series for the owner `(owner_id, owner_category)`, or
    /// every time series in the store when `owner` is `None`. Returns the count
    /// removed.
    pub fn clear_time_series(&mut self, owner: Option<(i64, OwnerCategory)>) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.transaction()?;
        let removed = match owner {
            Some((id, category)) => MetadataStore::delete_by_owner(&tx, id, category)?,
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
    pub fn replace_owner(
        &mut self,
        old_owner: i64,
        new_owner: i64,
        owner_category: OwnerCategory,
    ) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let tx = self.metadata.transaction()?;
        let updated = MetadataStore::replace_owner(&tx, old_owner, new_owner, owner_category)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Copy an existing association onto another owner, optionally renaming it.
    ///
    /// Arrays are content-addressed, so this writes only a new association row
    /// pointing at the same `data_hash`: no array data is duplicated. Every
    /// descriptive column is carried over verbatim — crucially the
    /// `time_series_type`, so a `DeterministicSingleTimeSeries` stays one instead
    /// of being materialized into a dense `Deterministic` (which is what a
    /// read-then-write copy through the bindings would produce).
    ///
    /// The copy keeps the source's `owner_category`; `new_name` defaults to the
    /// source name. Fails with `DuplicateTimeSeries` if the destination already
    /// holds a series with the same identity.
    #[tracing::instrument(skip(self, src), fields(owner = src.owner_id, name = %src.name))]
    pub fn copy_time_series(
        &mut self,
        src: &KeyIdentity,
        dst_owner_id: i64,
        dst_owner_type: &str,
        new_name: Option<&str>,
    ) -> Result<TimeSeriesKey> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }

        let mut meta = self.metadata.get_by_key(src)?;
        meta.owner_id = dst_owner_id;
        meta.owner_type = dst_owner_type.to_string();
        if let Some(name) = new_name {
            meta.name = name.to_string();
        }

        let dst = KeyIdentity {
            owner_id: meta.owner_id,
            owner_category: meta.owner_category,
            time_series_type: meta.time_series_type,
            name: meta.name.clone(),
            resolution: meta.resolution,
            interval: meta.interval,
            features: meta.features.clone(),
        };
        if self.has_time_series(&dst)? {
            return Err(TimeSeriesError::DuplicateTimeSeries);
        }

        let tx = self.metadata.transaction()?;
        MetadataStore::insert(&tx, &meta)?;
        tx.commit()?;

        TimeSeriesKey::from_metadata(&meta)
    }

    #[tracing::instrument(skip(self, key, time_range), fields(owner = key.owner_id, name = %key.name, has_time_range = time_range.is_some()))]
    pub fn get_time_series(
        &self,
        key: &KeyIdentity,
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
                        if !resolution.is_positive() {
                            return Err(TimeSeriesError::InvalidParameter(
                                "resolution must be positive".into(),
                            ));
                        }
                        // `start` is floored and `end` is ceil-ed onto the
                        // resolution grid (calendar-aware for monthly periods); the
                        // bounds need not be grid-aligned.
                        let start_idx = resolution.floor_steps(initial, start).min(length);
                        let end_idx = resolution
                            .ceil_steps(initial, end)
                            .min(length)
                            .max(start_idx);
                        let data = self
                            .backend
                            .get_slice(&meta.data_hash, start_idx..end_idx)?;
                        let new_initial =
                            resolution
                                .add_to(initial, start_idx as i64)
                                .ok_or_else(|| {
                                    TimeSeriesError::IntegrityError(
                                        "sliced initial overflow".into(),
                                    )
                                })?;
                        (data, new_initial, end_idx - start_idx)
                    }
                };

                Ok(TimeSeriesData::SingleTimeSeries(SingleTimeSeries {
                    initial_timestamp: sliced_initial,
                    resolution,
                    length: sliced_length,
                    data,
                    name: meta.name.clone(),
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
                let series = NonSequentialTimeSeries::new(timestamps, data, meta.name.clone())
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
                    meta.name.clone(),
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
                    meta.name.clone(),
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
                    meta.name.clone(),
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
                let interval_steps = resolution.divide_into(&interval).map_err(|_| {
                    TimeSeriesError::IntegrityError(format!(
                        "DeterministicSingleTimeSeries: interval ({}) is not an integer \
                         multiple of resolution ({})",
                        interval.to_iso8601(),
                        resolution.to_iso8601()
                    ))
                })?;
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
                    meta.name.clone(),
                )
                .map_err(TimeSeriesError::IntegrityError)?;
                Ok(TimeSeriesData::Deterministic(det))
            }
        }
    }

    pub fn list_time_series(&self, filter: ListFilter) -> Result<Vec<TimeSeriesMetadata>> {
        self.metadata.list(&filter.into())
    }

    /// List the [`TimeSeriesKey`] of every association matching `filter`. This is
    /// the key-centric counterpart of [`Self::list_time_series`]: each row is
    /// reduced to its identifying + descriptive key, dropping physical storage
    /// detail (`data_hash`, `dtype`, `logical_type`, `percentiles`) which is read
    /// on demand via [`Self::get_metadata`]. The binding-facing listing path.
    pub fn list_keys(&self, filter: ListFilter) -> Result<Vec<TimeSeriesKey>> {
        self.metadata
            .list(&filter.into())?
            .iter()
            .map(TimeSeriesKey::from_metadata)
            .collect()
    }

    /// Like [`Self::list_keys`], but pairs each key with the 32-byte content hash
    /// of the array it resolves to. Rows that share a stored array carry the same
    /// hash, which is what lets a caller group time series by their underlying
    /// data: deduplicated identical arrays, and a `SingleTimeSeries` together with
    /// any `DeterministicSingleTimeSeries` derived from it. The hash is read
    /// straight off each metadata row, so this is still one catalog query.
    pub fn list_keys_with_hash(
        &self,
        filter: ListFilter,
    ) -> Result<Vec<(TimeSeriesKey, [u8; 32])>> {
        self.metadata
            .list(&filter.into())?
            .iter()
            .map(|m| Ok((TimeSeriesKey::from_metadata(m)?, m.data_hash)))
            .collect()
    }

    /// Build a [`StaticReader`] over the `SingleTimeSeries` matching `filter`.
    ///
    /// The filter must pin a resolution (one resolution per reader). All matched
    /// series must share one grid (`initial_timestamp` + `length`); this is
    /// validated here and errors on divergence, which is what lets the per-read
    /// path skip presence checks. The reader is then driven with
    /// [`Self::static_read`]. See [`crate::reader`].
    pub fn build_static_reader(&self, mut filter: ListFilter) -> Result<StaticReader> {
        if filter.resolution.is_none() {
            return Err(TimeSeriesError::InvalidParameter(
                "build_static_reader requires a resolution filter (one resolution per reader)"
                    .into(),
            ));
        }
        // SingleTimeSeries-only; accept an explicit matching type, reject others.
        match filter.time_series_type {
            None | Some(TimeSeriesType::SingleTimeSeries) => {
                filter.time_series_type = Some(TimeSeriesType::SingleTimeSeries);
            }
            Some(other) => {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "build_static_reader handles SingleTimeSeries only; got {}",
                    other.as_str()
                )));
            }
        }
        let rows = self.list_time_series(filter)?;
        let (initial, resolution, length, groups) = crate::reader::build_groups(rows)?;
        Ok(StaticReader::from_parts(
            initial, resolution, length, groups,
        ))
    }

    /// Read the value of every series in `reader` at timestamp `at`, filling the
    /// reader's reusable buffers in place. Afterwards walk
    /// [`StaticReader::groups`] for the columnar bytes. Errors (never clamps) if
    /// `at` is off the reader's grid.
    pub fn static_read(
        &self,
        reader: &mut StaticReader,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let index = reader.index_at(at)?;
        for group in reader.groups_mut() {
            group.fill(|hashes, out| self.backend.read_index_into(hashes, index, out))?;
        }
        reader.mark_read(at);
        Ok(())
    }

    /// Build a [`ForecastReader`] over the forecasts matching `filter`.
    ///
    /// The filter must name a forecast type and pin a resolution. A
    /// `Deterministic` reader is **abstract**: it also includes
    /// `DeterministicSingleTimeSeries`, read into identical `[H, *E]` windows
    /// (mirroring core's `AbstractDeterministic`). `Probabilistic` and
    /// `Scenarios` are exact. All matched forecasts must share one window
    /// timeline (`initial_timestamp` + `interval` + `count`); this is validated
    /// and errors on divergence. Drive with [`Self::forecast_read`]. See
    /// [`crate::reader`].
    pub fn build_forecast_reader(&self, filter: ListFilter) -> Result<ForecastReader> {
        let reported = match filter.time_series_type {
            Some(
                t @ (TimeSeriesType::Deterministic
                | TimeSeriesType::DeterministicSingleTimeSeries
                | TimeSeriesType::Probabilistic
                | TimeSeriesType::Scenarios),
            ) => t,
            Some(other) => {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "build_forecast_reader handles forecast types (Deterministic/\
                     DeterministicSingleTimeSeries/Probabilistic/Scenarios); got {}",
                    other.as_str()
                )));
            }
            None => {
                return Err(TimeSeriesError::InvalidParameter(
                    "build_forecast_reader requires a forecast time_series_type in the filter"
                        .into(),
                ));
            }
        };
        if filter.resolution.is_none() {
            return Err(TimeSeriesError::InvalidParameter(
                "build_forecast_reader requires a resolution filter (one resolution per reader)"
                    .into(),
            ));
        }
        // A Deterministic reader is abstract over its concrete storage types.
        let concrete: Vec<TimeSeriesType> = match reported {
            TimeSeriesType::Deterministic => vec![
                TimeSeriesType::Deterministic,
                TimeSeriesType::DeterministicSingleTimeSeries,
            ],
            other => vec![other],
        };
        let mut items = Vec::new();
        for t in concrete {
            let mut f = filter.clone();
            f.time_series_type = Some(t);
            for m in self.list_time_series(f)? {
                let (_dtype, shape) = self.backend.array_shape(&m.data_hash)?;
                items.push((m, shape));
            }
        }
        crate::reader::build_forecast_entries(reported, items)
    }

    /// Read the forecast window at timestamp `at` for every forecast in `reader`,
    /// filling the reader's reusable per-entry buffers in place. Afterwards walk
    /// [`ForecastReader::entries`] for the window bytes. Errors (never clamps) if
    /// `at` is off the reader's window timeline.
    pub fn forecast_read(
        &self,
        reader: &mut ForecastReader,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let window = reader.window_index(at)?;
        // One read per *slot*: forecasts that share an array and read plan
        // (e.g. components referencing one shared forecast) collapse to a single
        // slot at build time, so the backend is hit once for all of them.
        for slot in reader.slots_mut() {
            slot.fill(|read, hash, out| match *read {
                WindowRead::Dense { count_axis } => {
                    self.backend.read_window_into(hash, count_axis, window, out)
                }
                WindowRead::Derived {
                    interval_steps,
                    horizon_steps,
                } => {
                    let start = window * interval_steps;
                    self.backend
                        .read_range_into(hash, start, horizon_steps, out)
                }
            })?;
        }
        reader.mark_read(at);
        Ok(())
    }

    /// Read many full series at once, returning a [`TimeSeriesData`] per key in
    /// order. This is the bulk counterpart to [`Self::get_time_series`] for
    /// whole-series reads (e.g. exploration or plotting): packed `SingleTimeSeries`
    /// are read in one decompress-once pass per dataset via
    /// [`StorageBackend::read_arrays`], rather than re-reading every chunk once per
    /// series — the read-side complement to the timestamp-major layout, where a
    /// single full-series read is otherwise the slow direction. Other types are
    /// standalone arrays with no batching benefit, so they reuse the per-key
    /// [`Self::get_time_series`] path. No time-range slicing — each series is
    /// returned in full.
    #[tracing::instrument(skip(self, keys), fields(count = keys.len()))]
    pub fn bulk_read(&self, keys: &[&KeyIdentity]) -> Result<Vec<TimeSeriesData>> {
        let metas: Vec<TimeSeriesMetadata> = keys
            .iter()
            .map(|k| self.metadata.get_by_key(k))
            .collect::<Result<_>>()?;

        // Batch the packed SingleTimeSeries reads; everything else is standalone
        // and reuses the per-key reconstruction.
        let single_hashes: Vec<[u8; 32]> = metas
            .iter()
            .filter(|m| m.time_series_type == TimeSeriesType::SingleTimeSeries)
            .map(|m| m.data_hash)
            .collect();
        let mut single_arrays = self.backend.read_arrays(&single_hashes)?.into_iter();

        let mut out = Vec::with_capacity(keys.len());
        for (meta, key) in metas.iter().zip(keys) {
            if meta.time_series_type == TimeSeriesType::SingleTimeSeries {
                let data = single_arrays.next().ok_or_else(|| {
                    TimeSeriesError::IntegrityError(
                        "bulk_read: fewer arrays returned than SingleTimeSeries keys".into(),
                    )
                })?;
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
                out.push(TimeSeriesData::SingleTimeSeries(SingleTimeSeries {
                    initial_timestamp: initial,
                    resolution,
                    length,
                    data,
                    name: meta.name.clone(),
                }));
            } else {
                out.push(self.get_time_series(key, None)?);
            }
        }
        Ok(out)
    }

    /// Look up the full metadata record for a key. Errors with `NotFound` if no
    /// association matches. Used by external bindings (e.g. the Julia
    /// `RustTimeSeriesStore`) to reconstruct a typed metadata object on read.
    pub fn get_metadata(&self, key: &KeyIdentity) -> Result<TimeSeriesMetadata> {
        self.metadata.get_by_key(key)
    }

    /// Resolve a forecast addressed by attributes plus a [`RequestedType`] to the
    /// concrete [`TimeSeriesKey`] of the single matching association. The returned
    /// key's `time_series_type` is the concrete type that matched.
    ///
    /// For a [`RequestedType::Concrete`] request the concrete type must match;
    /// for [`RequestedType::AbstractDeterministic`] a stored `Deterministic` or
    /// `DeterministicSingleTimeSeries` matches (the two cannot coexist for one
    /// family, so at most one ever does). This is the authoritative replacement
    /// for the bindings' former guess-and-retry fallback: the catalog — not the
    /// caller — decides which concrete type satisfies the request.
    ///
    /// `resolution` and `interval` are optional filters on the identity. Leave
    /// either unset to match across it; supply it to disambiguate when several
    /// series share the other attributes (e.g. a day-ahead and a real-time
    /// forecast that differ only by interval).
    ///
    /// Errors:
    /// - [`TimeSeriesError::NotFound`] if nothing matches.
    /// - [`TimeSeriesError::InvalidParameter`] if the request is ambiguous (more
    ///   than one stored series matches); the caller must then narrow it with a
    ///   concrete type, a resolution, and/or an interval.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_forecast_key(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
        name: &str,
        resolution: Option<Period>,
        interval: Option<Period>,
        features: Features,
        requested: crate::types::time_series::RequestedType,
    ) -> Result<TimeSeriesKey> {
        // List candidates sharing (owner, name, resolution, interval, features),
        // then keep those whose concrete type satisfies the request. A concrete
        // request with resolution+interval pinned resolves to one row via the
        // unique index; looser requests may match several and are reported as
        // ambiguous rather than silently picking one.
        let f_hash = crate::hash::features_hash(&features);
        let mut matches = self.metadata.list(&MetadataFilter {
            owner_id: Some(owner_id),
            owner_category: Some(owner_category),
            name: Some(name.to_string()),
            resolution,
            interval,
            features_hash: Some(f_hash),
            ..Default::default()
        })?;
        matches.retain(|m| m.features == features && requested.matches(m.time_series_type));
        match matches.len() {
            0 => Err(TimeSeriesError::NotFound),
            1 => TimeSeriesKey::from_metadata(&matches.pop().unwrap()),
            _ => {
                // More than one candidate matches: with `resolution`/`interval`
                // unset this can be several forecasts at different resolutions or
                // intervals, so report the actual candidates rather than
                // asserting a single shape.
                let describe = |p: Option<Period>| match p {
                    Some(p) => p.to_iso8601(),
                    None => "-".to_string(),
                };
                let mut candidates: Vec<String> = matches
                    .iter()
                    .map(|m| {
                        format!(
                            "{} (resolution={}, interval={})",
                            m.time_series_type.as_str(),
                            describe(m.resolution),
                            describe(m.interval),
                        )
                    })
                    .collect();
                candidates.sort();
                Err(TimeSeriesError::InvalidParameter(format!(
                    "ambiguous forecast request for '{name}': {} candidates match \
                     ({}); narrow it with a concrete type, resolution, and/or interval",
                    candidates.len(),
                    candidates.join(", "),
                )))
            }
        }
    }

    /// Count `SingleTimeSeries` and `DeterministicSingleTimeSeries` associations
    /// that reference `data_hash`, across all owners, as `(sts, dst)`.
    ///
    /// A `DeterministicSingleTimeSeries` shares the underlying array of the
    /// `SingleTimeSeries` it was derived from, so a binding deciding whether a
    /// `SingleTimeSeries` can be removed without orphaning a DST needs these
    /// counts. Resolved by a single grouped catalog query rather than scanning
    /// every association in the caller.
    pub fn count_array_references(&self, data_hash: &[u8; 32]) -> Result<(usize, usize)> {
        let (sts, dst) = self.metadata.count_array_references(data_hash)?;
        Ok((sts as usize, dst as usize))
    }

    /// Fetch the full stored array for a content hash. The metadata-owning
    /// binding resolves a key to its `data_hash`, then calls this to read the
    /// underlying values.
    pub fn get_array_by_hash(&self, hash: &[u8; 32]) -> Result<TypedArray> {
        self.backend.get_array(hash)
    }

    pub fn get_time_series_keys(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
    ) -> Result<Vec<TimeSeriesKey>> {
        self.metadata.list_keys_for_owner(owner_id, owner_category)
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
        horizon: impl Into<Period>,
        interval: impl Into<Period>,
        owner_category: Option<OwnerCategory>,
        resolution: Option<Period>,
    ) -> Result<usize> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let (horizon, interval) = (horizon.into(), interval.into());
        // Push the owner-category and resolution restrictions into SQL rather
        // than listing every SingleTimeSeries and discarding the misses: a store
        // whose components are transformed one resolution at a time should not
        // pay to hydrate the other resolutions' features on every call.
        let sources = self.metadata.list(&MetadataFilter {
            time_series_type: Some(TimeSeriesType::SingleTimeSeries),
            owner_category,
            resolution,
            ..Default::default()
        })?;

        // Series that already have a DeterministicSingleTimeSeries view *at this
        // interval* are skipped so the transform is idempotent (e.g. re-deriving
        // one series when others were transformed earlier, as during a component
        // copy) — but only when the existing view also has this `horizon`. The
        // identity (owner_id/owner_category plus name/resolution/interval/
        // features) does not include the horizon, so a same-identity view with a
        // *different* horizon cannot coexist with the requested one; silently
        // skipping it would leave the old horizon serving reads while reporting
        // success, so that case is an error instead. Interval is part of the
        // identity, so re-deriving the same series at a different interval is a
        // distinct view.
        //
        // Both dedup sets are read via `list_identities`, which returns the
        // stored `features_hash` column directly: the identity test needs the
        // hash, not the features themselves, so this skips hydrating (and
        // re-hashing) the features of every forecast already in the store.
        let interval_iso = interval.to_iso8601();
        let existing_dst: HashMap<AssociationIdentity, Option<Period>> = self
            .metadata
            .list_identities(TimeSeriesType::DeterministicSingleTimeSeries)?
            .into_iter()
            .collect();

        // Families that already hold a real `Deterministic` forecast. A DST is a
        // synthetic view and is mutually exclusive with a `Deterministic` for one
        // family (owner, name, resolution, features, ignoring interval), so
        // deriving a DST over such a family is rejected.
        let existing_det: HashSet<SeriesFamily> = self
            .metadata
            .list_identities(TimeSeriesType::Deterministic)?
            .into_iter()
            .map(|(identity, _horizon)| SeriesFamily::from(identity))
            .collect();

        // Build every DST metadata row up front so a single ineligible series
        // aborts the whole transform before any write.
        let mut new_metas = Vec::with_capacity(sources.len());
        for src in &sources {
            let src_features_hash = crate::hash::features_hash(&src.features);
            let src_resolution_iso = src.resolution.map(|r| r.to_iso8601());
            // Reject deriving a DST over a family that already holds a real
            // Deterministic forecast (interval-independent).
            let det_family = SeriesFamily {
                owner_id: src.owner_id,
                owner_category: src.owner_category,
                name: src.name.clone(),
                resolution: src_resolution_iso.clone(),
                features_hash: src_features_hash,
            };
            if existing_det.contains(&det_family) {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "cannot derive DeterministicSingleTimeSeries for '{}': a Deterministic \
                     forecast of the same series already exists; they are mutually exclusive",
                    src.name
                )));
            }
            let src_key = AssociationIdentity {
                owner_id: src.owner_id,
                owner_category: src.owner_category,
                name: src.name.clone(),
                resolution: src_resolution_iso,
                interval: Some(interval_iso.clone()),
                features_hash: src_features_hash,
            };
            if let Some(existing_horizon) = existing_dst.get(&src_key) {
                if *existing_horizon == Some(horizon) {
                    // Same identity, same horizon: already derived; idempotent.
                    continue;
                }
                // Same identity but a different horizon: the two views cannot
                // coexist (horizon is not part of the identity), and silently
                // keeping the old one would misreport success.
                let describe = |h: &Option<Period>| match h {
                    Some(h) => h.to_iso8601(),
                    None => "-".to_string(),
                };
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "cannot derive DeterministicSingleTimeSeries for '{}' at interval {}: \
                     a view with horizon {} already exists (requested {}); remove it first \
                     or transform at a different interval",
                    src.name,
                    interval.to_iso8601(),
                    describe(existing_horizon),
                    horizon.to_iso8601(),
                )));
            }
            let resolution = required_resolution(src, "transform_single_time_series")?;
            let total_len = src.length.ok_or_else(|| {
                TimeSeriesError::IntegrityError("SingleTimeSeries missing length".into())
            })?;
            let interval_steps = resolution.divide_into(&interval).map_err(|_| {
                TimeSeriesError::InvalidParameter(format!(
                    "interval ({}) must be a positive integer multiple of resolution ({})",
                    interval.to_iso8601(),
                    resolution.to_iso8601()
                ))
            })?;
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
        // One cache for the whole batch: every derived row shares its source's
        // feature set, and sources overwhelmingly share sets with each other, so
        // the feature-set writes collapse to a handful regardless of how many
        // series are transformed.
        let mut feature_sets = FeatureSetCache::default();
        for meta in &new_metas {
            if let Err(e) = MetadataStore::insert_batched(&tx, meta, &mut feature_sets) {
                drop(tx);
                return Err(e);
            }
        }
        tx.commit()?;
        Ok(new_metas.len())
    }

    pub fn has_time_series(&self, key: &KeyIdentity) -> Result<bool> {
        match self.metadata.get_by_key(key) {
            Ok(_) => Ok(true),
            Err(TimeSeriesError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn get_resolutions(&self, time_series_type: Option<TimeSeriesType>) -> Result<Vec<Period>> {
        self.metadata.distinct_resolutions(time_series_type)
    }

    /// Return the forecast parameters recorded in the store, optionally
    /// restricted to forecasts with a given `resolution` and/or `interval`.
    ///
    /// Looks for any metadata row whose type is a forecast type (and matches the
    /// filters) and returns its `horizon`, `interval`, `count`, and `resolution`.
    /// If none match, returns [`ForecastParameters::default()`]. When multiple
    /// match, returns the first one found (v0 stores a single coherent forecast
    /// configuration; callers that need per-type parameters should use
    /// [`Self::list_time_series`] directly). Both `resolution` and `interval`
    /// are pushed into the catalog query.
    pub fn get_forecast_parameters(
        &self,
        resolution: Option<Period>,
        interval: Option<Period>,
    ) -> Result<ForecastParameters> {
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
                resolution,
                interval,
                ..Default::default()
            })?;
            if let Some(row) = rows.into_iter().next() {
                return Ok(ForecastParameters {
                    horizon: row.horizon,
                    interval: row.interval,
                    count: row.count,
                    resolution: row.resolution,
                    initial_timestamp: row.initial_timestamp,
                });
            }
        }
        Ok(ForecastParameters::default())
    }

    /// Verify that, per resolution, all `SingleTimeSeries` share one
    /// `(initial_timestamp, length)` grid. Series at *different* resolutions
    /// legitimately have different grids (an hourly and a 5-minute profile over
    /// one year differ in length), so consistency is only required within a
    /// resolution.
    ///
    /// Returns one [`StaticConsistency`] row per resolution present (empty when
    /// the store has no `SingleTimeSeries`), ordered by resolution. `resolution`
    /// optionally scopes the check (and the returned rows) to one grid. Errors
    /// with [`TimeSeriesError::IntegrityError`] when any single resolution holds
    /// more than one distinct `(initial_timestamp, length)` pair. One `DISTINCT`
    /// query.
    pub fn check_static_consistency(
        &self,
        resolution: Option<Period>,
    ) -> Result<Vec<StaticConsistency>> {
        let rows = self.metadata.distinct_single_grids(resolution)?;
        let mut out: Vec<StaticConsistency> = Vec::with_capacity(rows.len());
        for (res, ts, len) in rows {
            // Rows arrive ordered by resolution, so a divergent grid shows up
            // as two adjacent rows with the same resolution.
            if let Some(prev) = out.last()
                && prev.resolution == res
            {
                return Err(TimeSeriesError::IntegrityError(format!(
                    "SingleTimeSeries at resolution {} have more than one \
                     (initial_timestamp, length) pair: ({}, {}) vs ({}, {})",
                    res.to_iso8601(),
                    prev.initial_timestamp,
                    prev.length,
                    ts,
                    len,
                )));
            }
            out.push(StaticConsistency {
                resolution: res,
                initial_timestamp: ts,
                length: len as usize,
            });
        }
        Ok(out)
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

    /// Association count grouped by time series type. Replaces a binding-side
    /// scan-and-group with one catalog query.
    pub fn counts_by_type(&self) -> Result<Vec<(TimeSeriesType, i64)>> {
        self.metadata.counts_by_type()
    }

    /// Number of distinct stored arrays (content hashes); shared series count once.
    pub fn num_distinct_arrays(&self) -> Result<i64> {
        self.metadata.count_distinct_arrays()
    }

    /// Distinct owners per category and distinct stored arrays per kind (static
    /// vs forecast). Replaces a binding-side full scan that grouped owners and
    /// hashes in memory.
    pub fn time_series_counts_detailed(&self) -> Result<TimeSeriesCountsDetailed> {
        const STATIC: [TimeSeriesType; 2] = [
            TimeSeriesType::SingleTimeSeries,
            TimeSeriesType::NonSequentialTimeSeries,
        ];
        const FORECAST: [TimeSeriesType; 4] = [
            TimeSeriesType::Deterministic,
            TimeSeriesType::DeterministicSingleTimeSeries,
            TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios,
        ];
        Ok(TimeSeriesCountsDetailed {
            components_with_time_series: self
                .metadata
                .count_distinct_owners_in_category(OwnerCategory::Component)?,
            supplemental_attributes_with_time_series: self
                .metadata
                .count_distinct_owners_in_category(OwnerCategory::SupplementalAttribute)?,
            static_time_series_count: self.metadata.count_distinct_arrays_for_types(&STATIC)?,
            forecast_count: self.metadata.count_distinct_arrays_for_types(&FORECAST)?,
        })
    }

    /// Distinct owner ids of `category` that have a time series, optionally
    /// restricted by type and/or resolution.
    pub fn list_owner_ids(
        &self,
        category: OwnerCategory,
        time_series_type: Option<TimeSeriesType>,
        resolution: Option<Period>,
    ) -> Result<Vec<i64>> {
        self.metadata
            .list_owner_ids(category, time_series_type, resolution)
    }

    /// Grouped static-series summary (one row per distinct owner/name/shape
    /// combination, with the association count). The binding builds the
    /// presentation table; the core does the grouping.
    pub fn static_summary(&self) -> Result<Vec<crate::metadata::StaticSummaryRow>> {
        self.metadata.static_summary()
    }

    /// Grouped forecast summary (one row per distinct owner/name/window
    /// configuration, with the association count).
    pub fn forecast_summary(&self) -> Result<Vec<crate::metadata::ForecastSummaryRow>> {
        self.metadata.forecast_summary()
    }

    /// Reclaim space in both halves of the artifact: reusable packed slots and
    /// unreachable arrays in the NetCDF file, and feature sets in the SQLite
    /// catalog that no association references any more.
    pub fn compact(&mut self) -> Result<CompactionReport> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        let mut report = self.backend.compact()?;
        let tx = self.metadata.transaction()?;
        report.feature_sets_reclaimed = MetadataStore::sweep_orphan_feature_sets(&tx)?;
        tx.commit()?;
        Ok(report)
    }

    pub fn verify_integrity(&self) -> Result<IntegrityReport> {
        self.backend.verify()
    }

    pub fn flush(&mut self) -> Result<()> {
        self.backend.flush()
    }

    /// Persist this store's data to `path` (the NetCDF arrays) and its companion
    /// `<path>.sqlite` (the metadata). Works for both on-disk stores (copies the
    /// two artifacts) and in-memory stores (materializes arrays + metadata to
    /// disk). Existing target files are overwritten.
    ///
    /// Because arrays are content-addressed, copying every array by hash plus the
    /// full metadata database reproduces all time series — static, forecast, and
    /// non-sequential — without reconstructing per-type semantics.
    pub fn persist_to(&mut self, path: &Path) -> Result<()> {
        self.flush()?;
        let sqlite_path = catalog_sqlite_path(path);

        if let Some(src) = self.netcdf_path.clone() {
            if src != path {
                std::fs::copy(&src, path)?;
                std::fs::copy(catalog_sqlite_path(&src), &sqlite_path)?;
            }
            return Ok(());
        }

        // In-memory store: materialize arrays and metadata to disk. VACUUM INTO
        // requires the target sqlite to be absent, so clear both artifacts first.
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(&sqlite_path);
        {
            let mut nc = NetCdfBackend::create(path, self.compression())?;
            // Plan each distinct array's layout before writing: packed is only
            // valid for arrays that every referencing association reads as a
            // series along axis 0 (SingleTimeSeries and its derived DST views).
            // Dense forecasts and non-sequential series must stay standalone —
            // the forecast window read path rejects packed arrays.
            let mut plans: HashMap<[u8; 32], (bool, Period)> = HashMap::new();
            for (key, hash) in self.list_keys_with_hash(ListFilter::default())? {
                let packed = matches!(
                    key.time_series_type(),
                    TimeSeriesType::SingleTimeSeries
                        | TimeSeriesType::DeterministicSingleTimeSeries
                );
                // The resolution only groups the packed on-disk layout; reads
                // locate arrays by content hash, so the fallback for
                // resolution-less (non-sequential) series is harmless.
                let resolution = key
                    .resolution()
                    .unwrap_or_else(|| Period::fixed(chrono::Duration::nanoseconds(1)));
                plans
                    .entry(hash)
                    .and_modify(|(p, _)| *p &= packed)
                    .or_insert((packed, resolution));
            }
            for (hash, (packed, resolution)) in &plans {
                let array = self.backend.get_array(hash)?;
                nc.put_array(hash, &array, *resolution, *packed)?;
            }
            nc.flush()?;
        }
        self.metadata.backup_to(&sqlite_path)?;
        Ok(())
    }
}

/// A buffered bulk-add session returned by [`Store::bulk_add`]. Requests are
/// accumulated in memory via [`Self::push`] / [`Self::add`] and written together
/// by [`Self::commit`], which packs each shape group into batch-sized datasets
/// so writes fill whole chunks. Dropping the guard without calling `commit`
/// discards every buffered request and writes nothing.
pub struct BulkAdd<'a> {
    store: &'a mut Store,
    items: Vec<AddRequest>,
    committed: bool,
}

impl BulkAdd<'_> {
    /// Buffer one prebuilt request. No validation or I/O happens here; both are
    /// deferred to [`Self::commit`], which is all-or-nothing.
    pub fn push(&mut self, request: AddRequest) -> &mut Self {
        self.items.push(request);
        self
    }

    /// Buffer one request from its parts (convenience over [`Self::push`]).
    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &mut self,
        owner_id: i64,
        owner_type: &str,
        owner_category: OwnerCategory,
        data: TimeSeriesData,
        features: Features,
        units: Option<String>,
    ) -> &mut Self {
        self.push(AddRequest {
            owner_id,
            owner_type: owner_type.to_string(),
            owner_category,
            data,
            features,
            units,
            logical_type: None,
        })
    }

    /// The number of requests buffered so far.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no requests have been buffered.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Flush the buffer: write all arrays as batch-sized blocks and insert every
    /// association in one transaction, returning the keys in push order. On any
    /// error nothing is committed and staged arrays are rolled back.
    pub fn commit(mut self) -> Result<Vec<TimeSeriesKey>> {
        self.committed = true;
        let items = std::mem::take(&mut self.items);
        self.store.flush_bulk_add(items)
    }
}

impl Drop for BulkAdd<'_> {
    fn drop(&mut self) {
        if !self.committed && !self.items.is_empty() {
            tracing::debug!(
                discarded = self.items.len(),
                "BulkAdd dropped without commit; buffered requests discarded"
            );
        }
    }
}

/// The persistence inputs derived from one [`AddRequest`]: the array content
/// hash, the resolution that keys the packed pool, whether the array is packed
/// (vs. standalone), the metadata row, and the resulting key. Shared by the
/// per-item write path ([`Store::add_time_series_bulk`]) and the buffered
/// block-write path ([`Store::bulk_add`]).
struct RequestParts {
    hash: [u8; 32],
    resolution: Period,
    packed: bool,
    meta: TimeSeriesMetadata,
    key: TimeSeriesKey,
}

/// Derive the [`RequestParts`] for one request, validating where required
/// (`NonSequentialTimeSeries` timestamps). `SingleTimeSeries` is packed; every
/// other type is stored standalone.
fn build_request_parts(item: &AddRequest) -> Result<RequestParts> {
    let (hash, resolution, packed, meta, key) = match &item.data {
        TimeSeriesData::SingleTimeSeries(single) => {
            let hash = array_hash(&single.data);
            (
                hash,
                single.resolution,
                true,
                TimeSeriesMetadata {
                    owner_id: item.owner_id,
                    owner_type: item.owner_type.clone(),
                    owner_category: item.owner_category,
                    time_series_type: TimeSeriesType::SingleTimeSeries,
                    name: single.name.clone(),
                    data_hash: hash,
                    initial_timestamp: Some(single.initial_timestamp),
                    resolution: Some(single.resolution),
                    length: Some(single.length),
                    horizon: None,
                    interval: None,
                    count: None,
                    timestamps: None,
                    features: item.features.clone(),
                    units: item.units.clone(),
                    percentiles: None,
                    dtype: single.data.dtype,
                    element_shape: single.data.element_shape().to_vec(),
                    logical_type: item.logical_type.clone(),
                },
                TimeSeriesKey::Single(SingleTimeSeriesKey::new(
                    item.owner_id,
                    item.owner_category,
                    single.name.clone(),
                    single.resolution,
                    item.features.clone(),
                    single.initial_timestamp,
                    single.length,
                )),
            )
        }
        TimeSeriesData::NonSequentialTimeSeries(non_sequential) => {
            validate_non_sequential(non_sequential)?;
            let hash = array_hash(&non_sequential.data);
            (
                hash,
                // Non-sequential series are stored standalone, so the
                // resolution (which keys the packed pool) is unused.
                Period::Months(0),
                false,
                TimeSeriesMetadata {
                    owner_id: item.owner_id,
                    owner_type: item.owner_type.clone(),
                    owner_category: item.owner_category,
                    time_series_type: TimeSeriesType::NonSequentialTimeSeries,
                    name: non_sequential.name.clone(),
                    data_hash: hash,
                    initial_timestamp: None,
                    resolution: None,
                    length: Some(non_sequential.length),
                    horizon: None,
                    interval: None,
                    count: None,
                    timestamps: Some(non_sequential.timestamps.clone()),
                    features: item.features.clone(),
                    units: item.units.clone(),
                    percentiles: None,
                    dtype: non_sequential.data.dtype,
                    element_shape: non_sequential.data.element_shape().to_vec(),
                    logical_type: item.logical_type.clone(),
                },
                TimeSeriesKey::NonSequential(NonSequentialTimeSeriesKey::new(
                    item.owner_id,
                    item.owner_category,
                    non_sequential.name.clone(),
                    item.features.clone(),
                    non_sequential.length,
                )),
            )
        }
        // Dense forecast types are stored as standalone arrays in their
        // native shape. `DeterministicSingleTimeSeries` is not added
        // directly; it is derived from a stored `SingleTimeSeries` via
        // [`Store::transform_single_time_series`].
        TimeSeriesData::Deterministic(det) => (
            array_hash(&det.data),
            det.resolution,
            false,
            forecast_metadata(
                item,
                TimeSeriesType::Deterministic,
                &det.name,
                det.initial_timestamp,
                det.resolution,
                det.horizon,
                det.interval,
                det.count,
                &det.data,
                None,
            ),
            forecast_key(
                item,
                TimeSeriesType::Deterministic,
                &det.name,
                det.resolution,
                det.initial_timestamp,
                det.horizon,
                det.interval,
                det.count,
            ),
        ),
        TimeSeriesData::Probabilistic(prob) => (
            array_hash(&prob.data),
            prob.resolution,
            false,
            forecast_metadata(
                item,
                TimeSeriesType::Probabilistic,
                &prob.name,
                prob.initial_timestamp,
                prob.resolution,
                prob.horizon,
                prob.interval,
                prob.count,
                &prob.data,
                Some(prob.percentiles.clone()),
            ),
            forecast_key(
                item,
                TimeSeriesType::Probabilistic,
                &prob.name,
                prob.resolution,
                prob.initial_timestamp,
                prob.horizon,
                prob.interval,
                prob.count,
            ),
        ),
        TimeSeriesData::Scenarios(scen) => (
            array_hash(&scen.data),
            scen.resolution,
            false,
            forecast_metadata(
                item,
                TimeSeriesType::Scenarios,
                &scen.name,
                scen.initial_timestamp,
                scen.resolution,
                scen.horizon,
                scen.interval,
                scen.count,
                &scen.data,
                None,
            ),
            forecast_key(
                item,
                TimeSeriesType::Scenarios,
                &scen.name,
                scen.resolution,
                scen.initial_timestamp,
                scen.horizon,
                scen.interval,
                scen.count,
            ),
        ),
    };
    Ok(RequestParts {
        hash,
        resolution,
        packed,
        meta,
        key,
    })
}

/// The value array backing a request, regardless of time-series type.
fn request_array(item: &AddRequest) -> &TypedArray {
    match &item.data {
        TimeSeriesData::SingleTimeSeries(single) => &single.data,
        TimeSeriesData::NonSequentialTimeSeries(non_sequential) => &non_sequential.data,
        TimeSeriesData::Deterministic(det) => &det.data,
        TimeSeriesData::Probabilistic(prob) => &prob.data,
        TimeSeriesData::Scenarios(scen) => &scen.data,
    }
}

/// Insert one association, enforcing the Deterministic/DeterministicSingleTimeSeries
/// mutual exclusion: a DST is a synthetic view of a SingleTimeSeries, so a family
/// may hold one or the other but never both. A DST is only ever created by
/// [`Store::transform_single_time_series`], so the only overlap reachable here is
/// adding a `Deterministic` when a DST already exists.
fn insert_association(
    tx: &rusqlite::Transaction<'_>,
    meta: &TimeSeriesMetadata,
    cache: &mut FeatureSetCache,
) -> Result<()> {
    let conflict = if meta.time_series_type == TimeSeriesType::Deterministic {
        crate::metadata::forecast_family_conflict(
            tx,
            meta.owner_id,
            meta.owner_category,
            &meta.name,
            meta.resolution,
            &crate::hash::features_hash(&meta.features),
            TimeSeriesType::DeterministicSingleTimeSeries,
        )
    } else {
        Ok(false)
    };
    match conflict {
        Ok(true) => Err(TimeSeriesError::InvalidParameter(format!(
            "cannot add Deterministic '{}': a DeterministicSingleTimeSeries view of the \
             same series already exists; they are mutually exclusive",
            meta.name
        ))),
        Ok(false) => MetadataStore::insert_batched(tx, meta, cache).map(|_| ()),
        Err(e) => Err(e),
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
    name: &str,
    initial_timestamp: chrono::DateTime<chrono::Utc>,
    resolution: Period,
    horizon: Period,
    interval: Period,
    count: usize,
    data: &TypedArray,
    percentiles: Option<Vec<f64>>,
) -> TimeSeriesMetadata {
    TimeSeriesMetadata {
        owner_id: item.owner_id,
        owner_type: item.owner_type.clone(),
        owner_category: item.owner_category,
        time_series_type,
        name: name.to_owned(),
        data_hash: array_hash(data),
        initial_timestamp: Some(initial_timestamp),
        resolution: Some(resolution),
        length: Some(data.length()),
        horizon: Some(horizon),
        interval: Some(interval),
        count: Some(count),
        timestamps: None,
        features: item.features.clone(),
        units: item.units.clone(),
        percentiles,
        dtype: data.dtype,
        element_shape: data.element_shape().to_vec(),
        logical_type: item.logical_type.clone(),
    }
}

/// Build the key returned for a dense forecast added via
/// [`Store::add_time_series_bulk`].
#[allow(clippy::too_many_arguments)]
fn forecast_key(
    item: &AddRequest,
    time_series_type: TimeSeriesType,
    name: &str,
    resolution: Period,
    initial_timestamp: chrono::DateTime<chrono::Utc>,
    horizon: Period,
    interval: Period,
    count: usize,
) -> TimeSeriesKey {
    TimeSeriesKey::Forecast(ForecastTimeSeriesKey::new(
        item.owner_id,
        item.owner_category,
        time_series_type,
        name.to_owned(),
        resolution,
        item.features.clone(),
        initial_timestamp,
        horizon,
        interval,
        count,
    ))
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
/// On success, `w0 <= w1 <= count`. A `start` aligned to the grid but at or
/// beyond `count` (i.e. past the last stored window) is rejected with
/// [`TimeSeriesError::InvalidParameter`] rather than returning an empty
/// selection. A zero-width range (`end == start`) over an in-range `start`
/// legitimately selects nothing and returns `(0, 0, start)`.
fn resolve_windows(
    initial: chrono::DateTime<chrono::Utc>,
    _resolution: Period,
    _horizon: Period,
    interval: Period,
    count: usize,
    time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
) -> Result<(usize, usize, chrono::DateTime<chrono::Utc>)> {
    let window_start = |k: usize| -> Result<chrono::DateTime<chrono::Utc>> {
        interval
            .add_to(initial, k as i64)
            .ok_or_else(|| TimeSeriesError::IntegrityError("window timestamp overflow".into()))
    };
    match time_range {
        None => Ok((0, count, initial)),
        Some((start, end)) => {
            if end < start {
                return Err(TimeSeriesError::InvalidParameter("end < start".into()));
            }
            if !interval.is_positive() {
                return Err(TimeSeriesError::InvalidParameter(
                    "forecast interval must be positive".into(),
                ));
            }
            // `start` must be a window boundary: `initial + k·interval`
            // (calendar-aware for monthly intervals).
            let start_k = interval.steps_between(initial, start).map_err(|_| {
                TimeSeriesError::InvalidParameter(
                    "forecast start_time must align to a window boundary \
                     (initial_timestamp + k·interval)"
                        .into(),
                )
            })?;

            // A start aligned to the grid but at or beyond the window count
            // refers to windows that do not exist; reject it rather than
            // silently returning an empty selection.
            if start_k >= count {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "forecast start_time is past the last window (resolves to window \
                     index {start_k}, but only {count} window(s) are stored)"
                )));
            }

            // Collect all k in [0, count) whose window start is in [start, end).
            let mut w0 = count; // sentinel: no window selected yet
            let mut w1 = 0usize;
            for k in 0..count {
                let ws = window_start(k)?;
                if ws >= start && ws < end {
                    if w0 == count {
                        w0 = k;
                    }
                    w1 = k + 1;
                }
            }

            // Empty selection: report the initial timestamp at the requested start.
            if w0 == count {
                return Ok((0, 0, window_start(start_k)?));
            }

            Ok((w0, w1, window_start(w0)?))
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
) -> Result<Period> {
    meta.resolution
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing resolution")))
}

fn required_horizon(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<Period> {
    meta.horizon
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("{label} missing horizon")))
}

fn required_interval(
    meta: &crate::types::metadata::TimeSeriesMetadata,
    label: &str,
) -> Result<Period> {
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

#[cfg(test)]
mod resolve_windows_tests {
    use super::*;
    use chrono::{DateTime, Duration, TimeZone, Utc};

    // A forecast grid of 4 windows spaced 12h apart at hours 0, 12, 24, 36.
    fn t(h: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap() + Duration::hours(h)
    }

    fn rw(start_h: i64, end_h: i64) -> Result<(usize, usize, DateTime<Utc>)> {
        let interval = Period::Fixed(Duration::hours(12));
        let res = Period::Fixed(Duration::hours(1));
        let horizon = Period::Fixed(Duration::hours(6));
        resolve_windows(
            t(0),
            res,
            horizon,
            interval,
            4,
            Some((t(start_h), t(end_h))),
        )
    }

    #[test]
    fn full_and_partial_selection() {
        let interval = Period::Fixed(Duration::hours(12));
        let res = Period::Fixed(Duration::hours(1));
        let horizon = Period::Fixed(Duration::hours(6));
        // `None` selects every window.
        assert_eq!(
            resolve_windows(t(0), res, horizon, interval, 4, None).unwrap(),
            (0, 4, t(0))
        );
        // Middle range [12h, 36h) -> windows 1 and 2.
        assert_eq!(rw(12, 36).unwrap(), (1, 3, t(12)));
        // The exact last window (index 3) is in range.
        assert_eq!(rw(36, 48).unwrap(), (3, 4, t(36)));
    }

    #[test]
    fn zero_width_in_range_is_empty() {
        // An in-range start with `end == start` legitimately selects nothing.
        assert_eq!(rw(12, 12).unwrap(), (0, 0, t(12)));
    }

    #[test]
    fn start_past_last_window_errors() {
        // Hour 48 resolves to window index 4, but only 4 windows (0..=3) exist.
        assert!(matches!(
            rw(48, 60),
            Err(TimeSeriesError::InvalidParameter(_))
        ));
    }

    #[test]
    fn misaligned_or_reversed_errors() {
        // 1h off the 12h window grid.
        assert!(matches!(
            rw(1, 24),
            Err(TimeSeriesError::InvalidParameter(_))
        ));
        // end < start.
        assert!(matches!(
            rw(24, 12),
            Err(TimeSeriesError::InvalidParameter(_))
        ));
    }
}
