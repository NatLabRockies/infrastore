//! Direct-HDF5 storage backend.
//!
//! Layout (shared policy in [`super::common`]): packed
//! `sts_{dtype}_{shape}_{length}_{res}` datasets for SingleTimeSeries/DST
//! arrays and `nsts_{dtype}_{shape}_{length}_{timestamps_hash}` datasets for the
//! NonSequentialTimeSeries sharing one time axis, plus one standalone
//! `arr_{hexhash}` dataset per dense forecast (and per irregular series whose
//! time axis nothing else shares), written through libhdf5 via `hdf5-metno`.
//!
//! This backend replaced a netcdf-c–driven one with the same logical layout.
//! The motivation was netcdf-c's define-mode semantics: every new variable
//! followed by a data write triggers an implicit whole-file `H5Fflush` that
//! iterates every open dataset, making a forecast-heavy ingest O(N²). Plain
//! HDF5 dataset creation is O(log n) and flushes metadata lazily, so the
//! per-array standalone layout stays flat with store size. Its files are not
//! readable by this backend (`Store::open` checks the `storage_backend` root
//! attribute, which netcdf-written stores lack, and rejects them).
//!
//! Layout details specific to this backend:
//!
//! * A packed dataset's per-column hashes live in a `{name}_h` dataset of
//!   shape `(cols, 64)` u8 (hex bytes; an all-zero row = free slot).
//! * Standalone arrays at or below [`COMPACT_MAX_BYTES`] use HDF5's compact
//!   layout: the data lives in the object header, no chunk B-tree, no filter
//!   pipeline. (Compact datasets cannot be compressed; arrays that small gain
//!   little from DEFLATE anyway.)
//! * No per-variable dimension objects — HDF5 dataspaces carry the shape.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;

use hdf5_metno as h5;

use h5::types::VarLenUnicode;
use h5::{Group, Hyperslab, Selection, SliceOrIndex};

use crate::error::{Result, TimeSeriesError};
use crate::hash::{array_hash, hash_hex};
use crate::storage::{ArrayLayout, Compression};
use crate::types::array::{Dtype, TypedArray};
use crate::version::DATA_FORMAT_VERSION;

use super::common::{
    COMPRESSION_ATTR, HASH_SUFFIX, PackGroup, ROOT_GROUP, SINGLE_GROUP, STANDALONE_PREFIX,
    dataset_base_name, element_block_bytes, hex_to_hash, parse_dataset_name, resolve_dataset_cols,
    spill_name, standalone_chunks,
};
use super::{ArrayLocation, BackendStats, CompactionReport, IntegrityReport, StorageBackend};

/// Root attribute naming the backend that wrote the file; `Store::open` checks
/// it (absent on stores written by the removed netcdf backend, which are
/// rejected).
pub(crate) const BACKEND_ATTR: &str = "storage_backend";
pub(crate) const BACKEND_NAME: &str = "hdf5";

/// Standalone arrays at or below this size use HDF5's compact layout (data in
/// the object header, no chunk index, no filters). The format limit on a
/// compact dataset is 64 KiB including type/space metadata; stay safely under.
const COMPACT_MAX_BYTES: usize = 56 * 1024;

/// Raw-data chunk cache applied file-wide (every dataset opened through the
/// handle inherits it unless overridden). HDF5's default is 1 MiB / 521 slots
/// — far too small for the packed layout, whose timestamp-major chunks run up
/// to the 1 MiB budget × the series length per dataset: a repeated
/// single-column read then evicts and re-inflates every chunk on each call.
/// 64 MiB keeps a whole packed dataset's chunks resident (netcdf-c sizes its
/// per-variable cache generously for the same reason). Slot count is a prime
/// well above the chunks a dataset can hold.
const CHUNK_CACHE_NSLOTS: usize = 8209;
const CHUNK_CACHE_NBYTES: usize = 64 << 20;
const CHUNK_CACHE_W0: f64 = 0.75;

/// File builder with the store's chunk-cache policy applied.
fn file_builder() -> h5::FileBuilder {
    let mut b = h5::FileBuilder::new();
    b.with_fapl(|p| p.chunk_cache(CHUNK_CACHE_NSLOTS, CHUNK_CACHE_NBYTES, CHUNK_CACHE_W0));
    b
}

fn map_h5(e: h5::Error) -> TimeSeriesError {
    TimeSeriesError::IntegrityError(format!("hdf5: {e}"))
}

/// Selection from one range per axis.
fn sel(ranges: Vec<Range<usize>>) -> Selection {
    Selection::from(Hyperslab::from(
        ranges
            .into_iter()
            .map(SliceOrIndex::from)
            .collect::<Vec<_>>(),
    ))
}

fn packed_ranges(time: Range<usize>, col: usize, element_shape: &[usize]) -> Vec<Range<usize>> {
    let mut ranges = vec![time, col..col + 1];
    ranges.extend(element_shape.iter().map(|&k| 0..k));
    ranges
}

fn packed_block_ranges(
    time: Range<usize>,
    width: usize,
    element_shape: &[usize],
) -> Vec<Range<usize>> {
    let mut ranges = vec![time, 0..width];
    ranges.extend(element_shape.iter().map(|&k| 0..k));
    ranges
}

fn standalone_ranges(time: Range<usize>, element_shape: &[usize]) -> Vec<Range<usize>> {
    let mut ranges = vec![time];
    ranges.extend(element_shape.iter().map(|&k| 0..k));
    ranges
}

// ---- typed read/write helpers ---------------------------------------------

/// Element-by-element decode of little-endian `$bytes` into `Vec<$t>`. Only the
/// big-endian path needs it; see [`le_values!`].
#[cfg(not(target_endian = "little"))]
macro_rules! vec_from_le {
    ($bytes:expr, $t:ty, $n:expr) => {
        $bytes
            .chunks_exact($n)
            .map(|c| <$t>::from_le_bytes(c.try_into().unwrap()))
            .collect::<Vec<$t>>()
    };
}

/// View little-endian `$bytes` as `Cow<[$t]>` for handing to libhdf5.
///
/// A [`TypedArray`]'s buffer is already in the layout libhdf5 wants on a
/// little-endian host, so this borrows it outright whenever the slice is
/// `$t`-aligned — which is the case for a whole array buffer, and is the point:
/// a write no longer allocates and fills a second copy of the data first. A
/// misaligned slice (a sub-slice of a packed write, say) still only costs a
/// memcpy. Big-endian hosts decode element by element, as before.
macro_rules! le_values {
    ($bytes:expr, $t:ty, $n:expr) => {{
        #[cfg(target_endian = "little")]
        {
            match bytemuck::try_cast_slice::<u8, $t>($bytes) {
                Ok(values) => std::borrow::Cow::Borrowed(values),
                // Alignment is the only failure mode reachable here: callers
                // pass whole elements, so the length is always a multiple of
                // `size_of::<$t>()`.
                Err(_) => std::borrow::Cow::Owned(bytemuck::pod_collect_to_vec::<u8, $t>($bytes)),
            }
        }
        #[cfg(not(target_endian = "little"))]
        {
            std::borrow::Cow::<[$t]>::Owned(vec_from_le!($bytes, $t, $n))
        }
    }};
}

/// The inverse of [`le_values!`]: `$values` in its canonical little-endian byte
/// form. A memcpy on a little-endian host, an element-wise swap elsewhere.
macro_rules! le_bytes {
    ($values:expr, $t:ty) => {{
        #[cfg(target_endian = "little")]
        {
            bytemuck::cast_slice::<$t, u8>($values).to_vec()
        }
        #[cfg(not(target_endian = "little"))]
        {
            $values
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<u8>>()
        }
    }};
}

/// Read the hyperslab `ranges` of `ds` as little-endian bytes.
fn read_sel(ds: &h5::Dataset, dtype: Dtype, ranges: Vec<Range<usize>>) -> Result<Vec<u8>> {
    let s = sel(ranges);
    macro_rules! rd {
        ($t:ty) => {{
            let a = ds.read_slice::<$t, _, ndarray::IxDyn>(s).map_err(map_h5)?;
            match a.as_slice() {
                // A hyperslab read is standard-layout, so this is the path taken.
                Some(values) => le_bytes!(values, $t),
                None => a.iter().flat_map(|v| v.to_le_bytes()).collect(),
            }
        }};
    }
    Ok(match dtype {
        Dtype::F64 => rd!(f64),
        Dtype::F32 => rd!(f32),
        Dtype::I64 => rd!(i64),
        Dtype::I32 => rd!(i32),
        Dtype::I16 => rd!(i16),
        Dtype::I8 => rd!(i8),
        Dtype::U64 => rd!(u64),
        Dtype::U32 => rd!(u32),
        Dtype::U16 => rd!(u16),
        Dtype::U8 => rd!(u8),
        Dtype::Bool => {
            let a = ds.read_slice::<u8, _, ndarray::IxDyn>(s).map_err(map_h5)?;
            match a.as_slice() {
                Some(values) => values.to_vec(),
                None => a.iter().copied().collect(),
            }
        }
    })
}

/// Read the whole dataset as little-endian bytes (row-major).
fn read_all(ds: &h5::Dataset, dtype: Dtype) -> Result<Vec<u8>> {
    macro_rules! rd {
        ($t:ty) => {{
            let v = ds.read_raw::<$t>().map_err(map_h5)?;
            le_bytes!(&v, $t)
        }};
    }
    Ok(match dtype {
        Dtype::F64 => rd!(f64),
        Dtype::F32 => rd!(f32),
        Dtype::I64 => rd!(i64),
        Dtype::I32 => rd!(i32),
        Dtype::I16 => rd!(i16),
        Dtype::I8 => rd!(i8),
        Dtype::U64 => rd!(u64),
        Dtype::U32 => rd!(u32),
        Dtype::U16 => rd!(u16),
        Dtype::U8 => rd!(u8),
        Dtype::Bool => ds.read_raw::<u8>().map_err(map_h5)?,
    })
}

/// Write little-endian `bytes` (logical shape `shape`) into the hyperslab
/// `ranges` of `ds`. The selection's shape must equal `shape`.
fn write_sel(
    ds: &h5::Dataset,
    dtype: Dtype,
    bytes: &[u8],
    shape: &[usize],
    ranges: Vec<Range<usize>>,
) -> Result<()> {
    let s = sel(ranges);
    let dyn_shape = ndarray::IxDyn(shape);
    macro_rules! wr {
        ($t:ty, $n:expr) => {{
            let v = le_values!(bytes, $t, $n);
            let view = ndarray::ArrayViewD::from_shape(dyn_shape, v.as_ref())
                .map_err(|e| TimeSeriesError::IntegrityError(format!("shape error: {e}")))?;
            ds.write_slice(view, s).map_err(map_h5)
        }};
    }
    match dtype {
        Dtype::F64 => wr!(f64, 8),
        Dtype::F32 => wr!(f32, 4),
        Dtype::I64 => wr!(i64, 8),
        Dtype::I32 => wr!(i32, 4),
        Dtype::I16 => wr!(i16, 2),
        Dtype::I8 => wr!(i8, 1),
        Dtype::U64 => wr!(u64, 8),
        Dtype::U32 => wr!(u32, 4),
        Dtype::U16 => wr!(u16, 2),
        Dtype::U8 => wr!(u8, 1),
        Dtype::Bool => {
            let view = ndarray::ArrayViewD::from_shape(dyn_shape, bytes)
                .map_err(|e| TimeSeriesError::IntegrityError(format!("shape error: {e}")))?;
            ds.write_slice(view, s).map_err(map_h5)
        }
    }
}

/// Write little-endian `bytes` as the whole content of `ds`.
fn write_all(ds: &h5::Dataset, dtype: Dtype, bytes: &[u8]) -> Result<()> {
    macro_rules! wr {
        ($t:ty, $n:expr) => {{
            let v = le_values!(bytes, $t, $n);
            ds.write_raw(v.as_ref()).map_err(map_h5)
        }};
    }
    match dtype {
        Dtype::F64 => wr!(f64, 8),
        Dtype::F32 => wr!(f32, 4),
        Dtype::I64 => wr!(i64, 8),
        Dtype::I32 => wr!(i32, 4),
        Dtype::I16 => wr!(i16, 2),
        Dtype::I8 => wr!(i8, 1),
        Dtype::U64 => wr!(u64, 8),
        Dtype::U32 => wr!(u32, 4),
        Dtype::U16 => wr!(u16, 2),
        Dtype::U8 => wr!(u8, 1),
        Dtype::Bool => ds.write_raw(bytes).map_err(map_h5),
    }
}

/// Create a dataset of `dtype` with the given shape/chunking/filters.
/// `chunks = None` → compact when small enough, else contiguous.
fn create_ds(
    group: &Group,
    name: &str,
    dtype: Dtype,
    shape: &[usize],
    chunks: Option<&[usize]>,
    compression: Compression,
) -> Result<h5::Dataset> {
    let nbytes: usize = shape.iter().product::<usize>() * dtype.size();
    macro_rules! mk {
        ($t:ty) => {{
            let b = group.new_dataset::<$t>().shape(shape.to_vec());
            match chunks {
                Some(c) => {
                    let b = b.chunk(c.to_vec());
                    match compression {
                        Compression::Deflate { level, shuffle } => {
                            let b = if shuffle { b.shuffle() } else { b };
                            b.deflate(level).create(name)
                        }
                        Compression::None => b.create(name),
                    }
                }
                None => {
                    if nbytes > 0 && nbytes <= COMPACT_MAX_BYTES {
                        b.layout(h5::dataset::Layout::Compact).create(name)
                    } else {
                        b.create(name)
                    }
                }
            }
        }
        .map_err(map_h5)};
    }
    match dtype {
        Dtype::F64 => mk!(f64),
        Dtype::F32 => mk!(f32),
        Dtype::I64 => mk!(i64),
        Dtype::I32 => mk!(i32),
        Dtype::I16 => mk!(i16),
        Dtype::I8 => mk!(i8),
        Dtype::U64 => mk!(u64),
        Dtype::U32 => mk!(u32),
        Dtype::U16 => mk!(u16),
        Dtype::U8 => mk!(u8),
        Dtype::Bool => mk!(u8),
    }
}

fn write_str_attr(file: &h5::File, name: &str, value: &str) -> Result<()> {
    let v = VarLenUnicode::from_str(value)
        .map_err(|e| TimeSeriesError::IntegrityError(format!("attr encode: {e}")))?;
    file.new_attr::<VarLenUnicode>()
        .create(name)
        .map_err(map_h5)?
        .write_scalar(&v)
        .map_err(map_h5)
}

fn read_str_attr(file: &h5::File, name: &str) -> Option<String> {
    file.attr(name)
        .ok()?
        .read_scalar::<VarLenUnicode>()
        .ok()
        .map(|v| v.to_string())
}

// ---- backend state ---------------------------------------------------------

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

type DatasetGroupKey = (Dtype, Vec<usize>, usize, PackGroup);

pub(crate) struct Hdf5Backend {
    inner: Mutex<Inner>,
}

struct Inner {
    file: h5::File,
    /// The `/time_series/single` group, held open for the file's lifetime.
    single: Group,
    /// Open dataset handles, cached for the file's lifetime. HDF5's raw-data
    /// chunk cache lives per *open dataset handle* — reopening a dataset on
    /// every read would discard the cache and re-inflate every touched chunk
    /// per call (netcdf-c avoids this by keeping all variables open; so do
    /// we). Interior mutability is safe: `Inner` sits behind the backend's
    /// `Mutex`.
    handles: std::cell::RefCell<HashMap<String, h5::Dataset>>,
    read_only: bool,
    datasets: HashMap<String, DatasetState>,
    dataset_groups: HashMap<DatasetGroupKey, Vec<String>>,
    standalone_vars: HashSet<String>,
    by_hash: HashMap<[u8; 32], Location>,
    compression: Compression,
}

impl Hdf5Backend {
    pub fn create(path: &Path, compression: Compression) -> Result<Self> {
        let file = file_builder().create(path).map_err(map_h5)?;
        write_str_attr(&file, "data_format_version", DATA_FORMAT_VERSION)?;
        write_str_attr(&file, COMPRESSION_ATTR, &compression.encode())?;
        write_str_attr(&file, BACKEND_ATTR, BACKEND_NAME)?;
        let ts = file.create_group(ROOT_GROUP).map_err(map_h5)?;
        let single = ts.create_group(SINGLE_GROUP).map_err(map_h5)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                file,
                single,
                handles: std::cell::RefCell::new(HashMap::new()),
                read_only: false,
                datasets: HashMap::new(),
                dataset_groups: HashMap::new(),
                standalone_vars: HashSet::new(),
                by_hash: HashMap::new(),
                compression,
            }),
        })
    }

    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        let file = if read_only {
            file_builder().open(path).map_err(map_h5)?
        } else {
            file_builder().open_rw(path).map_err(map_h5)?
        };
        let found =
            read_str_attr(&file, "data_format_version").unwrap_or_else(|| "unspecified".into());
        if found != DATA_FORMAT_VERSION {
            return Err(TimeSeriesError::IncompatibleFormat {
                found,
                expected: DATA_FORMAT_VERSION,
            });
        }
        let compression = read_str_attr(&file, COMPRESSION_ATTR)
            .map(|s| Compression::decode(&s))
            .unwrap_or_default();
        let single = file
            .group(&format!("{ROOT_GROUP}/{SINGLE_GROUP}"))
            .map_err(map_h5)?;
        let mut backend = Self {
            inner: Mutex::new(Inner {
                file,
                single,
                handles: std::cell::RefCell::new(HashMap::new()),
                read_only,
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

    pub fn compression(&self) -> Compression {
        self.inner.lock().expect("mutex poisoned").compression
    }

    /// Pre-create a packed pool wide enough for up to `cols` columns, returning
    /// the width it actually got (the per-chunk byte budget caps it, so a large
    /// request may need several calls).
    ///
    /// A pool created on demand by the single-array write path is sized for
    /// growth — it reserves [`DEFAULT_COLS_PER_DATASET`](super::common::DEFAULT_COLS_PER_DATASET)
    /// columns, and its hash companion costs 64 bytes per column whether or not
    /// the slot is ever filled. A caller that knows the whole cohort up front
    /// (rewriting a store: [`crate::Store::compact`] and
    /// [`crate::Store::persist_to`]) uses this to size each pool exactly, the
    /// same bet the bulk-add path makes. Otherwise a rewrite of a
    /// bulk-written store would *grow* the file.
    pub(crate) fn reserve_pack_group(
        &mut self,
        dtype: Dtype,
        element_shape: &[usize],
        length: usize,
        group: PackGroup,
        cols: usize,
    ) -> Result<usize> {
        let inner = self.inner.get_mut().expect("mutex poisoned");
        let key = (dtype, element_shape.to_vec(), length, group);
        let spill_count = inner.dataset_groups.get(&key).map_or(0, Vec::len);
        let base = dataset_base_name(dtype, element_shape, length, group);
        let name = spill_name(&base, spill_count);
        inner.create_packed_dataset(&name, dtype, element_shape, length, group, Some(cols))?;
        Ok(inner
            .datasets
            .get(&name)
            .map_or(0, |state| state.columns.len()))
    }

    #[tracing::instrument(skip(self))]
    fn rebuild_index(&mut self) -> Result<()> {
        let inner = self.inner.get_mut().expect("mutex poisoned");
        let single = inner.single()?;
        let names = single.member_names().map_err(map_h5)?;
        for name in names {
            if name.ends_with(HASH_SUFFIX) {
                continue;
            }
            if let Some(hex) = name.strip_prefix(STANDALONE_PREFIX) {
                let hash = hex_to_hash(hex)?;
                inner.standalone_vars.insert(name.clone());
                inner
                    .by_hash
                    .insert(hash, Location::Standalone { var: name });
                continue;
            }

            let (dtype, element_shape, length, group) = parse_dataset_name(&name)?;
            let hash_name = format!("{name}{HASH_SUFFIX}");
            let hash_ds = single.dataset(&hash_name).map_err(|_| {
                TimeSeriesError::IntegrityError(format!("missing hash dataset {hash_name}"))
            })?;
            let raw = hash_ds.read_raw::<u8>().map_err(map_h5)?;
            let num_cols = raw.len() / 64;
            let mut columns = Vec::with_capacity(num_cols);
            for i in 0..num_cols {
                let row = &raw[i * 64..(i + 1) * 64];
                if row[0] == 0 {
                    columns.push(None);
                } else {
                    let hex = std::str::from_utf8(row).map_err(|_| {
                        TimeSeriesError::IntegrityError(format!("bad hash row {i} in {hash_name}"))
                    })?;
                    let hash = hex_to_hash(hex)?;
                    inner.by_hash.insert(
                        hash,
                        Location::Packed {
                            dataset: name.clone(),
                            col: i,
                        },
                    );
                    columns.push(Some(hex.to_string()));
                }
            }
            inner
                .dataset_groups
                .entry((dtype, element_shape.clone(), length, group))
                .or_default()
                .push(name.clone());
            inner.datasets.insert(
                name.clone(),
                DatasetState {
                    hash_name,
                    dtype,
                    element_shape,
                    length,
                    columns,
                },
            );
        }
        for names in inner.dataset_groups.values_mut() {
            names.sort();
        }
        Ok(())
    }
}

impl Inner {
    fn single(&self) -> Result<Group> {
        Ok(self.single.clone())
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        Ok(())
    }

    fn dataset(&self, name: &str) -> Result<h5::Dataset> {
        if let Some(ds) = self.handles.borrow().get(name) {
            return Ok(ds.clone());
        }
        let ds = self
            .single
            .dataset(name)
            .map_err(|_| TimeSeriesError::IntegrityError(format!("missing dataset {name}")))?;
        self.handles
            .borrow_mut()
            .insert(name.to_string(), ds.clone());
        Ok(ds)
    }

    fn ensure_writable_dataset(
        &mut self,
        dtype: Dtype,
        element_shape: &[usize],
        length: usize,
        group: PackGroup,
    ) -> Result<String> {
        let key = (dtype, element_shape.to_vec(), length, group);
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
        let base = dataset_base_name(dtype, element_shape, length, group);
        let new_name = spill_name(&base, spill_count);
        self.create_packed_dataset(&new_name, dtype, element_shape, length, group, None)?;
        Ok(new_name)
    }

    fn create_packed_dataset(
        &mut self,
        name: &str,
        dtype: Dtype,
        element_shape: &[usize],
        length: usize,
        group: PackGroup,
        requested_cols: Option<usize>,
    ) -> Result<()> {
        let cols = resolve_dataset_cols(requested_cols, dtype, element_shape);
        let mut shape = vec![length, cols];
        shape.extend_from_slice(element_shape);
        // Chunk one timestamp row across every column, matching the packed
        // backend: a read-by-timestamp gathers one chunk and a full-width bulk
        // write fills whole chunks.
        let mut chunks = vec![1, cols];
        chunks.extend_from_slice(element_shape);
        let hash_name = format!("{name}{HASH_SUFFIX}");
        let single = self.single()?;
        create_ds(
            &single,
            name,
            dtype,
            &shape,
            Some(&chunks),
            self.compression,
        )?;
        // The hash companion is tiny and rewritten per column; contiguous.
        create_ds(
            &single,
            &hash_name,
            Dtype::Bool, // stored as u8
            &[cols, 64],
            None,
            Compression::None,
        )?;
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
            .entry((dtype, element_shape.to_vec(), length, group))
            .or_default();
        let pos = group
            .binary_search_by(|n| n.as_str().cmp(name))
            .unwrap_or_else(|p| p);
        group.insert(pos, name.to_string());
        Ok(())
    }

    fn write_hash_row(&self, hash_name: &str, col: usize, hex: Option<&str>) -> Result<()> {
        let ds = self.dataset(hash_name)?;
        let row: Vec<u8> = match hex {
            Some(h) => h.as_bytes().to_vec(),
            None => vec![0u8; 64],
        };
        write_sel(&ds, Dtype::Bool, &row, &[1, 64], vec![col..col + 1, 0..64])
    }

    #[tracing::instrument(skip(self, hash, data), fields(bytes = data.bytes.len()))]
    fn put_packed(&mut self, hash: &[u8; 32], data: &TypedArray, group: PackGroup) -> Result<()> {
        let length = data.length();
        let element_shape = data.element_shape().to_vec();
        let dtype = data.dtype;

        let dataset_name = self.ensure_writable_dataset(dtype, &element_shape, length, group)?;
        let (col_index, hash_name) = {
            let state = self.datasets.get(&dataset_name).expect("dataset ensured");
            let col = state.first_free().ok_or_else(|| {
                TimeSeriesError::IntegrityError("no free slot in newly-ensured dataset".into())
            })?;
            (col, state.hash_name.clone())
        };

        let ds = self.dataset(&dataset_name)?;
        let mut slice_shape = vec![length, 1];
        slice_shape.extend_from_slice(&element_shape);
        write_sel(
            &ds,
            dtype,
            &data.bytes,
            &slice_shape,
            packed_ranges(0..length, col_index, &element_shape),
        )?;
        let hex = hash_hex(hash);
        self.write_hash_row(&hash_name, col_index, Some(&hex))?;

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
    /// created, batch-sized datasets — one interleaved hyperslab write per
    /// dataset (every chunk is written whole, no read-modify-write).
    #[tracing::instrument(skip(self, hashes, arrays), fields(n = hashes.len()))]
    fn put_packed_block(
        &mut self,
        hashes: &[[u8; 32]],
        arrays: &[&TypedArray],
        group: PackGroup,
    ) -> Result<Vec<bool>> {
        let mut written = vec![false; hashes.len()];

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
        let group_key = (dtype, element_shape.clone(), length, group);

        let mut start = 0;
        while start < new.len() {
            let remaining = new.len() - start;
            let width = resolve_dataset_cols(Some(remaining), dtype, &element_shape);
            let seg = &new[start..start + width];

            let base = dataset_base_name(dtype, &element_shape, length, group);
            let spill_count = self.dataset_groups.get(&group_key).map_or(0, Vec::len);
            let name = spill_name(&base, spill_count);
            self.create_packed_dataset(&name, dtype, &element_shape, length, group, Some(width))?;
            let hash_name = format!("{name}{HASH_SUFFIX}");

            // Interleave the segment's arrays into one row-major
            // `(length, width, *element_shape)` buffer and write it as a
            // single hyperslab covering whole chunks.
            let mut buf = vec![0u8; length * width * block];
            for (c, (_, _, array)) in seg.iter().enumerate() {
                for t in 0..length {
                    let src = &array.bytes[t * block..(t + 1) * block];
                    let dst = (t * width + c) * block;
                    buf[dst..dst + block].copy_from_slice(src);
                }
            }
            let ds = self.dataset(&name)?;
            let mut slice_shape = vec![length, width];
            slice_shape.extend_from_slice(&element_shape);
            write_sel(
                &ds,
                dtype,
                &buf,
                &slice_shape,
                packed_block_ranges(0..length, width, &element_shape),
            )?;
            // Hash rows for the whole segment in one write.
            let mut hash_buf = vec![0u8; width * 64];
            for (c, (_, hash, _)) in seg.iter().enumerate() {
                hash_buf[c * 64..(c + 1) * 64].copy_from_slice(hash_hex(hash).as_bytes());
            }
            let hash_ds = self.dataset(&hash_name)?;
            write_sel(
                &hash_ds,
                Dtype::Bool,
                &hash_buf,
                &[width, 64],
                vec![0..width, 0..64],
            )?;

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
    fn put_standalone(
        &mut self,
        hash: &[u8; 32],
        data: &TypedArray,
        window_axis: Option<usize>,
    ) -> Result<()> {
        let var = format!("{STANDALONE_PREFIX}{}", hash_hex(hash));
        if self.standalone_vars.contains(&var) {
            self.by_hash.insert(*hash, Location::Standalone { var });
            return Ok(());
        }
        let single = self.single()?;
        // Small arrays go compact (no chunks, no filters, data in the object
        // header). Larger ones keep the shared chunking policy:
        // whole-array for irregular series, bounded blocks along the window
        // axis for dense forecasts.
        let ds = if data.bytes.len() <= COMPACT_MAX_BYTES || data.shape.contains(&0) {
            create_ds(
                &single,
                &var,
                data.dtype,
                &data.shape,
                None,
                Compression::None,
            )?
        } else {
            let chunks = standalone_chunks(data, window_axis);
            create_ds(
                &single,
                &var,
                data.dtype,
                &data.shape,
                Some(&chunks),
                self.compression,
            )?
        };
        if !data.bytes.is_empty() {
            write_all(&ds, data.dtype, &data.bytes)?;
        }
        self.standalone_vars.insert(var.clone());
        self.by_hash.insert(*hash, Location::Standalone { var });
        Ok(())
    }

    #[tracing::instrument(skip(self, hash, range))]
    fn read_locked(
        &self,
        hash: &[u8; 32],
        dtype: Dtype,
        range: Option<Range<usize>>,
    ) -> Result<TypedArray> {
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
                // A packed dataset's name carries its dtype, so here the
                // caller's value is an assertion the two artifacts agree.
                super::check_dtype(hash, state.dtype, dtype)?;
                let total = state.length;
                let range = range.unwrap_or(0..total);
                if range.start > range.end || range.end > total {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "slice {:?} out of bounds for length {}",
                        range, total
                    )));
                }
                let out_len = range.end - range.start;
                let ds = self.dataset(&dataset)?;
                let bytes = read_sel(
                    &ds,
                    state.dtype,
                    packed_ranges(range, col, &state.element_shape),
                )?;
                let mut shape = vec![out_len];
                shape.extend_from_slice(&state.element_shape);
                TypedArray::new(state.dtype, shape, bytes).map_err(TimeSeriesError::IntegrityError)
            }
            Location::Standalone { var } => {
                let ds = self.dataset(&var)?;
                let full_shape = ds.shape();
                let total = full_shape.first().copied().unwrap_or(0);
                match range {
                    None => {
                        let bytes = read_all(&ds, dtype)?;
                        TypedArray::new(dtype, full_shape, bytes)
                            .map_err(TimeSeriesError::IntegrityError)
                    }
                    Some(range) => {
                        if range.start > range.end || range.end > total {
                            return Err(TimeSeriesError::InvalidParameter(format!(
                                "slice {:?} out of bounds for length {}",
                                range, total
                            )));
                        }
                        let element_shape = &full_shape[1.min(full_shape.len())..];
                        let out_len = range.end - range.start;
                        let bytes = read_sel(&ds, dtype, standalone_ranges(range, element_shape))?;
                        let mut shape = vec![out_len];
                        shape.extend_from_slice(element_shape);
                        TypedArray::new(dtype, shape, bytes)
                            .map_err(TimeSeriesError::IntegrityError)
                    }
                }
            }
        }
    }

    fn read_index_locked(
        &self,
        hashes: &[[u8; 32]],
        dtype: Dtype,
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

        let mut placements: Vec<Placement> = Vec::with_capacity(hashes.len());
        let mut max_col: HashMap<String, usize> = HashMap::new();
        for hash in hashes {
            match self.by_hash.get(hash).ok_or(TimeSeriesError::NotFound)? {
                Location::Packed { dataset, col } => {
                    self.check_packed_dtype(hash, dataset, dtype)?;
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
            let elem_bytes =
                state.element_shape.iter().product::<usize>().max(1) * state.dtype.size();
            let ds = self.dataset(dataset)?;
            let bytes = read_sel(
                &ds,
                state.dtype,
                packed_block_ranges(index..index + 1, width, &state.element_shape),
            )?;
            rows.insert(dataset.clone(), (bytes, elem_bytes));
        }

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
                    let arr = self.read_locked(hash, dtype, Some(index..index + 1))?;
                    out.extend_from_slice(&arr.bytes);
                }
            }
        }
        Ok(())
    }

    fn read_arrays_locked(&self, hashes: &[[u8; 32]], dtypes: &[Dtype]) -> Result<Vec<TypedArray>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        enum Placement {
            Packed { dataset: String, col: usize },
            Standalone { hash: [u8; 32], dtype: Dtype },
        }

        if dtypes.len() != hashes.len() {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "read_arrays: {} dtypes for {} hashes",
                dtypes.len(),
                hashes.len()
            )));
        }
        let mut placements: Vec<Placement> = Vec::with_capacity(hashes.len());
        let mut max_col: HashMap<String, usize> = HashMap::new();
        for (hash, &dtype) in hashes.iter().zip(dtypes) {
            match self.by_hash.get(hash).ok_or(TimeSeriesError::NotFound)? {
                Location::Packed { dataset, col } => {
                    self.check_packed_dtype(hash, dataset, dtype)?;
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
                    placements.push(Placement::Standalone { hash: *hash, dtype });
                }
            }
        }

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
            let elem_bytes =
                state.element_shape.iter().product::<usize>().max(1) * state.dtype.size();
            let ds = self.dataset(dataset)?;
            let bytes = read_sel(
                &ds,
                state.dtype,
                packed_block_ranges(0..state.length, width, &state.element_shape),
            )?;
            reads.insert(
                dataset.clone(),
                DatasetRead {
                    bytes,
                    width,
                    length: state.length,
                    dtype: state.dtype,
                    element_shape: state.element_shape.clone(),
                    elem_bytes,
                },
            );
        }

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
                Placement::Standalone { hash, dtype } => {
                    out.push(self.read_locked(hash, *dtype, None)?);
                }
            }
        }
        Ok(out)
    }

    /// Assert that a packed dataset's own dtype (recovered from its name) is the
    /// one the catalog says the array has. Only reachable if the two artifacts
    /// have drifted.
    fn check_packed_dtype(&self, hash: &[u8; 32], dataset: &str, dtype: Dtype) -> Result<()> {
        let state = self
            .datasets
            .get(dataset)
            .ok_or_else(|| TimeSeriesError::IntegrityError(format!("dataset {dataset} missing")))?;
        super::check_dtype(hash, state.dtype, dtype)
    }

    fn array_shape_locked(&self, hash: &[u8; 32]) -> Result<Vec<usize>> {
        let loc = self
            .by_hash
            .get(hash)
            .ok_or(TimeSeriesError::NotFound)?
            .clone();
        match loc {
            Location::Standalone { var } => Ok(self.dataset(&var)?.shape()),
            Location::Packed { dataset, .. } => {
                let state = self.datasets.get(&dataset).ok_or_else(|| {
                    TimeSeriesError::IntegrityError(format!("dataset {dataset} missing"))
                })?;
                let mut shape = vec![state.length];
                shape.extend_from_slice(&state.element_shape);
                Ok(shape)
            }
        }
    }

    fn read_window_block_locked(
        &self,
        hash: &[u8; 32],
        dtype: Dtype,
        count_axis: usize,
        start: usize,
        len: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        out.clear();
        let loc = self
            .by_hash
            .get(hash)
            .ok_or(TimeSeriesError::NotFound)?
            .clone();
        match loc {
            Location::Standalone { var } => {
                let ds = self.dataset(&var)?;
                let dims = ds.shape();
                if count_axis >= dims.len() {
                    return Err(TimeSeriesError::IntegrityError(format!(
                        "count axis {count_axis} out of bounds for shape {dims:?}"
                    )));
                }
                if start + len > dims[count_axis] {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "window block {start}..{} out of bounds for axis length {}",
                        start + len,
                        dims[count_axis]
                    )));
                }
                let ranges: Vec<Range<usize>> = dims
                    .iter()
                    .enumerate()
                    .map(|(i, &axis_len)| {
                        if i == count_axis {
                            start..start + len
                        } else {
                            0..axis_len
                        }
                    })
                    .collect();
                let bytes = read_sel(&ds, dtype, ranges)?;
                out.extend_from_slice(&bytes);
                Ok(())
            }
            Location::Packed { .. } => Err(TimeSeriesError::IntegrityError(
                "forecast window read expects a standalone array, found a packed one".into(),
            )),
        }
    }
}

impl StorageBackend for Hdf5Backend {
    #[tracing::instrument(skip(self, hash, data), fields(bytes = data.bytes.len()))]
    fn put_array(
        &mut self,
        hash: &[u8; 32],
        data: &TypedArray,
        group: PackGroup,
        layout: ArrayLayout,
    ) -> Result<bool> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        inner.ensure_writable()?;
        if inner.by_hash.contains_key(hash) {
            return Ok(false);
        }
        match layout {
            ArrayLayout::Packed => inner.put_packed(hash, data, group)?,
            ArrayLayout::Standalone => inner.put_standalone(hash, data, None)?,
            ArrayLayout::StandaloneWindowed { count_axis } => {
                inner.put_standalone(hash, data, Some(count_axis))?
            }
        }
        Ok(true)
    }

    #[tracing::instrument(skip(self, hashes, arrays), fields(n = hashes.len()))]
    fn put_packed_block(
        &mut self,
        hashes: &[[u8; 32]],
        arrays: &[&TypedArray],
        group: PackGroup,
    ) -> Result<Vec<bool>> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        inner.ensure_writable()?;
        inner.put_packed_block(hashes, arrays, group)
    }

    fn has_pack_group(
        &self,
        dtype: Dtype,
        element_shape: &[usize],
        length: usize,
        group: PackGroup,
    ) -> bool {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner
            .dataset_groups
            .contains_key(&(dtype, element_shape.to_vec(), length, group))
    }

    #[tracing::instrument(skip(self, hash))]
    fn get_array(&self, hash: &[u8; 32], dtype: Dtype) -> Result<TypedArray> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.read_locked(hash, dtype, None)
    }

    #[tracing::instrument(skip(self, hash), fields(start = range.start, end = range.end))]
    fn get_slice(&self, hash: &[u8; 32], dtype: Dtype, range: Range<usize>) -> Result<TypedArray> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.read_locked(hash, dtype, Some(range))
    }

    #[tracing::instrument(skip(self, hashes, out), fields(n = hashes.len(), index))]
    fn read_index_into(
        &self,
        hashes: &[[u8; 32]],
        dtype: Dtype,
        index: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.read_index_locked(hashes, dtype, index, out)
    }

    #[tracing::instrument(skip(self, hashes), fields(n = hashes.len()))]
    fn read_arrays(&self, hashes: &[[u8; 32]], dtypes: &[Dtype]) -> Result<Vec<TypedArray>> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.read_arrays_locked(hashes, dtypes)
    }

    fn array_shape(&self, hash: &[u8; 32]) -> Result<Vec<usize>> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.array_shape_locked(hash)
    }

    #[tracing::instrument(skip(self, hash, out), fields(count_axis, window_start))]
    fn read_window_block_into(
        &self,
        hash: &[u8; 32],
        dtype: Dtype,
        count_axis: usize,
        window_start: usize,
        len: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let inner = self.inner.lock().expect("mutex poisoned");
        inner.read_window_block_locked(hash, dtype, count_axis, window_start, len, out)
    }

    fn remove_array(&mut self, hash: &[u8; 32]) -> Result<()> {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        inner.ensure_writable()?;
        let loc = match inner.by_hash.remove(hash) {
            Some(v) => v,
            None => return Ok(()),
        };
        match loc {
            // Standalone: unlink the dataset outright. HDF5 does not return the
            // freed space to the filesystem — the file only shrinks when
            // `Store::compact` repacks it — but the object becomes unreachable
            // immediately and stays that way across a reopen (`rebuild_index`
            // re-indexes by scanning links, so a lingering dataset would be
            // resurrected as live). Re-adding the same content therefore
            // rewrites the dataset instead of being a pure re-index; that is
            // the accepted cost of making removal real.
            Location::Standalone { var } => {
                // Drop any cached handle first: it keeps the object alive past
                // the unlink and would serve stale reads.
                inner.handles.borrow_mut().remove(&var);
                inner.standalone_vars.remove(&var);
                let single = inner.single()?;
                single.unlink(&var).map_err(map_h5)
            }
            // Packed: drop the column from the index, clear its hash companion
            // so a reopen does not re-index it, and zero the column's data so
            // no stale values are readable through the slot once it is reused.
            // Costs a read-modify-write of the touched chunks; removal is not a
            // hot path.
            Location::Packed { dataset, col } => {
                let (hash_name, dtype, element_shape, length) = {
                    let state = inner.datasets.get_mut(&dataset).ok_or_else(|| {
                        TimeSeriesError::IntegrityError(format!("dataset {dataset} missing"))
                    })?;
                    if col < state.columns.len() {
                        state.columns[col] = None;
                    }
                    (
                        state.hash_name.clone(),
                        state.dtype,
                        state.element_shape.clone(),
                        state.length,
                    )
                };
                inner.write_hash_row(&hash_name, col, None)?;
                if length > 0 {
                    let ds = inner.dataset(&dataset)?;
                    let mut slice_shape = vec![length, 1];
                    slice_shape.extend_from_slice(&element_shape);
                    let zeros = vec![0u8; slice_shape.iter().product::<usize>() * dtype.size()];
                    write_sel(
                        &ds,
                        dtype,
                        &zeros,
                        &slice_shape,
                        packed_ranges(0..length, col, &element_shape),
                    )?;
                }
                Ok(())
            }
        }
    }

    fn contains(&self, hash: &[u8; 32]) -> Result<bool> {
        let inner = self.inner.lock().expect("mutex poisoned");
        Ok(inner.by_hash.contains_key(hash))
    }

    fn locate(&self, hash: &[u8; 32]) -> Result<ArrayLocation> {
        let inner = self.inner.lock().expect("mutex poisoned");
        // Both layouts live in the same group; `path` makes the name absolute
        // so it can be pasted straight into h5dump/h5py.
        let path = |name: &str| format!("/{ROOT_GROUP}/{SINGLE_GROUP}/{name}");
        Ok(
            match inner.by_hash.get(hash).ok_or(TimeSeriesError::NotFound)? {
                Location::Packed { dataset, col } => ArrayLocation::Packed {
                    dataset: path(dataset),
                    column: *col,
                },
                Location::Standalone { var } => ArrayLocation::Standalone { dataset: path(var) },
            },
        )
    }

    /// Reports what a compaction would reclaim without touching the file.
    ///
    /// `Store::compact` does not call this for an on-disk store: reclaiming
    /// space here means rewriting the file from the catalog's live set, which
    /// the backend cannot see. It stays as the honest "nothing was reclaimed in
    /// place" answer for any other caller.
    fn compact(&mut self) -> Result<CompactionReport> {
        let stats = self.stats();
        Ok(CompactionReport {
            slots_reclaimed: stats.free_packed_slots,
            datasets_dropped: 0,
            feature_sets_reclaimed: 0,
            timestamp_sets_reclaimed: 0,
            bytes_reclaimed: 0,
        })
    }

    fn stats(&self) -> BackendStats {
        let inner = self.inner.lock().expect("mutex poisoned");
        BackendStats {
            free_packed_slots: inner
                .datasets
                .values()
                .map(|s| s.columns.iter().filter(|c| c.is_none()).count())
                .sum(),
            data_datasets: inner.datasets.len() + inner.standalone_vars.len(),
        }
    }

    fn verify(&self, arrays: &[([u8; 32], Dtype)]) -> Result<IntegrityReport> {
        let inner = self.inner.lock().expect("mutex poisoned");
        let mut errors = Vec::new();
        for &(hash, dtype) in arrays {
            match inner.read_locked(&hash, dtype, None) {
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
                Err(TimeSeriesError::NotFound) => errors.push(format!(
                    "dangling reference: the catalog references array {} but the array \
                     file does not hold it",
                    hash_hex(&hash),
                )),
                Err(e) => errors.push(format!("read error for array {}: {e}", hash_hex(&hash))),
            }
        }
        Ok(IntegrityReport { errors })
    }

    fn flush(&mut self) -> Result<()> {
        let inner = self.inner.lock().expect("mutex poisoned");
        if inner.read_only {
            return Ok(());
        }
        inner.file.flush().map_err(map_h5)
    }

    fn compression(&self) -> Compression {
        Hdf5Backend::compression(self)
    }
}

/// True iff the file at `path` was written by this backend (sniffed from the
/// `storage_backend` root attribute; netcdf-written stores lack it).
pub(crate) fn is_hdf5_backend_file(path: &Path) -> bool {
    let Ok(file) = h5::File::open(path) else {
        return false;
    };
    read_str_attr(&file, BACKEND_ATTR).as_deref() == Some(BACKEND_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::array_hash;
    use crate::types::array::TypedArray;
    use crate::types::period::Period;

    fn f64_array(shape: Vec<usize>, seed: f64) -> TypedArray {
        let n: usize = shape.iter().product();
        let vals: Vec<f64> = (0..n).map(|x| seed + x as f64).collect();
        TypedArray::from_f64(shape, &vals)
    }

    /// The packed pool every regular-series test writes into.
    fn res() -> PackGroup {
        PackGroup::Regular(Period::from_iso8601("PT1H").unwrap())
    }

    /// A distinct irregular pool, keyed by a stand-in timestamp-vector hash.
    fn cohort(tag: u8) -> PackGroup {
        PackGroup::Irregular([tag; 32])
    }

    #[test]
    fn packed_round_trip_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        let a = f64_array(vec![24], 0.0);
        let b = f64_array(vec![24], 100.0);
        let (ha, hb) = (array_hash(&a), array_hash(&b));
        {
            let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
            assert!(be.put_array(&ha, &a, res(), ArrayLayout::Packed).unwrap());
            assert!(be.put_array(&hb, &b, res(), ArrayLayout::Packed).unwrap());
            // Duplicate put is a no-op.
            assert!(!be.put_array(&ha, &a, res(), ArrayLayout::Packed).unwrap());
            assert_eq!(be.get_array(&ha, Dtype::F64).unwrap(), a);
            assert_eq!(be.get_array(&hb, Dtype::F64).unwrap(), b);
            let slice = be.get_slice(&ha, Dtype::F64, 6..18).unwrap();
            assert_eq!(slice.shape, vec![12]);
            assert_eq!(slice.bytes, a.bytes[6 * 8..18 * 8]);
            be.flush().unwrap();
        }
        // Reopen: index rebuilt from the hash companions.
        let be = Hdf5Backend::open(&path, true).unwrap();
        assert_eq!(be.get_array(&ha, Dtype::F64).unwrap(), a);
        assert_eq!(be.get_array(&hb, Dtype::F64).unwrap(), b);
        assert!(
            be.verify(&[(ha, Dtype::F64), (hb, Dtype::F64)])
                .unwrap()
                .ok()
        );
    }

    #[test]
    fn packed_block_write_and_bulk_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        let arrays: Vec<TypedArray> = (0..7).map(|i| f64_array(vec![8, 3], i as f64)).collect();
        let hashes: Vec<[u8; 32]> = arrays.iter().map(array_hash).collect();
        let refs: Vec<&TypedArray> = arrays.iter().collect();
        let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
        let written = be.put_packed_block(&hashes, &refs, res()).unwrap();
        assert!(written.iter().all(|&w| w));
        // read_arrays gathers all columns from one hyperslab per dataset.
        let out = be
            .read_arrays(&hashes, &vec![Dtype::F64; hashes.len()])
            .unwrap();
        assert_eq!(out, arrays);
        // read_index_into returns each column's element block at one timestep.
        let mut buf = Vec::new();
        be.read_index_into(&hashes, Dtype::F64, 5, &mut buf)
            .unwrap();
        let elem = 3 * 8;
        for (i, a) in arrays.iter().enumerate() {
            assert_eq!(buf[i * elem..(i + 1) * elem], a.bytes[5 * elem..6 * elem]);
        }
    }

    /// Irregular series pool by their timestamp-vector hash, so two cohorts at
    /// the same dtype/shape/length are distinct datasets — and each is a real
    /// packed pool, with columns addressable at one timestamp.
    #[test]
    fn irregular_cohorts_pack_separately_and_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        let a = f64_array(vec![8], 0.0);
        let b = f64_array(vec![8], 100.0);
        // Same shape and length as the pair above, but a different time axis.
        let c = f64_array(vec![8], 200.0);
        let (ha, hb, hc) = (array_hash(&a), array_hash(&b), array_hash(&c));

        let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
        be.put_packed_block(&[ha, hb], &[&a, &b], cohort(1))
            .unwrap();
        be.put_array(&hc, &c, cohort(2), ArrayLayout::Packed)
            .unwrap();
        assert!(be.has_pack_group(Dtype::F64, &[], 8, cohort(1)));
        assert!(be.has_pack_group(Dtype::F64, &[], 8, cohort(2)));
        // A cohort nothing has written yet, and the regular pool at the same
        // dtype/shape/length, are both absent: the pools cannot bleed together.
        assert!(!be.has_pack_group(Dtype::F64, &[], 8, cohort(3)));
        assert!(!be.has_pack_group(Dtype::F64, &[], 8, res()));

        let dataset_of = |be: &Hdf5Backend, hash| match be.locate(&hash).unwrap() {
            ArrayLocation::Packed { dataset, column } => (dataset, column),
            other => panic!("expected a packed location, got {other:?}"),
        };
        let (ds_a, col_a) = dataset_of(&be, ha);
        let (ds_b, col_b) = dataset_of(&be, hb);
        let (ds_c, _) = dataset_of(&be, hc);
        assert_eq!(ds_a, ds_b, "one cohort shares one dataset");
        assert_ne!(col_a, col_b, "and each member gets its own column");
        assert_ne!(ds_a, ds_c, "distinct time axes never share a dataset");
        assert!(ds_a.contains("nsts_f64_s_8_"));

        // The cohort's columns are gathered at one timestamp in a single read —
        // the whole point of packing them.
        let mut buf = Vec::new();
        be.read_index_into(&[ha, hb], Dtype::F64, 5, &mut buf)
            .unwrap();
        assert_eq!(buf[..8], a.bytes[5 * 8..6 * 8]);
        assert_eq!(buf[8..], b.bytes[5 * 8..6 * 8]);
        drop(be);

        // Reopen: the pools are rebuilt from the dataset names, so a later add
        // joins the same cohort rather than starting a parallel one.
        let mut be = Hdf5Backend::open(&path, false).unwrap();
        assert_eq!(be.get_array(&ha, Dtype::F64).unwrap(), a);
        assert!(be.has_pack_group(Dtype::F64, &[], 8, cohort(1)));
        let d = f64_array(vec![8], 300.0);
        let hd = array_hash(&d);
        be.put_array(&hd, &d, cohort(1), ArrayLayout::Packed)
            .unwrap();
        // The block write sized that first dataset to its batch, so it is full
        // and this add spills — into a sibling of the *same* pool, which is what
        // the rebuilt index has to get right.
        let spilled = dataset_of(&be, hd).0;
        assert_ne!(spilled, ds_a);
        let leaf = |path: &str| path.rsplit('/').next().unwrap().to_string();
        assert_eq!(
            parse_dataset_name(&leaf(&spilled)).unwrap(),
            (Dtype::F64, vec![], 8, cohort(1))
        );
        assert!(
            be.verify(&[(ha, Dtype::F64), (hb, Dtype::F64), (hd, Dtype::F64)])
                .unwrap()
                .ok()
        );
    }

    /// `bool` and `u8` are the same byte on disk and the HDF5 type descriptor
    /// cannot tell them apart — which is exactly why the dtype comes from the
    /// caller (the catalog) rather than from the file. Each reads back as what
    /// it was written as, and the content hash that addresses it survives.
    #[test]
    fn standalone_bool_and_u8_are_distinguished_by_the_caller_dtype() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        let bools = TypedArray::from_slice(vec![4], &[true, false, true, true]).unwrap();
        let bytes = TypedArray::from_slice(vec![4], &[1u8, 0, 1, 1]).unwrap();
        // Byte-identical, yet distinct arrays: `array_hash` mixes in the dtype,
        // so they content-address to different datasets rather than colliding.
        assert_eq!(bools.bytes, bytes.bytes);
        let (hb, hu) = (array_hash(&bools), array_hash(&bytes));
        assert_ne!(hb, hu);

        let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
        be.put_array(&hb, &bools, res(), ArrayLayout::Standalone)
            .unwrap();
        be.put_array(&hu, &bytes, res(), ArrayLayout::Standalone)
            .unwrap();
        assert_eq!(be.get_array(&hb, Dtype::Bool).unwrap(), bools);
        assert_eq!(be.get_array(&hu, Dtype::U8).unwrap(), bytes);
        assert_eq!(be.array_shape(&hb).unwrap(), vec![4]);
        drop(be);

        // And across a reopen, where the in-memory index is rebuilt from the file.
        let be = Hdf5Backend::open(&path, false).unwrap();
        assert_eq!(be.get_array(&hb, Dtype::Bool).unwrap(), bools);
        let read = be.get_array(&hu, Dtype::U8).unwrap();
        assert_eq!(read, bytes);
        assert_eq!(array_hash(&read), hu, "content address must round-trip");
        assert!(
            be.verify(&[(hb, Dtype::Bool), (hu, Dtype::U8)])
                .unwrap()
                .ok()
        );
    }

    /// A packed dataset knows its own dtype (it is in the dataset name), so a
    /// caller whose catalog disagrees is told, rather than handed misread bytes.
    #[test]
    fn a_packed_read_rejects_a_dtype_the_dataset_contradicts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        let a = f64_array(vec![4], 0.0);
        let ha = array_hash(&a);
        let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
        be.put_array(&ha, &a, res(), ArrayLayout::Packed).unwrap();

        let err = be.get_array(&ha, Dtype::I64).unwrap_err();
        assert!(
            matches!(&err, TimeSeriesError::IntegrityError(m)
                if m.contains("stored as f64") && m.contains("catalog says i64")),
            "{err}"
        );
    }

    #[test]
    fn standalone_forecast_round_trip_window_reads_and_compact_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        // (horizon=6, count=4) dense forecast, small enough for compact.
        let a = f64_array(vec![6, 4], 0.0);
        let ha = array_hash(&a);
        let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
        be.put_array(
            &ha,
            &a,
            res(),
            ArrayLayout::StandaloneWindowed { count_axis: 1 },
        )
        .unwrap();
        assert_eq!(be.get_array(&ha, Dtype::F64).unwrap(), a);
        assert_eq!(be.array_shape(&ha).unwrap(), vec![6, 4]);
        // Window block: windows 1..3 along axis 1.
        let mut buf = Vec::new();
        be.read_window_block_into(&ha, Dtype::F64, 1, 1, 2, &mut buf)
            .unwrap();
        let mut expect = Vec::new();
        for h in 0..6 {
            expect.extend_from_slice(&a.bytes[(h * 4 + 1) * 8..(h * 4 + 3) * 8]);
        }
        assert_eq!(buf, expect);
        // Small standalone arrays use the compact layout (no chunk index).
        {
            let inner = be.inner.lock().unwrap();
            let ds = inner
                .dataset(&format!("{STANDALONE_PREFIX}{}", hash_hex(&ha)))
                .unwrap();
            assert_eq!(ds.layout(), h5::dataset::Layout::Compact);
        }
        // A large forecast falls back to chunked+compressed.
        let big = f64_array(vec![24, 400], 0.5); // 76.8 KB > COMPACT_MAX_BYTES
        let hb = array_hash(&big);
        be.put_array(
            &hb,
            &big,
            res(),
            ArrayLayout::StandaloneWindowed { count_axis: 1 },
        )
        .unwrap();
        assert_eq!(be.get_array(&hb, Dtype::F64).unwrap(), big);
        {
            let inner = be.inner.lock().unwrap();
            let ds = inner
                .dataset(&format!("{STANDALONE_PREFIX}{}", hash_hex(&hb)))
                .unwrap();
            assert_eq!(ds.layout(), h5::dataset::Layout::Chunked);
        }
        // Reopen keeps both readable.
        drop(be);
        let be = Hdf5Backend::open(&path, false).unwrap();
        assert_eq!(be.get_array(&ha, Dtype::F64).unwrap(), a);
        assert_eq!(be.get_array(&hb, Dtype::F64).unwrap(), big);
        assert!(
            be.verify(&[(ha, Dtype::F64), (hb, Dtype::F64)])
                .unwrap()
                .ok()
        );
    }

    #[test]
    fn remove_and_re_add() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        let a = f64_array(vec![24], 0.0);
        let ha = array_hash(&a);
        let f = f64_array(vec![6, 4], 9.0);
        let hf = array_hash(&f);
        let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
        be.put_array(&ha, &a, res(), ArrayLayout::Packed).unwrap();
        be.put_array(
            &hf,
            &f,
            res(),
            ArrayLayout::StandaloneWindowed { count_axis: 1 },
        )
        .unwrap();
        be.remove_array(&ha).unwrap();
        be.remove_array(&hf).unwrap();
        assert!(!be.contains(&ha).unwrap());
        assert!(!be.contains(&hf).unwrap());
        assert!(matches!(
            be.get_array(&ha, Dtype::F64),
            Err(TimeSeriesError::NotFound)
        ));
        // The standalone dataset is unlinked outright, so it is gone from the
        // file rather than lingering as a tombstone.
        {
            let inner = be.inner.lock().unwrap();
            let names = inner.single().unwrap().member_names().unwrap();
            assert!(
                !names.contains(&format!("{STANDALONE_PREFIX}{}", hash_hex(&hf))),
                "removed standalone dataset still present: {names:?}"
            );
        }
        // Re-adding reuses the freed packed slot; the standalone is rewritten.
        assert!(be.put_array(&ha, &a, res(), ArrayLayout::Packed).unwrap());
        assert!(
            be.put_array(
                &hf,
                &f,
                res(),
                ArrayLayout::StandaloneWindowed { count_axis: 1 }
            )
            .unwrap()
        );
        assert_eq!(be.get_array(&ha, Dtype::F64).unwrap(), a);
        assert_eq!(be.get_array(&hf, Dtype::F64).unwrap(), f);
    }

    #[test]
    fn removed_standalone_stays_removed_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        let f = f64_array(vec![6, 4], 9.0);
        let hf = array_hash(&f);
        {
            let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
            be.put_array(
                &hf,
                &f,
                res(),
                ArrayLayout::StandaloneWindowed { count_axis: 1 },
            )
            .unwrap();
            be.remove_array(&hf).unwrap();
            be.flush().unwrap();
        }
        // `rebuild_index` re-indexes standalone datasets by scanning links, so a
        // tombstoned dataset would come back as live. The unlink prevents that.
        let be = Hdf5Backend::open(&path, false).unwrap();
        assert!(!be.contains(&hf).unwrap());
    }

    #[test]
    fn removed_packed_column_reads_back_as_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        let a = f64_array(vec![24], 7.5);
        let ha = array_hash(&a);
        let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
        be.put_array(&ha, &a, res(), ArrayLayout::Packed).unwrap();
        let (dataset, col) = match be.locate(&ha).unwrap() {
            ArrayLocation::Packed { dataset, column } => (dataset, column),
            other => panic!("expected a packed location, got {other:?}"),
        };
        let name = dataset.rsplit('/').next().unwrap().to_string();
        be.remove_array(&ha).unwrap();
        // Read the raw column straight out of the dataset: the removal zero-fills
        // it, so a reused slot can never surface the old values.
        let inner = be.inner.lock().unwrap();
        let ds = inner.dataset(&name).unwrap();
        let raw = read_sel(&ds, Dtype::F64, packed_ranges(0..24, col, &[])).unwrap();
        assert!(
            raw.iter().all(|&b| b == 0),
            "removed column was not zero-filled"
        );
    }

    #[test]
    fn read_only_open_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        let a = f64_array(vec![4], 0.0);
        let ha = array_hash(&a);
        {
            let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
            be.put_array(&ha, &a, res(), ArrayLayout::Packed).unwrap();
        }
        let mut be = Hdf5Backend::open(&path, true).unwrap();
        assert_eq!(be.get_array(&ha, Dtype::F64).unwrap(), a);
        let b = f64_array(vec![4], 5.0);
        let hb = array_hash(&b);
        assert!(matches!(
            be.put_array(&hb, &b, res(), ArrayLayout::Packed),
            Err(TimeSeriesError::ReadOnlyStore)
        ));
    }

    #[test]
    fn backend_sniff() {
        let dir = tempfile::tempdir().unwrap();
        let h5_path = dir.path().join("h.h5");
        let plain_path = dir.path().join("p.h5");
        Hdf5Backend::create(&h5_path, Compression::default()).unwrap();
        // A valid HDF5 file without the backend attribute (e.g. one written by
        // the removed netcdf backend) is not recognized as ours.
        h5::File::create(&plain_path).unwrap();
        assert!(is_hdf5_backend_file(&h5_path));
        assert!(!is_hdf5_backend_file(&plain_path));
    }

    #[test]
    fn standalone_chunk_shapes_follow_the_layout_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.h5");
        // Big enough to skip the compact layout so a chunk shape exists.
        let windowed = f64_array(vec![3, 4000], 0.0);
        let whole = f64_array(vec![3, 4000], 1.0);
        let (hw, hn) = (array_hash(&windowed), array_hash(&whole));
        let mut be = Hdf5Backend::create(&path, Compression::default()).unwrap();
        be.put_array(
            &hw,
            &windowed,
            res(),
            ArrayLayout::StandaloneWindowed { count_axis: 1 },
        )
        .unwrap();
        be.put_array(&hn, &whole, res(), ArrayLayout::Standalone)
            .unwrap();
        let inner = be.inner.lock().unwrap();
        // Full on the horizon axis, blocked along the count axis.
        let cols = super::super::common::window_block_cols(Dtype::F64, &[3, 4000], 1);
        let ds = inner
            .dataset(&format!("{STANDALONE_PREFIX}{}", hash_hex(&hw)))
            .unwrap();
        assert_eq!(ds.chunk(), Some(vec![3, cols]));
        // No window axis -> a single whole-array chunk.
        let ds = inner
            .dataset(&format!("{STANDALONE_PREFIX}{}", hash_hex(&hn)))
            .unwrap();
        assert_eq!(ds.chunk(), Some(vec![3, 4000]));
    }
}
