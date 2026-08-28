//! Storage backend abstraction.
//!
//! The [`StorageBackend`] trait is the only seam between the public API and
//! the actual array-storage implementation. v0 ships two implementations:
//! [`MemoryBackend`] (in-memory) and [`Hdf5Backend`] (HDF5 on disk).

use std::ops::Range;

use chrono::{DateTime, Utc};

use crate::error::{Result, TimeSeriesError};
use crate::types::array::{Dtype, TypedArray};

pub mod common;
pub mod hdf5;
pub mod memory;

pub(crate) use common::PackGroup;

// The concrete backends and the trait seam are internal: the public surface is
// `Store`, which owns a boxed backend. (The `common` module stays `pub` so
// white-box tests can reach `DEFAULT_COLS_PER_DATASET`.)
pub(crate) use hdf5::Hdf5Backend;
pub(crate) use memory::MemoryBackend;

/// Compression filter applied to HDF5 data variables when they are created.
///
/// This is a write-time storage policy: it controls how arrays are encoded on
/// disk but never affects the logical data, which is decoded transparently by
/// HDF5 on read regardless of the filter used. Stores created with
/// different settings therefore remain mutually readable, and the on-disk
/// [`DATA_FORMAT_VERSION`](crate::version::DATA_FORMAT_VERSION) is unaffected.
///
/// The chosen policy is persisted as a global attribute so that appends made
/// after re-opening a store reuse the same filter (see
/// [`Hdf5Backend::open`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// No compression filter is applied; arrays are stored uncompressed.
    None,
    /// DEFLATE (zlib) at `level` (0–9), optionally preceded by the byte-shuffle
    /// filter, which usually improves the ratio for numeric data.
    Deflate { level: u8, shuffle: bool },
}

/// How an array is physically laid out by [`StorageBackend::put_array`].
///
/// This is a write-time storage policy only: it controls chunking and packing
/// but never the logical data, which is read back via chunk-agnostic hyperslabs
/// regardless of the layout chosen at write time. The on-disk
/// [`DATA_FORMAT_VERSION`](crate::version::DATA_FORMAT_VERSION) is therefore
/// unaffected by the choice, and stores written under different layouts stay
/// mutually readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArrayLayout {
    /// Column-pack with other same-shaped arrays (`SingleTimeSeries` and the
    /// backing array of a `DeterministicSingleTimeSeries`). Chunked
    /// timestamp-major so a read across series at one timestamp is one chunk.
    Packed,
    /// Standalone multi-dimensional variable chunked as a single whole-array
    /// chunk. Used for `NonSequentialTimeSeries`, which is read whole or by an
    /// axis-0 time range.
    Standalone,
    /// Standalone, but chunked in bounded blocks along `count_axis` so that
    /// reading one forecast window — a size-1 slice on that axis — decompresses
    /// one block rather than the whole array. Used for dense forecasts
    /// (`Deterministic` → axis 1; `Probabilistic` / `Scenarios` → axis 2).
    StandaloneWindowed { count_axis: usize },
}

impl ArrayLayout {
    /// Whether this layout column-packs the array.
    pub(crate) fn is_packed(self) -> bool {
        matches!(self, ArrayLayout::Packed)
    }
}

impl Default for Compression {
    /// DEFLATE level 3 + shuffle.
    fn default() -> Self {
        Compression::Deflate {
            level: 3,
            shuffle: true,
        }
    }
}

impl Compression {
    /// Reject DEFLATE levels outside the zlib-supported 0–9 range.
    pub fn validate(&self) -> Result<()> {
        if let Compression::Deflate { level, .. } = self
            && *level > 9
        {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "deflate compression level must be 0-9, got {level}"
            )));
        }
        Ok(())
    }

    /// Encode as a stable string for persistence in an HDF5 root attribute.
    pub(crate) fn encode(&self) -> String {
        match self {
            Compression::None => "none".to_string(),
            Compression::Deflate { level, shuffle } => {
                let s = if *shuffle { "shuffle" } else { "noshuffle" };
                format!("deflate:{level}:{s}")
            }
        }
    }

    /// Parse the persisted attribute string. Unknown/malformed values (and
    /// stores predating this attribute) fall back to [`Compression::default`],
    /// which is also the filter such legacy stores were written with.
    pub(crate) fn decode(s: &str) -> Self {
        if s == "none" {
            return Compression::None;
        }
        let mut parts = s.split(':');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("deflate"), Some(level), Some(shuffle)) => match level.parse::<u8>() {
                Ok(level) if level <= 9 => Compression::Deflate {
                    level,
                    shuffle: shuffle != "noshuffle",
                },
                _ => Compression::default(),
            },
            _ => Compression::default(),
        }
    }
}

/// What a [`crate::Store::compact`] reclaimed, across both halves of the
/// artifact.
///
/// For an on-disk store compaction rewrites the HDF5 file from the catalog's
/// live set and swaps the rewritten copy over the original, so these counts
/// describe things that existed in the old file and do not exist in the new
/// one. An in-memory store has no file to rewrite: only `slots_reclaimed` (its
/// tombstone count) and the two catalog sweeps are meaningful there, and
/// `bytes_reclaimed` / `datasets_dropped` stay zero.
#[derive(Debug, Default, Clone)]
pub struct CompactionReport {
    /// Reusable packed-column slots that a removal had freed in the old file.
    /// The rewrite lays the live columns out contiguously, so none of them
    /// survive into the new one.
    pub slots_reclaimed: usize,
    /// Datasets present in the old file and absent from the new one: arrays no
    /// catalog row references any more (leftovers from an interrupted bulk add,
    /// or tombstones written by a version that did not unlink on removal), plus
    /// packed pools the rewrite could fold into fewer datasets.
    pub datasets_dropped: usize,
    /// Content-addressed feature sets in the SQLite catalog that no association
    /// referenced any more, and were deleted. Feature sets are shared, so
    /// removing an association cannot cascade-delete them; they accumulate as
    /// unreachable rows until a compaction sweeps them.
    pub feature_sets_reclaimed: usize,
    /// Content-addressed timestamp vectors in the array file that no association
    /// referenced any more, and were deleted. Shared and swept for exactly the
    /// same reasons as `feature_sets_reclaimed`: removing the last
    /// `NonSequentialTimeSeries` on a time axis leaves that axis behind as an
    /// unreachable dataset until a compaction reclaims it.
    pub timestamp_sets_reclaimed: usize,
    /// How much smaller the HDF5 file got, in bytes (saturating: a rewrite that
    /// happened to grow the file reports 0). Always 0 for an in-memory store.
    pub bytes_reclaimed: u64,
}

/// Physical counts a backend can report about itself, used by
/// [`crate::Store::compact`] to describe what a rewrite removed. The in-memory
/// backend has no such structure and reports zeros.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackendStats {
    /// Packed columns currently marked free (a removal cleared them).
    pub free_packed_slots: usize,
    /// Datasets holding array data — packed pools plus standalone arrays. Hash
    /// companions are bookkeeping, not data, and are not counted.
    pub data_datasets: usize,
}

/// The result of [`crate::Store::verify_integrity`]: one message per array whose
/// recomputed content hash disagreed with the recorded one, or that could not be
/// read at all.
///
/// Empty means every *array* checked out. It does not mean the store as a whole
/// is sound — the SQLite catalog is not inspected. See
/// [`crate::Store::verify_integrity`] for what falls outside the check.
#[derive(Debug, Default, Clone)]
pub struct IntegrityReport {
    pub errors: Vec<String>,
}

impl IntegrityReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Where an array physically lives in the backing store, as returned by
/// [`crate::Store::locate_array`].
///
/// This exists so a caller holding a series' `data_hash` can go and look at the
/// bytes with an outside tool (`h5ls`, `h5dump`, `h5py`). The hash alone is not
/// enough: a packed array is one *column* of a shared dataset, and the column
/// index is only recoverable by scanning that dataset's companion `_h` hash
/// dataset. The dataset name is not derivable either, because a packed pool
/// that fills up spills into `{base}__1`, `{base}__2`, ....
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArrayLocation {
    /// One column of a packed dataset shared with other same-shaped arrays.
    /// `dataset` is the full path from the file root; `column` indexes axis 1.
    Packed { dataset: String, column: usize },
    /// A self-contained dataset holding exactly this array. `dataset` is the
    /// full path from the file root.
    Standalone { dataset: String },
    /// The store is in-memory, so the array has no on-disk location.
    InMemory,
}

impl std::fmt::Display for ArrayLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArrayLocation::Packed { dataset, column } => write!(f, "{dataset}[:, {column}]"),
            ArrayLocation::Standalone { dataset } => write!(f, "{dataset}"),
            ArrayLocation::InMemory => f.write_str("(in-memory)"),
        }
    }
}

/// Pluggable array-storage backend.
///
/// Each array is identified by its 32-byte content hash. Implementations are
/// responsible for any deduplication, slot management, or compaction; the
/// `Store` layer above drives them through this trait.
///
/// **Every read takes the array's `dtype` from the caller.** A backend does not
/// infer it from what it stored: the HDF5 type descriptor cannot distinguish
/// `bool` from `u8`, and it says nothing at all about what the elements *mean*.
/// The catalog owns that (`element_type`, whose `physical_dtype` is what lands
/// here), so the two artifacts cannot drift — a store's `.h5` is not readable
/// without its `.sqlite` in any case. A backend that does know the dtype
/// independently (the packed pool encodes it in the dataset name) treats the
/// caller's value as an assertion and errors on a mismatch.
pub(crate) trait StorageBackend: Send + Sync {
    /// Insert an array. If `hash` already exists, this is a no-op (the existing
    /// data is reused for content addressing) and `false` is returned; a write
    /// of new content returns `true`. The array's dtype + shape travel with it;
    /// `group` names the time axis whose packed pool it joins, and is ignored
    /// for the standalone layouts.
    ///
    /// `layout` selects the physical placement: [`ArrayLayout::Packed`]
    /// column-packs with the other arrays on `group`'s time axis, while the
    /// standalone variants store a self-contained multi-dimensional variable
    /// (native forecasts, and irregular series on an unshared axis), differing
    /// only in how the variable is chunked.
    fn put_array(
        &mut self,
        hash: &[u8; 32],
        data: &TypedArray,
        group: PackGroup,
        layout: ArrayLayout,
    ) -> Result<bool>;

    /// Whether a packed pool for `group` at this `(dtype, element_shape,
    /// length)` already exists.
    ///
    /// The write path asks before packing an irregular series: a pool is a win
    /// only once several columns share it (its timestamp-major chunking spreads
    /// one array over `length` chunks), so a series whose time axis nothing else
    /// uses is better off standalone. The default is `false` — a backend with no
    /// packed representation has no pools to join.
    fn has_pack_group(
        &self,
        _dtype: Dtype,
        _element_shape: &[usize],
        _length: usize,
        _group: PackGroup,
    ) -> bool {
        false
    }

    /// Insert a block of same-shaped packed arrays in one operation.
    ///
    /// `hashes[i]` is the content hash of `arrays[i]`; every array must share one
    /// `(dtype, element_shape, length)` and the given `group` (the caller —
    /// the buffered bulk-add — guarantees this by grouping). Hashes already
    /// stored, and duplicates within the block, are written only once (content
    /// addressing); the returned `Vec<bool>` is aligned to `hashes` and is `true`
    /// for each input that this call physically wrote (so the caller can stage it
    /// for rollback).
    ///
    /// The default loops [`Self::put_array`] with `packed = true`. The on-disk
    /// backend overrides it to create batch-sized datasets and fill whole chunks
    /// with one timestamp-row write per chunk, avoiding the per-column
    /// read-modify-write that the timestamp-major chunking imposes on single adds.
    fn put_packed_block(
        &mut self,
        hashes: &[[u8; 32]],
        arrays: &[&TypedArray],
        group: PackGroup,
    ) -> Result<Vec<bool>> {
        hashes
            .iter()
            .zip(arrays)
            .map(|(hash, data)| self.put_array(hash, data, group, ArrayLayout::Packed))
            .collect()
    }

    /// Fetch the full array for `hash`, decoding its bytes as `dtype`.
    fn get_array(&self, hash: &[u8; 32], dtype: Dtype) -> Result<TypedArray>;

    /// Read many full arrays at once, returning one [`TypedArray`] per input hash
    /// in order (duplicate hashes each yield a copy).
    ///
    /// The default loops [`Self::get_array`]. The on-disk backend overrides it to
    /// read each packed dataset's needed column span in a single hyperslab —
    /// decompressing each timestamp-major chunk once — then scatter the columns
    /// out, rather than re-reading every chunk once per series. This is the bulk
    /// counterpart to the per-timestamp [`Self::read_index_into`]: it amortizes
    /// the decompression cost of whole-series reads across a batch.
    /// `dtypes` is parallel to `hashes`.
    fn read_arrays(&self, hashes: &[[u8; 32]], dtypes: &[Dtype]) -> Result<Vec<TypedArray>> {
        hashes
            .iter()
            .zip(dtypes)
            .map(|(h, &dtype)| self.get_array(h, dtype))
            .collect()
    }

    /// Fetch a slice of the array along axis 0 (the time axis). End is exclusive.
    fn get_slice(&self, hash: &[u8; 32], dtype: Dtype, range: Range<usize>) -> Result<TypedArray>;

    /// Read the value at a single time `index` for a set of co-located arrays,
    /// appending the result to `out` in `hashes` order.
    ///
    /// All `hashes` must share one `(dtype, element_shape)`; the caller (the
    /// timestamp reader) guarantees this by grouping. `out` is cleared first and
    /// then filled with `hashes.len() * element_count * dtype.size()` bytes laid
    /// out row-major as `[column, *element_shape]`. Reusing the caller's buffer
    /// keeps a per-timestamp read loop allocation-free.
    ///
    /// The default reads each array's one-step slice individually; the on-disk
    /// backend overrides this to read a whole packed-dataset row per hyperslab.
    fn read_index_into(
        &self,
        hashes: &[[u8; 32]],
        dtype: Dtype,
        index: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        out.clear();
        for hash in hashes {
            let step = self.get_slice(hash, dtype, index..index + 1)?;
            out.extend_from_slice(&step.bytes);
        }
        Ok(())
    }

    /// The stored shape of an array, ideally without reading its data. Used by
    /// the forecast reader to plan window slicing. Needs no `dtype`: a shape is
    /// element-count-per-axis, which every backend records independently of how
    /// wide an element is.
    fn array_shape(&self, hash: &[u8; 32]) -> Result<Vec<usize>>;

    /// Read a contiguous block of `len` forecast windows starting at
    /// `window_start`: the `window_start..window_start + len` slice along
    /// `count_axis`, keeping that axis. `out` is cleared then filled with the
    /// block's row-major, little-endian bytes in the array's native layout
    /// (count axis interior), so the caller can gather individual windows from
    /// it. Backs the forecast reader's chunk-aligned block cache.
    ///
    /// The default materializes the whole array and copies the block out; the
    /// on-disk backend overrides this to read just the block with one hyperslab,
    /// decompressing only the storage chunks it overlaps.
    fn read_window_block_into(
        &self,
        hash: &[u8; 32],
        dtype: Dtype,
        count_axis: usize,
        window_start: usize,
        len: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let arr = self.get_array(hash, dtype)?;
        out.clear();
        write_window_block(&arr, count_axis, window_start, len, out)
    }

    /// Read `len` consecutive time steps along axis 0 starting at `start`,
    /// filling `out` (cleared first) with their row-major, little-endian bytes.
    /// Backs `DeterministicSingleTimeSeries` window reads, which gather a
    /// contiguous run from the packed underlying `SingleTimeSeries`; on the
    /// on-disk backend [`Self::get_slice`] is already a single packed hyperslab.
    fn read_range_into(
        &self,
        hash: &[u8; 32],
        dtype: Dtype,
        start: usize,
        len: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let slice = self.get_slice(hash, dtype, start..start + len)?;
        out.clear();
        out.extend_from_slice(&slice.bytes);
        Ok(())
    }

    /// Remove an array. Marks the slot reusable. No-op if `hash` is absent.
    fn remove_array(&mut self, hash: &[u8; 32]) -> Result<()>;

    /// True iff the backend currently stores `hash`.
    fn contains(&self, hash: &[u8; 32]) -> Result<bool>;

    // ---- explicit timestamp vectors ---------------------------------------
    //
    // A `NonSequentialTimeSeries` carries its time axis explicitly, and many
    // series share one. The axis is content-addressed by
    // [`crate::hash::timestamps_hash`] and stored once per distinct vector,
    // beside the arrays rather than in the catalog: it is data of the same kind,
    // it can be long, and a store can hold many of them. The catalog keeps only
    // the hash, which doubles as the [`PackGroup::Irregular`] cohort key.

    /// Store `timestamps` under `hash`. Storing a vector the backend already
    /// holds is a no-op returning `false` (content addressing: equal hash
    /// implies equal vector); a write of new content returns `true`, so the
    /// caller can stage it for rollback.
    fn put_timestamps(&mut self, hash: &[u8; 32], timestamps: &[DateTime<Utc>]) -> Result<bool>;

    /// The timestamp vector stored under `hash`, or [`TimeSeriesError::NotFound`].
    fn get_timestamps(&self, hash: &[u8; 32]) -> Result<Vec<DateTime<Utc>>>;

    /// Remove the timestamp vector stored under `hash`. No-op if absent.
    ///
    /// Vectors are shared, so this is never driven by removing one association:
    /// [`crate::Store::compact`] sweeps the ones the catalog no longer
    /// references, exactly as it does unreachable arrays.
    fn remove_timestamps(&mut self, hash: &[u8; 32]) -> Result<()>;

    /// Every timestamp vector the backend holds, by content hash.
    /// [`crate::Store::compact`] diffs this against the set the catalog still
    /// references to find the orphans.
    fn timestamp_hashes(&self) -> Result<Vec<[u8; 32]>>;

    /// Where `hash`'s array physically lives, for outside inspection of the
    /// backing file. The default reports [`ArrayLocation::InMemory`], which is
    /// correct for a backend with no on-disk representation; it still errors on
    /// an unknown hash so callers can tell "not stored" from "not on disk".
    fn locate(&self, hash: &[u8; 32]) -> Result<ArrayLocation> {
        if self.contains(hash)? {
            Ok(ArrayLocation::InMemory)
        } else {
            Err(TimeSeriesError::NotFound)
        }
    }

    /// Reclaim space from removed arrays.
    ///
    /// This is the in-memory backend's whole compaction story. An on-disk store
    /// is compacted by [`crate::Store::compact`] instead, which rewrites the
    /// file from the catalog's live set — the backend cannot do that on its own
    /// because liveness lives in the catalog, not in the file.
    fn compact(&mut self) -> Result<CompactionReport>;

    /// Physical counts describing the backend's current state, for the
    /// before/after arithmetic in a compaction report. The default reports
    /// zeros, which is correct for a backend with no on-disk structure.
    fn stats(&self) -> BackendStats {
        BackendStats::default()
    }

    /// Validate that each `(hash, dtype)` the caller names reads back and
    /// rehashes to its recorded hash.
    ///
    /// The caller supplies the list from the catalog, which is the only place
    /// an array's element typing is recorded. Arrays the backend holds but the
    /// catalog does not reference are therefore not checked: they are
    /// unreachable, nothing can read them, and nothing says what their bytes
    /// mean. [`Self::compact`] is what reports them.
    fn verify(&self, arrays: &[([u8; 32], Dtype)]) -> Result<IntegrityReport>;

    /// Flush any in-memory state to disk (no-op for in-memory backends).
    fn flush(&mut self) -> Result<()>;

    /// This file's generation stamp, pairing it with exactly one catalog.
    ///
    /// `None` both for a backend with no file to stamp and for one written
    /// before the stamp existed; `Store::open` treats either as "unstamped" and
    /// skips the pairing check rather than rejecting the store.
    fn generation(&self) -> Option<String> {
        None
    }

    /// Stamp this file, replacing any existing value. No-op for a backend with
    /// no file to stamp.
    fn set_generation(&mut self, _generation: &str) -> Result<()> {
        Ok(())
    }

    /// The compression policy applied to newly written arrays. In-memory
    /// backends report [`Compression::None`] since they never compress; the
    /// on-disk backend reports the policy it was created or reopened with.
    fn compression(&self) -> Compression {
        Compression::None
    }
}

/// Reject a read whose caller-supplied dtype disagrees with one the backend
/// independently knows (the packed pool's dataset name, or an in-memory array's
/// own tag). Only reachable if the catalog and the array store have drifted, so
/// it is an integrity error rather than a bad argument.
pub(crate) fn check_dtype(hash: &[u8; 32], stored: Dtype, requested: Dtype) -> Result<()> {
    if stored != requested {
        return Err(TimeSeriesError::IntegrityError(format!(
            "array {} is stored as {} but the catalog says {}",
            crate::hash::hash_hex(hash),
            stored.as_str(),
            requested.as_str(),
        )));
    }
    Ok(())
}

/// Copy the contiguous block of `len` windows starting at `start` along
/// `count_axis` out of `arr` into `out`, keeping the count axis. The result is
/// the array's natural row-major sub-block of shape
/// `[..outer.., len, ..inner..]`. For each outer index the `len` windows are
/// contiguous, so the copy is one run per outer index. `out` is appended to.
fn write_window_block(
    arr: &TypedArray,
    count_axis: usize,
    start: usize,
    len: usize,
    out: &mut Vec<u8>,
) -> Result<()> {
    if count_axis >= arr.shape.len() {
        return Err(TimeSeriesError::IntegrityError(format!(
            "count axis {count_axis} out of bounds for shape {:?}",
            arr.shape
        )));
    }
    let axis_len = arr.shape[count_axis];
    if start + len > axis_len {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "window block {start}..{} out of bounds for axis length {axis_len}",
            start + len
        )));
    }
    let outer: usize = arr.shape[..count_axis].iter().product();
    let inner_bytes: usize =
        arr.shape[count_axis + 1..].iter().product::<usize>() * arr.dtype.size();
    let run = len * inner_bytes;
    out.reserve(outer * run);
    for o in 0..outer {
        let src = (o * axis_len + start) * inner_bytes;
        out.extend_from_slice(&arr.bytes[src..src + run]);
    }
    Ok(())
}
