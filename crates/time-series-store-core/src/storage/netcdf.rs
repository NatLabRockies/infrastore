//! NetCDF4-backed storage backend.
//!
//! Two storage modes, both natively typed + fixed-dimension:
//!
//! * **Packed** (`packed = true`, used for SingleTimeSeries and the underlying
//!   array of a DeterministicSingleTimeSeries): many same-shaped arrays are
//!   column-packed into a dataset `sts_{dtype}_{shape}_{length}_{res}` of shape
//!   `(length, cols, *element_shape)`, chunked `(1, cols, *element_shape)` — one
//!   timestamp row across every column per chunk, so a read-by-timestamp gathers
//!   one chunk. `cols` is sized per dataset to the batch that created it (capped
//!   so a chunk stays within a byte budget); a group spills into a new dataset
//!   once full. A companion string variable `{name}_h` holds the per-column hex
//!   hash (empty = free slot). Removal frees a slot; `compact` is a stub because
//!   NetCDF can't shrink in place.
//!
//! * **Standalone** (`packed = false`, used for NonSequentialTimeSeries and
//!   native forecasts): each array is its own typed multi-dim variable `arr_{hexhash}`
//!   of shape `[length, k1, ...]`. Removal drops it from the index (the variable
//!   lingers as dead space until `compact`, since NetCDF can't delete variables).
//!
//! `shape` encodes the element shape: `s` = scalar, `3` = `[3]`, `3x2` = `[3, 2]`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use netcdf::{Extents, FileMut, Group, GroupMut};

use crate::error::{Result, TimeSeriesError};
use crate::hash::{array_hash, hash_hex};
use crate::storage::Compression;
use crate::types::array::{Dtype, TypedArray};
use crate::types::period::Period;
use crate::version::DATA_FORMAT_VERSION;

use super::{CompactionReport, IntegrityReport, StorageBackend};

/// Default column width for a packed dataset created by an un-managed
/// (one-at-a-time) write. A buffered bulk-add sizes its datasets to the batch
/// instead; either way a group spills into a new dataset once full.
pub const DEFAULT_COLS_PER_DATASET: usize = 1000;

/// Target upper bound on the bytes of one packed chunk. A dataset is chunked
/// `(1, cols, *element_shape)` — one timestamp row across every column — so the
/// column count is capped to keep that chunk at or below this budget. Batches
/// wider than the cap spill into additional datasets.
const MAX_CHUNK_BYTES: usize = 1 << 20; // 1 MiB

/// Bytes in one column's element block at a single timestep.
fn element_block_bytes(dtype: Dtype, element_shape: &[usize]) -> usize {
    element_shape.iter().product::<usize>() * dtype.size()
}

/// Resolve a packed dataset's column width: the `requested` count (defaulting to
/// [`DEFAULT_COLS_PER_DATASET`] for un-managed writes) clamped to at least one
/// column and to the [`MAX_CHUNK_BYTES`] budget, so a `(1, cols, *element_shape)`
/// timestamp-row chunk stays bounded regardless of dtype or element shape.
fn resolve_dataset_cols(requested: Option<usize>, dtype: Dtype, element_shape: &[usize]) -> usize {
    let block = element_block_bytes(dtype, element_shape).max(1);
    let cap = (MAX_CHUNK_BYTES / block).max(1);
    requested.unwrap_or(DEFAULT_COLS_PER_DATASET).clamp(1, cap)
}

const ROOT_GROUP: &str = "time_series";
const SINGLE_GROUP: &str = "single";
const HASH_SUFFIX: &str = "_h";
const STANDALONE_PREFIX: &str = "arr_";
/// Global attribute recording the compression policy a store was created with.
const COMPRESSION_ATTR: &str = "compression";

#[derive(Debug, Clone)]
enum Location {
    Packed { dataset: String, col: usize },
    Standalone { var: String },
}

#[derive(Debug, Clone)]
struct DatasetState {
    hash_name: String,
    dtype: Dtype,
    element_shape: Vec<usize>,
    length: usize,
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

fn encode_shape(element_shape: &[usize]) -> String {
    if element_shape.is_empty() {
        "s".to_string()
    } else {
        element_shape
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join("x")
    }
}

fn decode_shape(s: &str) -> Result<Vec<usize>> {
    if s == "s" {
        return Ok(Vec::new());
    }
    s.split('x')
        .map(|p| {
            p.parse::<usize>()
                .map_err(|_| TimeSeriesError::IntegrityError(format!("bad element shape '{s}'")))
        })
        .collect()
}

fn dataset_base_name(
    dtype: Dtype,
    element_shape: &[usize],
    length: usize,
    resolution: Period,
) -> String {
    // The resolution is the ISO-8601 duration (e.g. `PT1H`, `P1M`, `P1Y`); it
    // contains no `_`, so the `splitn(4, '_')` parser below stays unambiguous.
    format!(
        "sts_{}_{}_{}_{}",
        dtype.as_str(),
        encode_shape(element_shape),
        length,
        resolution.to_iso8601()
    )
}

fn spill_name(base: &str, n: usize) -> String {
    if n == 0 {
        base.to_string()
    } else {
        format!("{base}__{n}")
    }
}

fn parse_dataset_name(name: &str) -> Result<(Dtype, Vec<usize>, usize, Period)> {
    let core = name.strip_prefix("sts_").ok_or_else(|| {
        TimeSeriesError::IntegrityError(format!("dataset {name} missing 'sts_' prefix"))
    })?;
    let core = core.split("__").next().unwrap();
    let parts: Vec<&str> = core.splitn(4, '_').collect();
    if parts.len() != 4 {
        return Err(TimeSeriesError::IntegrityError(format!(
            "bad dataset name {name}"
        )));
    }
    let dtype = Dtype::parse(parts[0])
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("bad dtype in {name}")))?;
    let element_shape = decode_shape(parts[1])?;
    let length: usize = parts[2]
        .parse()
        .map_err(|_| TimeSeriesError::IntegrityError(format!("bad length in {name}")))?;
    let resolution = Period::from_iso8601(parts[3])
        .map_err(|_| TimeSeriesError::IntegrityError(format!("bad resolution in {name}")))?;
    Ok((dtype, element_shape, length, resolution))
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

// ---- byte <-> typed Vec conversions ---------------------------------------

macro_rules! vec_from_le {
    ($bytes:expr, $t:ty, $n:expr) => {
        $bytes
            .chunks_exact($n)
            .map(|c| <$t>::from_le_bytes(c.try_into().unwrap()))
            .collect::<Vec<$t>>()
    };
}

macro_rules! le_from_vec {
    ($vals:expr) => {
        $vals
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<u8>>()
    };
}

fn put_typed(
    var: &mut netcdf::VariableMut<'_>,
    dtype: Dtype,
    bytes: &[u8],
    extents: Extents,
) -> Result<()> {
    match dtype {
        Dtype::F64 => var.put_values(&vec_from_le!(bytes, f64, 8), extents),
        Dtype::F32 => var.put_values(&vec_from_le!(bytes, f32, 4), extents),
        Dtype::I64 => var.put_values(&vec_from_le!(bytes, i64, 8), extents),
        Dtype::I32 => var.put_values(&vec_from_le!(bytes, i32, 4), extents),
        Dtype::U64 => var.put_values(&vec_from_le!(bytes, u64, 8), extents),
        Dtype::Bool => var.put_values(bytes, extents),
    }
    .map_err(map_nc)
}

fn get_typed(var: &netcdf::Variable<'_>, dtype: Dtype, extents: Extents) -> Result<Vec<u8>> {
    Ok(match dtype {
        Dtype::F64 => le_from_vec!(var.get_values::<f64, _>(extents).map_err(map_nc)?),
        Dtype::F32 => le_from_vec!(var.get_values::<f32, _>(extents).map_err(map_nc)?),
        Dtype::I64 => le_from_vec!(var.get_values::<i64, _>(extents).map_err(map_nc)?),
        Dtype::I32 => le_from_vec!(var.get_values::<i32, _>(extents).map_err(map_nc)?),
        Dtype::U64 => le_from_vec!(var.get_values::<u64, _>(extents).map_err(map_nc)?),
        Dtype::Bool => var.get_values::<u8, _>(extents).map_err(map_nc)?,
    })
}

fn add_typed_variable(
    single: &mut GroupMut<'_>,
    name: &str,
    dtype: Dtype,
    dim_names: &[&str],
    chunks: &[usize],
    compression: Compression,
) -> Result<()> {
    macro_rules! add_var {
        ($t:ty) => {{
            let mut var = single.add_variable::<$t>(name, dim_names).map_err(map_nc)?;
            var.set_chunking(chunks).map_err(map_nc)?;
            if let Compression::Deflate { level, shuffle } = compression {
                var.set_compression(level as _, shuffle).map_err(map_nc)?;
            }
        }};
    }
    match dtype {
        Dtype::F64 => add_var!(f64),
        Dtype::F32 => add_var!(f32),
        Dtype::I64 => add_var!(i64),
        Dtype::I32 => add_var!(i32),
        Dtype::U64 => add_var!(u64),
        Dtype::Bool => add_var!(u8),
    }
    Ok(())
}

pub struct NetCdfBackend {
    inner: Mutex<Inner>,
}

/// Key grouping packed datasets that can hold the same arrays:
/// (dtype, element_shape, length, resolution).
type DatasetGroupKey = (Dtype, Vec<usize>, usize, Period);

struct Inner {
    file: FileMut,
    path: PathBuf,
    datasets: HashMap<String, DatasetState>,
    /// Packed dataset names per group key, kept sorted by name so writers
    /// prefer the earliest spill with a free slot. Avoids scanning every
    /// dataset on each put.
    dataset_groups: HashMap<DatasetGroupKey, Vec<String>>,
    standalone_vars: HashSet<String>,
    by_hash: HashMap<[u8; 32], Location>,
    compression: Compression,
}

impl NetCdfBackend {
    pub fn create(path: &Path, compression: Compression) -> Result<Self> {
        let mut file = netcdf::create(path).map_err(map_nc)?;
        file.add_attribute("data_format_version", DATA_FORMAT_VERSION)
            .map_err(map_nc)?;
        file.add_attribute(COMPRESSION_ATTR, compression.encode())
            .map_err(map_nc)?;
        {
            let mut ts = file.add_group(ROOT_GROUP).map_err(map_nc)?;
            let _ = ts.add_group(SINGLE_GROUP).map_err(map_nc)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                file,
                path: path.to_path_buf(),
                datasets: HashMap::new(),
                dataset_groups: HashMap::new(),
                standalone_vars: HashSet::new(),
                by_hash: HashMap::new(),
                compression,
            }),
        })
    }

    pub fn open(path: &Path) -> Result<Self> {
        let file = netcdf::append(path).map_err(map_nc)?;
        // Refuse a store written in a different on-disk format. `DATA_FORMAT_VERSION`
        // is bumped only for backward-incompatible changes, so any mismatch means
        // this build cannot read the file correctly — exact equality is the test.
        // Failing here yields a clear diagnostic instead of a downstream surprise
        // (a missing SQLite table, or worse, a plausible-but-wrong read).
        let found = match file.attribute("data_format_version").map(|a| a.value()) {
            Some(Ok(netcdf::AttributeValue::Str(s))) => s,
            // Predates the attribute entirely, so certainly not the current format.
            _ => "unspecified".to_string(),
        };
        if found != DATA_FORMAT_VERSION {
            return Err(TimeSeriesError::IncompatibleFormat {
                found,
                expected: DATA_FORMAT_VERSION,
            });
        }
        // Restore the compression policy the store was created with so that
        // appended arrays reuse the same filter. Legacy stores without the
        // attribute fall back to the historical default.
        let compression = match file.attribute(COMPRESSION_ATTR).map(|a| a.value()) {
            Some(Ok(netcdf::AttributeValue::Str(s))) => Compression::decode(&s),
            _ => Compression::default(),
        };
        let mut backend = Self {
            inner: Mutex::new(Inner {
                file,
                path: path.to_path_buf(),
                datasets: HashMap::new(),
                dataset_groups: HashMap::new(),
                standalone_vars: HashSet::new(),
                by_hash: HashMap::new(),
                compression,
            }),
        };
        backend.rebuild_index()?;
        Ok(backend)
    }

    pub fn path(&self) -> PathBuf {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.path.clone()
    }

    pub fn compression(&self) -> Compression {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.compression
    }

    #[tracing::instrument(skip(self))]
    fn rebuild_index(&mut self) -> Result<()> {
        let inner = self.inner.get_mut().expect("mutex poisoned");

        // Collect variable names + their column counts (for packed datasets).
        struct Row {
            name: String,
            num_cols: usize,
            standalone: bool,
        }
        let rows: Vec<Row> = inner.with_single(|single| {
            let mut out = Vec::new();
            for var in single.variables() {
                let name = var.name();
                if name.ends_with(HASH_SUFFIX) {
                    continue;
                }
                if let Some(rest) = name.strip_prefix(STANDALONE_PREFIX) {
                    let _ = rest;
                    out.push(Row {
                        name,
                        num_cols: 0,
                        standalone: true,
                    });
                } else {
                    let dims = var.dimensions();
                    out.push(Row {
                        name,
                        num_cols: dims.get(1).map(|d| d.len()).unwrap_or(0),
                        standalone: false,
                    });
                }
            }
            Ok(out)
        })?;

        for row in rows {
            if row.standalone {
                let hex = row.name.strip_prefix(STANDALONE_PREFIX).unwrap();
                let hash = hex_to_hash(hex)?;
                inner.standalone_vars.insert(row.name.clone());
                inner
                    .by_hash
                    .insert(hash, Location::Standalone { var: row.name });
                continue;
            }

            let (dtype, element_shape, length, resolution_ms) = parse_dataset_name(&row.name)?;
            let hash_name = format!("{}{}", row.name, HASH_SUFFIX);
            let hash_strings: Vec<String> = inner.with_single(|single| {
                let v = single.variable(&hash_name).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("missing hash variable {hash_name}"))
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
                    let hash = hex_to_hash(&s)?;
                    inner.by_hash.insert(
                        hash,
                        Location::Packed {
                            dataset: row.name.clone(),
                            col: i,
                        },
                    );
                    columns.push(Some(s));
                }
            }
            inner
                .dataset_groups
                .entry((dtype, element_shape.clone(), length, resolution_ms))
                .or_default()
                .push(row.name.clone());
            inner.datasets.insert(
                row.name.clone(),
                DatasetState {
                    hash_name,
                    dtype,
                    element_shape,
                    length,
                    columns,
                },
            );
        }
        // Variable iteration order is not guaranteed; sort each group so
        // writers fill the lexicographically-first spill first (matching the
        // historical scan-and-sort behaviour).
        for names in inner.dataset_groups.values_mut() {
            names.sort();
        }
        Ok(())
    }
}

impl Inner {
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

    fn ensure_writable_dataset(
        &mut self,
        dtype: Dtype,
        element_shape: &[usize],
        length: usize,
        resolution: Period,
    ) -> Result<String> {
        let key = (dtype, element_shape.to_vec(), length, resolution);
        let mut spill_count = 0;
        if let Some(names) = self.dataset_groups.get(&key) {
            spill_count = names.len();
            for name in names {
                if let Some(state) = self.datasets.get(name)
                    && !state.full()
                {
                    return Ok(name.clone());
                }
            }
        }
        let base = dataset_base_name(dtype, element_shape, length, resolution);
        let new_name = spill_name(&base, spill_count);
        // Un-managed (single) writes use the default width; the buffered bulk-add
        // path sizes datasets to the batch via its own creation route.
        self.create_dataset(&new_name, dtype, element_shape, length, resolution, None)?;
        Ok(new_name)
    }

    fn create_dataset(
        &mut self,
        name: &str,
        dtype: Dtype,
        element_shape: &[usize],
        length: usize,
        resolution: Period,
        requested_cols: Option<usize>,
    ) -> Result<()> {
        let cols = resolve_dataset_cols(requested_cols, dtype, element_shape);
        let dim_time = format!("{name}_t");
        let dim_col = format!("{name}_c");
        let elem_dims: Vec<(String, usize)> = element_shape
            .iter()
            .enumerate()
            .map(|(i, &sz)| (format!("{name}_e{i}"), sz))
            .collect();
        let hash_name = format!("{name}{HASH_SUFFIX}");
        let name_owned = name.to_string();
        let hash_name_for_closure = hash_name.clone();
        // Chunk one timestamp row across every column: `(1, cols, *element_shape)`.
        // This makes a read-by-timestamp gather one whole chunk and a buffered
        // full-width bulk write fill whole chunks; a single-column write becomes a
        // read-modify-write of every time-band chunk (the accepted un-managed cost).
        let chunks: Vec<usize> = std::iter::once(1usize)
            .chain(std::iter::once(cols))
            .chain(element_shape.iter().copied())
            .collect();
        let compression = self.compression;

        self.with_single_mut(|single| {
            single.add_dimension(&dim_time, length).map_err(map_nc)?;
            single.add_dimension(&dim_col, cols).map_err(map_nc)?;
            for (dn, sz) in &elem_dims {
                single.add_dimension(dn, *sz).map_err(map_nc)?;
            }
            let mut dim_names: Vec<&str> = vec![&dim_time, &dim_col];
            for (dn, _) in &elem_dims {
                dim_names.push(dn);
            }
            add_typed_variable(single, &name_owned, dtype, &dim_names, &chunks, compression)?;
            let _h = single
                .add_string_variable(&hash_name_for_closure, &[&dim_col])
                .map_err(map_nc)?;
            Ok(())
        })?;

        self.datasets.insert(
            name.to_string(),
            DatasetState {
                hash_name,
                dtype,
                element_shape: element_shape.to_vec(),
                length,
                columns: vec![None; cols],
            },
        );
        let group = self
            .dataset_groups
            .entry((dtype, element_shape.to_vec(), length, resolution))
            .or_default();
        let pos = group
            .binary_search_by(|n| n.as_str().cmp(name))
            .unwrap_or_else(|p| p);
        group.insert(pos, name.to_string());
        Ok(())
    }

    fn packed_extents(time: Range<usize>, col: usize, element_shape: &[usize]) -> Extents {
        let mut ranges: Vec<Range<usize>> = vec![time, col..col + 1];
        for &k in element_shape {
            ranges.push(0..k);
        }
        ranges.as_slice().into()
    }

    /// Extents selecting a single time index across the first `width` columns of
    /// a packed dataset: `[idx, 0..width, *element_shape]`. One hyperslab feeds a
    /// timestamp row to the [`StorageBackend::read_index_into`] override; `width`
    /// is bounded to the highest column that read actually gathers.
    fn packed_row_extents(time_index: usize, width: usize, element_shape: &[usize]) -> Extents {
        let mut ranges: Vec<Range<usize>> = vec![time_index..time_index + 1, 0..width];
        for &k in element_shape {
            ranges.push(0..k);
        }
        ranges.as_slice().into()
    }

    /// Extents selecting the full time axis across the first `width` columns of a
    /// packed dataset: `[0..length, 0..width, *element_shape]`. One hyperslab feeds
    /// a whole column span to the [`StorageBackend::read_arrays`] override, which
    /// then scatters individual series out of the row-major block.
    fn packed_block_extents(length: usize, width: usize, element_shape: &[usize]) -> Extents {
        let mut ranges: Vec<Range<usize>> = vec![0..length, 0..width];
        for &k in element_shape {
            ranges.push(0..k);
        }
        ranges.as_slice().into()
    }

    fn standalone_extents(time: Range<usize>, element_shape: &[usize]) -> Extents {
        let mut ranges: Vec<Range<usize>> = vec![time];
        for &k in element_shape {
            ranges.push(0..k);
        }
        ranges.as_slice().into()
    }

    #[tracing::instrument(skip(self, hash, data), fields(bytes = data.bytes.len()))]
    fn put_packed(&mut self, hash: &[u8; 32], data: &TypedArray, resolution: Period) -> Result<()> {
        let length = data.length();
        let element_shape = data.element_shape().to_vec();
        let dtype = data.dtype;

        let dataset_name =
            self.ensure_writable_dataset(dtype, &element_shape, length, resolution)?;
        let (col_index, hash_name) = {
            let state = self.datasets.get(&dataset_name).expect("dataset ensured");
            let col = state.first_free().ok_or_else(|| {
                TimeSeriesError::IntegrityError("no free slot in newly-ensured dataset".into())
            })?;
            (col, state.hash_name.clone())
        };

        let hex = hash_hex(hash);
        self.with_single_mut(|single| {
            let mut data_var = single.variable_mut(&dataset_name).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!("missing variable {dataset_name}"))
            })?;
            let extents = Inner::packed_extents(0..length, col_index, &element_shape);
            put_typed(&mut data_var, dtype, &data.bytes, extents)?;
            drop(data_var);
            let mut hash_var = single.variable_mut(&hash_name).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!("missing variable {hash_name}"))
            })?;
            hash_var.put_string(&hex, col_index).map_err(map_nc)?;
            Ok(())
        })?;
        self.datasets
            .get_mut(&dataset_name)
            .expect("dataset ensured")
            .columns[col_index] = Some(hex);
        self.by_hash.insert(
            *hash,
            Location::Packed {
                dataset: dataset_name,
                col: col_index,
            },
        );
        Ok(())
    }

    /// Write a block of same-shaped packed arrays into one or more freshly
    /// created, batch-sized datasets. Skips hashes already stored and duplicates
    /// within the block; returns a per-input flag (`true` = physically written).
    ///
    /// Each created dataset is sized to the block (capped so a `(1, cols, *elem)`
    /// chunk stays within [`MAX_CHUNK_BYTES`]); a block wider than the cap spills
    /// across consecutive datasets. Columns are written one timestamp row at a
    /// time, so every write covers exactly one full chunk — no read-modify-write.
    #[tracing::instrument(skip(self, hashes, arrays), fields(n = hashes.len()))]
    fn put_packed_block(
        &mut self,
        hashes: &[[u8; 32]],
        arrays: &[&TypedArray],
        resolution: Period,
    ) -> Result<Vec<bool>> {
        let mut written = vec![false; hashes.len()];

        // Keep only inputs that are new on disk and unique within the block,
        // preserving order. `new[k] = (original_index, hash, array)`.
        let mut seen: HashSet<[u8; 32]> = HashSet::new();
        let mut new: Vec<(usize, [u8; 32], &TypedArray)> = Vec::with_capacity(hashes.len());
        for (i, (&hash, &array)) in hashes.iter().zip(arrays).enumerate() {
            if self.by_hash.contains_key(&hash) || !seen.insert(hash) {
                continue;
            }
            new.push((i, hash, array));
        }
        if new.is_empty() {
            return Ok(written);
        }

        let dtype = new[0].2.dtype;
        let element_shape = new[0].2.element_shape().to_vec();
        let length = new[0].2.length();
        let block = element_block_bytes(dtype, &element_shape);
        let group_key = (dtype, element_shape.clone(), length, resolution);

        let mut start = 0;
        while start < new.len() {
            let remaining = new.len() - start;
            // Width of this dataset: the rest of the block, capped to the chunk
            // budget. `resolve_dataset_cols(Some(remaining))` == min(remaining, cap).
            let width = resolve_dataset_cols(Some(remaining), dtype, &element_shape);
            let seg = &new[start..start + width];

            let base = dataset_base_name(dtype, &element_shape, length, resolution);
            let spill_count = self.dataset_groups.get(&group_key).map_or(0, Vec::len);
            let name = spill_name(&base, spill_count);
            self.create_dataset(
                &name,
                dtype,
                &element_shape,
                length,
                resolution,
                Some(width),
            )?;
            let hash_name = format!("{name}{HASH_SUFFIX}");

            self.with_single_mut(|single| {
                let mut data_var = single.variable_mut(&name).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("missing variable {name}"))
                })?;
                // One full-chunk write per timestep: gather column c's element block
                // at time t into the row buffer, laid out `[col, *element_shape]`.
                let mut row = vec![0u8; width * block];
                for t in 0..length {
                    for (c, (_, _, array)) in seg.iter().enumerate() {
                        let src = &array.bytes[t * block..(t + 1) * block];
                        row[c * block..(c + 1) * block].copy_from_slice(src);
                    }
                    let extents = Inner::packed_row_extents(t, width, &element_shape);
                    put_typed(&mut data_var, dtype, &row, extents)?;
                }
                drop(data_var);
                let mut hash_var = single.variable_mut(&hash_name).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("missing variable {hash_name}"))
                })?;
                for (c, (_, hash, _)) in seg.iter().enumerate() {
                    hash_var.put_string(&hash_hex(hash), c).map_err(map_nc)?;
                }
                Ok(())
            })?;

            let state = self.datasets.get_mut(&name).expect("dataset created");
            for (c, (_, hash, _)) in seg.iter().enumerate() {
                state.columns[c] = Some(hash_hex(hash));
            }
            for (c, (orig, hash, _)) in seg.iter().enumerate() {
                self.by_hash.insert(
                    *hash,
                    Location::Packed {
                        dataset: name.clone(),
                        col: c,
                    },
                );
                written[*orig] = true;
            }
            start += width;
        }
        Ok(written)
    }

    #[tracing::instrument(skip(self, hash, data), fields(bytes = data.bytes.len()))]
    fn put_standalone(&mut self, hash: &[u8; 32], data: &TypedArray) -> Result<()> {
        let var = format!("{STANDALONE_PREFIX}{}", hash_hex(hash));
        // If the variable already exists (live or tombstoned), the content is
        // identical (content-addressed); just (re)index it.
        if self.standalone_vars.contains(&var) {
            self.by_hash.insert(*hash, Location::Standalone { var });
            return Ok(());
        }
        let compression = self.compression;
        self.with_single_mut(|single| {
            let dims: Vec<(String, usize)> = data
                .shape
                .iter()
                .enumerate()
                .map(|(i, &sz)| (format!("{var}_d{i}"), sz))
                .collect();
            for (dn, sz) in &dims {
                single.add_dimension(dn, *sz).map_err(map_nc)?;
            }
            let dim_names: Vec<&str> = dims.iter().map(|(n, _)| n.as_str()).collect();
            add_typed_variable(
                single,
                &var,
                data.dtype,
                &dim_names,
                &data.shape,
                compression,
            )?;
            let mut v = single.variable_mut(&var).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!("missing variable {var}"))
            })?;
            put_typed(&mut v, data.dtype, &data.bytes, Extents::All)?;
            Ok(())
        })?;
        self.standalone_vars.insert(var.clone());
        self.by_hash.insert(*hash, Location::Standalone { var });
        Ok(())
    }

    #[tracing::instrument(skip(self, hash, range))]
    fn read_locked(&self, hash: &[u8; 32], range: Option<Range<usize>>) -> Result<TypedArray> {
        let loc = self
            .by_hash
            .get(hash)
            .ok_or(TimeSeriesError::NotFound)?
            .clone();
        match loc {
            Location::Packed { dataset, col } => {
                let state = self.datasets.get(&dataset).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("dataset {dataset} missing"))
                })?;
                let total = state.length;
                let range = range.unwrap_or(0..total);
                if range.start > range.end || range.end > total {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "slice {:?} out of bounds for length {}",
                        range, total
                    )));
                }
                let out_len = range.end - range.start;
                let dtype = state.dtype;
                let element_shape = state.element_shape.clone();
                let bytes = self.with_single(|single| {
                    let var = single.variable(&dataset).ok_or_else(|| {
                        TimeSeriesError::IntegrityError(format!("missing variable {dataset}"))
                    })?;
                    let extents = Inner::packed_extents(range.clone(), col, &element_shape);
                    get_typed(&var, dtype, extents)
                })?;
                let mut shape = vec![out_len];
                shape.extend_from_slice(&element_shape);
                TypedArray::new(dtype, shape, bytes).map_err(TimeSeriesError::IntegrityError)
            }
            Location::Standalone { var } => self.with_single(|single| {
                let v = single.variable(&var).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("missing variable {var}"))
                })?;
                let full_shape: Vec<usize> = v.dimensions().iter().map(|d| d.len()).collect();
                let dtype = dtype_of_variable(&v)?;
                let total = full_shape.first().copied().unwrap_or(0);
                let range = range.unwrap_or(0..total);
                if range.start > range.end || range.end > total {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "slice {:?} out of bounds for length {}",
                        range, total
                    )));
                }
                let element_shape = &full_shape[1.min(full_shape.len())..];
                let extents = Inner::standalone_extents(range.clone(), element_shape);
                let bytes = get_typed(&v, dtype, extents)?;
                let mut shape = vec![range.end - range.start];
                shape.extend_from_slice(element_shape);
                TypedArray::new(dtype, shape, bytes).map_err(TimeSeriesError::IntegrityError)
            }),
        }
    }

    /// Read the value at a single time `index` for a set of packed arrays,
    /// appending each array's element block to `out` in `hashes` order.
    ///
    /// Backs the [`StorageBackend::read_index_into`] override: hashes are grouped
    /// by their physical dataset so each dataset's timestamp row is read with one
    /// hyperslab, then the requested columns are gathered. A `StaticReader`
    /// supplies same-shape packed hashes; any standalone hash (not expected)
    /// falls back to an individual single-step read.
    fn read_index_locked(
        &self,
        hashes: &[[u8; 32]],
        index: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        out.clear();
        if hashes.is_empty() {
            return Ok(());
        }

        enum Placement {
            Packed { dataset: String, col: usize },
            Standalone { hash: [u8; 32] },
        }

        // Resolve placements; track the highest column needed per dataset so the
        // row read is bounded to `[0, max_col]` rather than the full column
        // dimension (which is sized per dataset to the batch that created it).
        let mut placements: Vec<Placement> = Vec::with_capacity(hashes.len());
        let mut max_col: HashMap<String, usize> = HashMap::new();
        for hash in hashes {
            match self.by_hash.get(hash).ok_or(TimeSeriesError::NotFound)? {
                Location::Packed { dataset, col } => {
                    max_col
                        .entry(dataset.clone())
                        .and_modify(|m| *m = (*m).max(*col))
                        .or_insert(*col);
                    placements.push(Placement::Packed {
                        dataset: dataset.clone(),
                        col: *col,
                    });
                }
                Location::Standalone { .. } => {
                    placements.push(Placement::Standalone { hash: *hash });
                }
            }
        }

        // One hyperslab per dataset: `[idx, 0..=max_col, *element_shape]`.
        // Unneeded columns below `max_col` are read and discarded, but the read
        // never spans more columns than the highest one this call gathers.
        let mut rows: HashMap<String, (Vec<u8>, usize)> = HashMap::new();
        for (dataset, &top) in &max_col {
            let state = self.datasets.get(dataset).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!("dataset {dataset} missing"))
            })?;
            if index >= state.length {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "index {index} out of bounds for dataset {dataset} length {}",
                    state.length
                )));
            }
            let width = top + 1;
            let dtype = state.dtype;
            let element_shape = state.element_shape.clone();
            let elem_bytes = element_shape.iter().product::<usize>().max(1) * dtype.size();
            let bytes = self.with_single(|single| {
                let var = single.variable(dataset).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("missing variable {dataset}"))
                })?;
                let extents = Inner::packed_row_extents(index, width, &element_shape);
                get_typed(&var, dtype, extents)
            })?;
            rows.insert(dataset.clone(), (bytes, elem_bytes));
        }

        // Gather each column's block into `out`, in the caller's hash order.
        for placement in &placements {
            match placement {
                Placement::Packed { dataset, col } => {
                    let (row, elem_bytes) = rows.get(dataset).expect("dataset row read above");
                    let start = col * elem_bytes;
                    let block = row.get(start..start + elem_bytes).ok_or_else(|| {
                        TimeSeriesError::IntegrityError(format!(
                            "column {col} out of row bounds for dataset {dataset}"
                        ))
                    })?;
                    out.extend_from_slice(block);
                }
                Placement::Standalone { hash } => {
                    let arr = self.read_locked(hash, Some(index..index + 1))?;
                    out.extend_from_slice(&arr.bytes);
                }
            }
        }
        Ok(())
    }

    /// Read many full arrays at once, returning one [`TypedArray`] per input hash
    /// in order. Backs [`StorageBackend::read_arrays`]: packed hashes are grouped
    /// by dataset and that dataset's whole column span is read with one hyperslab
    /// (`[0..length, 0..=max_col, *element_shape]`), decompressing each timestamp-
    /// major chunk once; each requested series is then gathered out of the
    /// row-major block. Standalone hashes fall back to an individual read.
    fn read_arrays_locked(&self, hashes: &[[u8; 32]]) -> Result<Vec<TypedArray>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        enum Placement {
            Packed { dataset: String, col: usize },
            Standalone { hash: [u8; 32] },
        }

        // Resolve placements; bound each dataset read to its highest needed column.
        let mut placements: Vec<Placement> = Vec::with_capacity(hashes.len());
        let mut max_col: HashMap<String, usize> = HashMap::new();
        for hash in hashes {
            match self.by_hash.get(hash).ok_or(TimeSeriesError::NotFound)? {
                Location::Packed { dataset, col } => {
                    max_col
                        .entry(dataset.clone())
                        .and_modify(|m| *m = (*m).max(*col))
                        .or_insert(*col);
                    placements.push(Placement::Packed {
                        dataset: dataset.clone(),
                        col: *col,
                    });
                }
                Location::Standalone { .. } => {
                    placements.push(Placement::Standalone { hash: *hash });
                }
            }
        }

        // One hyperslab per dataset: the full time axis across `[0, max_col]`.
        struct DatasetRead {
            bytes: Vec<u8>,
            width: usize,
            length: usize,
            dtype: Dtype,
            element_shape: Vec<usize>,
            elem_bytes: usize,
        }
        let mut reads: HashMap<String, DatasetRead> = HashMap::new();
        for (dataset, &top) in &max_col {
            let state = self.datasets.get(dataset).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!("dataset {dataset} missing"))
            })?;
            let width = top + 1;
            let length = state.length;
            let dtype = state.dtype;
            let element_shape = state.element_shape.clone();
            let elem_bytes = element_shape.iter().product::<usize>().max(1) * dtype.size();
            let bytes = self.with_single(|single| {
                let var = single.variable(dataset).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("missing variable {dataset}"))
                })?;
                let extents = Inner::packed_block_extents(length, width, &element_shape);
                get_typed(&var, dtype, extents)
            })?;
            reads.insert(
                dataset.clone(),
                DatasetRead {
                    bytes,
                    width,
                    length,
                    dtype,
                    element_shape,
                    elem_bytes,
                },
            );
        }

        // Scatter each series out of its dataset block, in the caller's order.
        let mut out: Vec<TypedArray> = Vec::with_capacity(hashes.len());
        for placement in &placements {
            match placement {
                Placement::Packed { dataset, col } => {
                    let r = reads.get(dataset).expect("dataset read above");
                    let mut col_bytes = Vec::with_capacity(r.length * r.elem_bytes);
                    for t in 0..r.length {
                        let start = (t * r.width + col) * r.elem_bytes;
                        let block = r.bytes.get(start..start + r.elem_bytes).ok_or_else(|| {
                            TimeSeriesError::IntegrityError(format!(
                                "column {col} out of block bounds for dataset {dataset}"
                            ))
                        })?;
                        col_bytes.extend_from_slice(block);
                    }
                    let mut shape = vec![r.length];
                    shape.extend_from_slice(&r.element_shape);
                    out.push(
                        TypedArray::new(r.dtype, shape, col_bytes)
                            .map_err(TimeSeriesError::IntegrityError)?,
                    );
                }
                Placement::Standalone { hash } => {
                    out.push(self.read_locked(hash, None)?);
                }
            }
        }
        Ok(out)
    }

    /// The stored `(dtype, shape)` of an array without reading its data.
    fn array_shape_locked(&self, hash: &[u8; 32]) -> Result<(Dtype, Vec<usize>)> {
        let loc = self
            .by_hash
            .get(hash)
            .ok_or(TimeSeriesError::NotFound)?
            .clone();
        match loc {
            Location::Standalone { var } => self.with_single(|single| {
                let v = single.variable(&var).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("missing variable {var}"))
                })?;
                let shape: Vec<usize> = v.dimensions().iter().map(|d| d.len()).collect();
                Ok((dtype_of_variable(&v)?, shape))
            }),
            Location::Packed { dataset, .. } => {
                let state = self.datasets.get(&dataset).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("dataset {dataset} missing"))
                })?;
                let mut shape = vec![state.length];
                shape.extend_from_slice(&state.element_shape);
                Ok((state.dtype, shape))
            }
        }
    }

    /// Read one forecast window with a single hyperslab: the standalone array's
    /// slice at `window_index` along `count_axis`. The selected axis is size 1 in
    /// the read, contributing nothing to the row-major byte layout, so the result
    /// is exactly the window (axis removed). Appends to `out` (cleared first).
    fn read_window_locked(
        &self,
        hash: &[u8; 32],
        count_axis: usize,
        window_index: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        out.clear();
        let loc = self
            .by_hash
            .get(hash)
            .ok_or(TimeSeriesError::NotFound)?
            .clone();
        match loc {
            Location::Standalone { var } => self.with_single(|single| {
                let v = single.variable(&var).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("missing variable {var}"))
                })?;
                let dims: Vec<usize> = v.dimensions().iter().map(|d| d.len()).collect();
                if count_axis >= dims.len() {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "count axis {count_axis} out of bounds for shape {dims:?}"
                    )));
                }
                if window_index >= dims[count_axis] {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "window index {window_index} out of bounds for axis length {}",
                        dims[count_axis]
                    )));
                }
                let dtype = dtype_of_variable(&v)?;
                let ranges: Vec<Range<usize>> = dims
                    .iter()
                    .enumerate()
                    .map(|(i, &len)| {
                        if i == count_axis {
                            window_index..window_index + 1
                        } else {
                            0..len
                        }
                    })
                    .collect();
                let extents: Extents = ranges.as_slice().into();
                let bytes = get_typed(&v, dtype, extents)?;
                out.extend_from_slice(&bytes);
                Ok(())
            }),
            Location::Packed { .. } => Err(TimeSeriesError::IntegrityError(
                "forecast window read expects a standalone array, found a packed one".into(),
            )),
        }
    }
}

/// Map a NetCDF variable's element type back to a [`Dtype`].
fn dtype_of_variable(var: &netcdf::Variable<'_>) -> Result<Dtype> {
    use netcdf::types::{FloatType, IntType, NcVariableType};
    Ok(match var.vartype() {
        NcVariableType::Float(FloatType::F64) => Dtype::F64,
        NcVariableType::Float(FloatType::F32) => Dtype::F32,
        NcVariableType::Int(IntType::I64) => Dtype::I64,
        NcVariableType::Int(IntType::I32) => Dtype::I32,
        NcVariableType::Int(IntType::U64) => Dtype::U64,
        NcVariableType::Int(IntType::U8) => Dtype::Bool,
        other => {
            return Err(TimeSeriesError::IntegrityError(format!(
                "unsupported nc variable type {other:?}"
            )));
        }
    })
}

impl StorageBackend for NetCdfBackend {
    #[tracing::instrument(skip(self, hash, data), fields(bytes = data.bytes.len(), packed))]
    fn put_array(
        &mut self,
        hash: &[u8; 32],
        data: &TypedArray,
        resolution: Period,
        packed: bool,
    ) -> Result<bool> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        if inner.by_hash.contains_key(hash) {
            return Ok(false);
        }
        if packed {
            inner.put_packed(hash, data, resolution)?;
        } else {
            inner.put_standalone(hash, data)?;
        }
        Ok(true)
    }

    #[tracing::instrument(skip(self, hashes, arrays), fields(n = hashes.len()))]
    fn put_packed_block(
        &mut self,
        hashes: &[[u8; 32]],
        arrays: &[&TypedArray],
        resolution: Period,
    ) -> Result<Vec<bool>> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        inner.put_packed_block(hashes, arrays, resolution)
    }

    #[tracing::instrument(skip(self, hash))]
    fn get_array(&self, hash: &[u8; 32]) -> Result<TypedArray> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.read_locked(hash, None)
    }

    #[tracing::instrument(skip(self, hash), fields(start = range.start, end = range.end))]
    fn get_slice(&self, hash: &[u8; 32], range: Range<usize>) -> Result<TypedArray> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.read_locked(hash, Some(range))
    }

    #[tracing::instrument(skip(self, hashes, out), fields(n = hashes.len(), index))]
    fn read_index_into(&self, hashes: &[[u8; 32]], index: usize, out: &mut Vec<u8>) -> Result<()> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.read_index_locked(hashes, index, out)
    }

    #[tracing::instrument(skip(self, hashes), fields(n = hashes.len()))]
    fn read_arrays(&self, hashes: &[[u8; 32]]) -> Result<Vec<TypedArray>> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.read_arrays_locked(hashes)
    }

    fn array_shape(&self, hash: &[u8; 32]) -> Result<(Dtype, Vec<usize>)> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.array_shape_locked(hash)
    }

    #[tracing::instrument(skip(self, hash, out), fields(count_axis, window_index))]
    fn read_window_into(
        &self,
        hash: &[u8; 32],
        count_axis: usize,
        window_index: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.read_window_locked(hash, count_axis, window_index, out)
    }

    fn remove_array(&mut self, hash: &[u8; 32]) -> Result<()> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        let loc = match inner.by_hash.remove(hash) {
            Some(v) => v,
            None => return Ok(()),
        };
        match loc {
            // Standalone: drop from the index; the variable lingers until compact.
            Location::Standalone { .. } => Ok(()),
            Location::Packed { dataset, col } => {
                let (length, hash_name, dtype, element_shape) = {
                    let state = inner.datasets.get_mut(&dataset).ok_or_else(|| {
                        TimeSeriesError::IntegrityError(format!("dataset {dataset} missing"))
                    })?;
                    if col < state.columns.len() {
                        state.columns[col] = None;
                    }
                    (
                        state.length,
                        state.hash_name.clone(),
                        state.dtype,
                        state.element_shape.clone(),
                    )
                };
                let row_elems: usize = element_shape.iter().product::<usize>().max(1);
                let total = length * row_elems;
                inner.with_single_mut(move |single| {
                    let mut hash_var = single.variable_mut(&hash_name).ok_or_else(|| {
                        TimeSeriesError::IntegrityError(format!("missing variable {hash_name}"))
                    })?;
                    hash_var.put_string("", col).map_err(map_nc)?;
                    drop(hash_var);
                    let mut data_var = single.variable_mut(&dataset).ok_or_else(|| {
                        TimeSeriesError::IntegrityError(format!("missing variable {dataset}"))
                    })?;
                    let zeros = vec![0u8; total * dtype.size()];
                    let extents = Inner::packed_extents(0..length, col, &element_shape);
                    put_typed(&mut data_var, dtype, &zeros, extents)?;
                    Ok(())
                })
            }
        }
    }

    fn contains(&self, hash: &[u8; 32]) -> Result<bool> {
        let inner = self.inner.lock().expect("mutex poisoned");
        Ok(inner.by_hash.contains_key(hash))
    }

    fn compact(&mut self) -> Result<CompactionReport> {
        let inner = self.inner.lock().expect("mutex poisoned");
        let reclaimed = inner
            .datasets
            .values()
            .map(|s| s.columns.iter().filter(|c| c.is_none()).count())
            .sum();
        Ok(CompactionReport {
            slots_reclaimed: reclaimed,
            datasets_dropped: 0,
            // The catalog is not the backend's to sweep; `Store::compact` fills
            // this in after the array side is done.
            feature_sets_reclaimed: 0,
        })
    }

    fn verify(&self) -> Result<IntegrityReport> {
        let inner = self.inner.lock().expect("mutex poisoned");
        let mut errors = Vec::new();
        let hashes: Vec<[u8; 32]> = inner.by_hash.keys().copied().collect();
        for hash in hashes {
            match inner.read_locked(&hash, None) {
                Ok(arr) => {
                    let recomputed = array_hash(&arr);
                    if recomputed != hash {
                        errors.push(format!(
                            "hash mismatch: stored={} computed={}",
                            hash_hex(&hash),
                            hash_hex(&recomputed),
                        ));
                    }
                }
                Err(e) => errors.push(format!("read error: {e}")),
            }
        }
        Ok(IntegrityReport { errors })
    }

    fn flush(&mut self) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        inner.file.sync().map_err(map_nc)
    }

    fn compression(&self) -> Compression {
        NetCdfBackend::compression(self)
    }
}
