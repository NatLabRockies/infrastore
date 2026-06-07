//! NetCDF4-backed storage backend.
//!
//! ## Layout
//!
//! ```text
//! <file>.nc
//! ├── attribute  data_format_version = "0.1.0"
//! └── group      time_series/
//!     └── group  single/
//!         ├── var      sts_{length}_{resolution_s}        f64  shape (length, MAX_COLS) chunks (1, MAX_COLS)
//!         ├── var      sts_{length}_{resolution_s}_h      str  shape (MAX_COLS,)        # hex hashes
//!         ├── var      sts_{length}_{resolution_s}__1     ...                           # spill dataset
//!         └── var      sts_{length}_{resolution_s}__1_h   ...
//! ```
//!
//! v0 supports only 1-D `SingleTimeSeries` data (shape `(length,)`). Multi-dim
//! per-step values are returned as an `InvalidParameter` error from `put_array`;
//! the same backend is the natural place to add multi-dim handling later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ndarray::ArrayD;
use netcdf::{Extents, FileMut, Group, GroupMut};

use crate::error::{Result, TimeSeriesError};
use crate::hash::{array_hash, hash_hex};
use crate::version::DATA_FORMAT_VERSION;

use super::{CompactionReport, IntegrityReport, StorageBackend};

/// Max columns per compacted dataset before we spill into a new dataset.
pub const MAX_COLS_PER_DATASET: usize = 1000;

const ROOT_GROUP: &str = "time_series";
const SINGLE_GROUP: &str = "single";

/// Suffix for a data variable's companion hash-string variable.
const HASH_SUFFIX: &str = "_h";

#[derive(Debug, Clone)]
struct DatasetState {
    data_name: String,
    hash_name: String,
    length: usize,
    resolution_seconds: i64,
    /// Hex-encoded hash for each column. `None` means the slot is free.
    columns: Vec<Option<String>>,
}

impl DatasetState {
    fn first_free(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.is_none())
    }
    fn full(&self) -> bool {
        !self.columns.iter().any(|c| c.is_none())
    }
}

#[derive(Debug, Default)]
struct Index {
    by_hash: HashMap<[u8; 32], (String, usize)>,
}

fn dataset_base_name(length: usize, resolution_seconds: i64) -> String {
    format!("sts_{length}_{resolution_seconds}")
}

fn spill_name(base: &str, n: usize) -> String {
    if n == 0 {
        base.to_string()
    } else {
        format!("{base}__{n}")
    }
}

pub struct NetCdfBackend {
    inner: Mutex<Inner>,
}

struct Inner {
    file: FileMut,
    path: PathBuf,
    datasets: HashMap<String, DatasetState>,
    index: Index,
}

impl NetCdfBackend {
    pub fn create(path: &Path) -> Result<Self> {
        let mut file = netcdf::create(path).map_err(map_nc)?;
        file.add_attribute("data_format_version", DATA_FORMAT_VERSION)
            .map_err(map_nc)?;
        // Create the time_series/single hierarchy up-front.
        {
            let mut ts = file.add_group(ROOT_GROUP).map_err(map_nc)?;
            let _ = ts.add_group(SINGLE_GROUP).map_err(map_nc)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                file,
                path: path.to_path_buf(),
                datasets: HashMap::new(),
                index: Index::default(),
            }),
        })
    }

    pub fn open(path: &Path) -> Result<Self> {
        let file = netcdf::append(path).map_err(map_nc)?;
        let mut backend = Self {
            inner: Mutex::new(Inner {
                file,
                path: path.to_path_buf(),
                datasets: HashMap::new(),
                index: Index::default(),
            }),
        };
        backend.rebuild_index()?;
        Ok(backend)
    }

    pub fn path(&self) -> PathBuf {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.path.clone()
    }

    fn rebuild_index(&mut self) -> Result<()> {
        let inner = self.inner.get_mut().expect("mutex poisoned");
        // Step 1: read variable names + dim sizes through immutable groups.
        struct ScanRow {
            data_name: String,
            length: usize,
            num_cols: usize,
        }
        let scans: Vec<ScanRow> = inner.with_single(|single| {
            let mut out = Vec::new();
            for var in single.variables() {
                let name = var.name();
                if name.ends_with(HASH_SUFFIX) {
                    continue;
                }
                let dims = var.dimensions();
                if dims.len() != 2 {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "variable {name} has {} dims, expected 2",
                        dims.len()
                    )));
                }
                out.push(ScanRow {
                    data_name: name,
                    length: dims[0].len(),
                    num_cols: dims[1].len(),
                });
            }
            Ok(out)
        })?;

        for row in scans {
            let (length_from_name, resolution_seconds) = parse_dataset_name(&row.data_name)?;
            if length_from_name != row.length {
                return Err(TimeSeriesError::IntegrityError(format!(
                    "variable {} length ({}) disagrees with name ({})",
                    row.data_name, row.length, length_from_name,
                )));
            }
            let hash_name = format!("{}{}", row.data_name, HASH_SUFFIX);

            // Read all hash strings for this dataset in a fresh group borrow.
            let hash_strings: Vec<String> = inner.with_single(|single| {
                let v = single.variable(&hash_name).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!(
                        "missing hash variable {hash_name}"
                    ))
                })?;
                let mut out = Vec::with_capacity(row.num_cols);
                for i in 0..row.num_cols {
                    out.push(v.get_string(i).map_err(map_nc)?);
                }
                Ok(out)
            })?;

            let mut columns = Vec::with_capacity(row.num_cols);
            for (i, s) in hash_strings.into_iter().enumerate() {
                if s.is_empty() {
                    columns.push(None);
                } else {
                    let hash_bytes = hex_to_hash(&s)?;
                    inner
                        .index
                        .by_hash
                        .insert(hash_bytes, (row.data_name.clone(), i));
                    columns.push(Some(s));
                }
            }
            inner.datasets.insert(
                row.data_name.clone(),
                DatasetState {
                    data_name: row.data_name,
                    hash_name,
                    length: row.length,
                    resolution_seconds,
                    columns,
                },
            );
        }
        Ok(())
    }
}

fn parse_dataset_name(name: &str) -> Result<(usize, i64)> {
    let core = name.strip_prefix("sts_").ok_or_else(|| {
        TimeSeriesError::IntegrityError(format!("dataset {name} missing 'sts_' prefix"))
    })?;
    let core = core.split("__").next().unwrap();
    let mut parts = core.splitn(2, '_');
    let length: usize = parts
        .next()
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("bad dataset name {name}")))?
        .parse()
        .map_err(|_| TimeSeriesError::IntegrityError(format!("bad length in {name}")))?;
    let resolution: i64 = parts
        .next()
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("bad dataset name {name}")))?
        .parse()
        .map_err(|_| TimeSeriesError::IntegrityError(format!("bad resolution in {name}")))?;
    Ok((length, resolution))
}

fn hex_to_hash(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 {
        return Err(TimeSeriesError::IntegrityError(format!(
            "hash hex string should be 64 chars, got {}",
            s.len()
        )));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| TimeSeriesError::IntegrityError(format!("bad hex byte at {i} in {s}")))?;
    }
    Ok(out)
}

fn map_nc(e: netcdf::Error) -> TimeSeriesError {
    TimeSeriesError::IntegrityError(format!("netcdf: {e}"))
}

impl Inner {
    /// Run `f` against an immutable handle on `time_series/single`.
    fn with_single<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Group<'_>) -> Result<R>,
    {
        let ts = self
            .file
            .group(ROOT_GROUP)
            .map_err(map_nc)?
            .ok_or_else(|| TimeSeriesError::IntegrityError(format!("missing {ROOT_GROUP}")))?;
        let single = ts
            .group(SINGLE_GROUP)
            .ok_or_else(|| TimeSeriesError::IntegrityError(format!("missing {SINGLE_GROUP}")))?;
        f(&single)
    }

    /// Run `f` against a mutable handle on `time_series/single`.
    fn with_single_mut<F, R>(&mut self, f: F) -> Result<R>
    where
        F: FnOnce(&mut GroupMut<'_>) -> Result<R>,
    {
        let mut ts = self
            .file
            .group_mut(ROOT_GROUP)
            .map_err(map_nc)?
            .ok_or_else(|| TimeSeriesError::IntegrityError(format!("missing {ROOT_GROUP}")))?;
        let mut single = ts
            .group_mut(SINGLE_GROUP)
            .ok_or_else(|| TimeSeriesError::IntegrityError(format!("missing {SINGLE_GROUP}")))?;
        f(&mut single)
    }

    /// Get-or-create the dataset family member to use for the next write.
    fn ensure_writable_dataset(
        &mut self,
        length: usize,
        resolution_seconds: i64,
    ) -> Result<String> {
        let base = dataset_base_name(length, resolution_seconds);
        // Pick existing non-full dataset in this family, deterministically.
        let mut candidates: Vec<(String, bool)> = self
            .datasets
            .values()
            .filter(|d| d.length == length && d.resolution_seconds == resolution_seconds)
            .map(|d| (d.data_name.clone(), d.full()))
            .collect();
        candidates.sort_by_key(|(n, _)| n.clone());

        for (name, full) in &candidates {
            if !full {
                return Ok(name.clone());
            }
        }
        let new_name = spill_name(&base, candidates.len());
        self.create_dataset(&new_name, length, resolution_seconds)?;
        Ok(new_name)
    }

    fn create_dataset(
        &mut self,
        name: &str,
        length: usize,
        resolution_seconds: i64,
    ) -> Result<()> {
        let dim_time = format!("{name}_t");
        let dim_col = format!("{name}_c");
        let hash_name = format!("{name}{HASH_SUFFIX}");
        let name_owned = name.to_string();
        let hash_name_for_closure = hash_name.clone();

        self.with_single_mut(|single| {
            single.add_dimension(&dim_time, length).map_err(map_nc)?;
            single
                .add_dimension(&dim_col, MAX_COLS_PER_DATASET)
                .map_err(map_nc)?;
            let mut var = single
                .add_variable::<f64>(&name_owned, &[&dim_time, &dim_col])
                .map_err(map_nc)?;
            var.set_chunking(&[1, MAX_COLS_PER_DATASET]).map_err(map_nc)?;
            var.set_compression(3, true).map_err(map_nc)?;
            let _h = single
                .add_string_variable(&hash_name_for_closure, &[&dim_col])
                .map_err(map_nc)?;
            Ok(())
        })?;

        self.datasets.insert(
            name.to_string(),
            DatasetState {
                data_name: name.to_string(),
                hash_name,
                length,
                resolution_seconds,
                columns: vec![None; MAX_COLS_PER_DATASET],
            },
        );
        Ok(())
    }
}

impl StorageBackend for NetCdfBackend {
    fn put_array(
        &mut self,
        hash: &[u8; 32],
        data: &ArrayD<f64>,
        length: usize,
        resolution_seconds: i64,
    ) -> Result<()> {
        if data.ndim() != 1 {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "v0 NetCdfBackend supports only 1-D SingleTimeSeries data, got shape {:?}",
                data.shape()
            )));
        }
        if data.shape()[0] != length {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "data length {} disagrees with declared length {}",
                data.shape()[0],
                length
            )));
        }

        let mut inner = self.inner.lock().expect("mutex poisoned");

        if inner.index.by_hash.contains_key(hash) {
            return Ok(());
        }

        let dataset_name = inner.ensure_writable_dataset(length, resolution_seconds)?;
        let col_index = {
            let state = inner
                .datasets
                .get(&dataset_name)
                .expect("dataset just ensured");
            state
                .first_free()
                .ok_or_else(|| TimeSeriesError::IntegrityError(
                    "no free slot in newly-ensured dataset".into(),
                ))?
        };
        let hash_name = inner
            .datasets
            .get(&dataset_name)
            .map(|d| d.hash_name.clone())
            .expect("dataset just ensured");

        let hex = hash_hex(hash);
        let values: Vec<f64> = data.iter().copied().collect();

        {
            let dataset_name = dataset_name.clone();
            let hash_name = hash_name.clone();
            let hex_for_closure = hex.clone();
            inner.with_single_mut(move |single| {
                let mut data_var = single.variable_mut(&dataset_name).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!(
                        "missing variable {dataset_name}"
                    ))
                })?;
                let extents: Extents =
                    [0..length, col_index..col_index + 1].as_slice().into();
                data_var.put_values(&values, extents).map_err(map_nc)?;
                drop(data_var);

                let mut hash_var = single.variable_mut(&hash_name).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!(
                        "missing variable {hash_name}"
                    ))
                })?;
                hash_var.put_string(&hex_for_closure, col_index).map_err(map_nc)?;
                Ok(())
            })?;
        }

        let state = inner
            .datasets
            .get_mut(&dataset_name)
            .expect("dataset just ensured");
        state.columns[col_index] = Some(hex);
        inner
            .index
            .by_hash
            .insert(*hash, (dataset_name, col_index));

        Ok(())
    }

    fn get_array(&self, hash: &[u8; 32]) -> Result<ArrayD<f64>> {
        let inner = self.inner.lock().expect("mutex poisoned");
        let (dataset_name, col_index) = inner
            .index
            .by_hash
            .get(hash)
            .ok_or(TimeSeriesError::NotFound)?
            .clone();
        let length = inner
            .datasets
            .get(&dataset_name)
            .ok_or_else(|| TimeSeriesError::IntegrityError(format!(
                "dataset {dataset_name} missing from state"
            )))?
            .length;

        let values: Vec<f64> = inner.with_single(|single| {
            let var = single.variable(&dataset_name).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!("missing variable {dataset_name}"))
            })?;
            let extents: Extents = [0..length, col_index..col_index + 1].as_slice().into();
            var.get_values::<f64, _>(extents).map_err(map_nc)
        })?;
        ArrayD::from_shape_vec(vec![length], values).map_err(|e| {
            TimeSeriesError::IntegrityError(format!("shape mismatch on read: {e}"))
        })
    }

    fn get_slice(
        &self,
        hash: &[u8; 32],
        range: std::ops::Range<usize>,
    ) -> Result<ArrayD<f64>> {
        let inner = self.inner.lock().expect("mutex poisoned");
        let (dataset_name, col_index) = inner
            .index
            .by_hash
            .get(hash)
            .ok_or(TimeSeriesError::NotFound)?
            .clone();
        let total_length = inner
            .datasets
            .get(&dataset_name)
            .ok_or_else(|| TimeSeriesError::IntegrityError(format!(
                "dataset {dataset_name} missing from state"
            )))?
            .length;
        if range.start > range.end || range.end > total_length {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "slice {:?} out of bounds for length {}",
                range, total_length
            )));
        }
        let len = range.end - range.start;
        let start = range.start;
        let end = range.end;
        let values: Vec<f64> = inner.with_single(|single| {
            let var = single.variable(&dataset_name).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!("missing variable {dataset_name}"))
            })?;
            let extents: Extents = [start..end, col_index..col_index + 1].as_slice().into();
            var.get_values::<f64, _>(extents).map_err(map_nc)
        })?;
        ArrayD::from_shape_vec(vec![len], values).map_err(|e| {
            TimeSeriesError::IntegrityError(format!("shape mismatch on slice: {e}"))
        })
    }

    fn remove_array(&mut self, hash: &[u8; 32]) -> Result<()> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        let entry = inner.index.by_hash.remove(hash);
        let (dataset_name, col_index) = match entry {
            Some(v) => v,
            None => return Ok(()),
        };
        let length;
        let hash_name;
        {
            let state = inner
                .datasets
                .get_mut(&dataset_name)
                .ok_or_else(|| TimeSeriesError::IntegrityError(format!(
                    "dataset {dataset_name} missing from state"
                )))?;
            length = state.length;
            hash_name = state.hash_name.clone();
            if col_index < state.columns.len() {
                state.columns[col_index] = None;
            }
        }
        let dataset_name_owned = dataset_name.clone();
        let hash_name_owned = hash_name.clone();
        inner.with_single_mut(move |single| {
            let mut hash_var = single.variable_mut(&hash_name_owned).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!(
                    "missing variable {hash_name_owned}"
                ))
            })?;
            hash_var.put_string("", col_index).map_err(map_nc)?;
            drop(hash_var);

            let zeros = vec![0.0_f64; length];
            let mut data_var = single.variable_mut(&dataset_name_owned).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!(
                    "missing variable {dataset_name_owned}"
                ))
            })?;
            let extents: Extents =
                [0..length, col_index..col_index + 1].as_slice().into();
            data_var.put_values(&zeros, extents).map_err(map_nc)?;
            Ok(())
        })?;
        Ok(())
    }

    fn contains(&self, hash: &[u8; 32]) -> Result<bool> {
        let inner = self.inner.lock().expect("mutex poisoned");
        Ok(inner.index.by_hash.contains_key(hash))
    }

    fn compact(&mut self) -> Result<CompactionReport> {
        // v0: count tombstones; reusing slots happens automatically on subsequent
        // puts via `first_free`. Truly shrinking dimensions requires recreating
        // datasets, which netcdf-c doesn't do in-place — that's a follow-up.
        let inner = self.inner.lock().expect("mutex poisoned");
        let reclaimed = inner
            .datasets
            .values()
            .map(|s| s.columns.iter().filter(|c| c.is_none()).count())
            .sum();
        Ok(CompactionReport {
            slots_reclaimed: reclaimed,
            datasets_dropped: 0,
        })
    }

    fn verify(&self) -> Result<IntegrityReport> {
        let inner = self.inner.lock().expect("mutex poisoned");
        let mut errors = Vec::new();
        let entries: Vec<([u8; 32], String, usize, usize)> = inner
            .index
            .by_hash
            .iter()
            .map(|(h, (name, col))| {
                let length = inner.datasets.get(name).map(|d| d.length).unwrap_or(0);
                (*h, name.clone(), *col, length)
            })
            .collect();
        for (hash, name, col, length) in entries {
            let res: Result<Vec<f64>> = inner.with_single(|single| {
                let var = single.variable(&name).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("missing variable {name}"))
                })?;
                let extents: Extents = [0..length, col..col + 1].as_slice().into();
                var.get_values::<f64, _>(extents).map_err(map_nc)
            });
            match res {
                Ok(values) => match ArrayD::from_shape_vec(vec![length], values) {
                    Ok(arr) => {
                        let recomputed = array_hash(&arr);
                        if recomputed != hash {
                            errors.push(format!(
                                "hash mismatch in {name}[{col}]: stored={} computed={}",
                                hash_hex(&hash),
                                hash_hex(&recomputed),
                            ));
                        }
                    }
                    Err(e) => errors.push(format!("shape mismatch: {e}")),
                },
                Err(e) => errors.push(format!("read error in {name}[{col}]: {e}")),
            }
        }
        Ok(IntegrityReport { errors })
    }

    fn flush(&mut self) -> Result<()> {
        // `nc_sync` flushes buffered writes to disk so the file can be copied
        // for persistence without closing the handle.
        let inner = self.inner.lock().unwrap();
        inner.file.sync().map_err(map_nc)
    }
}
