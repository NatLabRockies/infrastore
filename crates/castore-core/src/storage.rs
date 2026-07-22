//! Storage backend abstraction.
//!
//! The [`StorageBackend`] trait is the only seam between the public API and
//! the actual array-storage implementation. v0 ships two implementations:
//! [`MemoryBackend`] (in-memory) and [`NetCdfBackend`] (NetCDF4 on disk).

use std::ops::Range;

use crate::error::{Result, TimeSeriesError};
use crate::types::array::{Dtype, TypedArray};
use crate::types::period::Period;

pub mod memory;
pub mod netcdf;

// The concrete backends and the trait seam are internal: the public surface is
// `Store`, which owns a boxed backend. (The `netcdf` module stays `pub` so
// white-box tests can reach `DEFAULT_COLS_PER_DATASET`.)
pub(crate) use memory::MemoryBackend;
pub(crate) use netcdf::NetCdfBackend;

/// Compression filter applied to NetCDF4 data variables when they are created.
///
/// This is a write-time storage policy: it controls how arrays are encoded on
/// disk but never affects the logical data, which is decoded transparently by
/// NetCDF/HDF5 on read regardless of the filter used. Stores created with
/// different settings therefore remain mutually readable, and the on-disk
/// [`DATA_FORMAT_VERSION`](crate::version::DATA_FORMAT_VERSION) is unaffected.
///
/// The chosen policy is persisted as a global attribute so that appends made
/// after re-opening a store reuse the same filter (see
/// [`NetCdfBackend::open`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// No compression filter is applied; arrays are stored uncompressed.
    None,
    /// DEFLATE (zlib) at `level` (0–9), optionally preceded by the byte-shuffle
    /// filter, which usually improves the ratio for numeric data.
    Deflate { level: u8, shuffle: bool },
}

impl Default for Compression {
    /// Matches the historical hard-coded behaviour: DEFLATE level 3 + shuffle.
    fn default() -> Self {
        Compression::Deflate {
            level: 3,
            shuffle: true,
        }
    }
}

impl Compression {
    /// Reject DEFLATE levels outside the NetCDF-supported 0–9 range.
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

    /// Encode as a stable string for persistence in a NetCDF global attribute.
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

#[derive(Debug, Default, Clone)]
pub struct CompactionReport {
    pub slots_reclaimed: usize,
    pub datasets_dropped: usize,
    /// Content-addressed feature sets in the SQLite catalog that no association
    /// referenced any more, and were deleted. Feature sets are shared, so
    /// removing an association cannot cascade-delete them; they accumulate as
    /// unreachable rows until a compaction sweeps them, exactly as deleted
    /// arrays leave unreachable NetCDF variables behind.
    pub feature_sets_reclaimed: usize,
}

#[derive(Debug, Default, Clone)]
pub struct IntegrityReport {
    pub errors: Vec<String>,
}

impl IntegrityReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Pluggable array-storage backend.
///
/// Each array is identified by its 32-byte content hash. Implementations are
/// responsible for any deduplication, slot management, or compaction; the
/// `Store` layer above drives them through this trait.
pub(crate) trait StorageBackend: Send + Sync {
    /// Insert an array. If `hash` already exists, this is a no-op (the existing
    /// data is reused for content addressing) and `false` is returned; a write
    /// of new content returns `true`. The array's dtype + shape travel with it;
    /// `resolution` keys the packed storage pool.
    ///
    /// `packed = true` column-packs the array with other same-shaped arrays (for
    /// SingleTimeSeries / DST); `packed = false` stores it as a standalone
    /// multi-dimensional variable (for irregular series and native forecasts).
    fn put_array(
        &mut self,
        hash: &[u8; 32],
        data: &TypedArray,
        resolution: Period,
        packed: bool,
    ) -> Result<bool>;

    /// Insert a block of same-shaped packed arrays in one operation.
    ///
    /// `hashes[i]` is the content hash of `arrays[i]`; every array must share one
    /// `(dtype, element_shape, length)` and the given `resolution` (the caller —
    /// the buffered bulk-add — guarantees this by grouping). Hashes already
    /// stored, and duplicates within the block, are written only once (content
    /// addressing); the returned `Vec<bool>` is aligned to `hashes` and is `true`
    /// for each input that this call physically wrote (so the caller can stage it
    /// for rollback).
    ///
    /// The default loops [`Self::put_array`] with `packed = true`. The NetCDF
    /// backend overrides it to create batch-sized datasets and fill whole chunks
    /// with one timestamp-row write per chunk, avoiding the per-column
    /// read-modify-write that the timestamp-major chunking imposes on single adds.
    fn put_packed_block(
        &mut self,
        hashes: &[[u8; 32]],
        arrays: &[&TypedArray],
        resolution: Period,
    ) -> Result<Vec<bool>> {
        hashes
            .iter()
            .zip(arrays)
            .map(|(hash, data)| self.put_array(hash, data, resolution, true))
            .collect()
    }

    /// Fetch the full array for `hash`.
    fn get_array(&self, hash: &[u8; 32]) -> Result<TypedArray>;

    /// Read many full arrays at once, returning one [`TypedArray`] per input hash
    /// in order (duplicate hashes each yield a copy).
    ///
    /// The default loops [`Self::get_array`]. The NetCDF backend overrides it to
    /// read each packed dataset's needed column span in a single hyperslab —
    /// decompressing each timestamp-major chunk once — then scatter the columns
    /// out, rather than re-reading every chunk once per series. This is the bulk
    /// counterpart to the per-timestamp [`Self::read_index_into`]: it amortizes
    /// the decompression cost of whole-series reads across a batch.
    fn read_arrays(&self, hashes: &[[u8; 32]]) -> Result<Vec<TypedArray>> {
        hashes.iter().map(|h| self.get_array(h)).collect()
    }

    /// Fetch a slice of the array along axis 0 (the time axis). End is exclusive.
    fn get_slice(&self, hash: &[u8; 32], range: Range<usize>) -> Result<TypedArray>;

    /// Read the value at a single time `index` for a set of co-located arrays,
    /// appending the result to `out` in `hashes` order.
    ///
    /// All `hashes` must share one `(dtype, element_shape)`; the caller (the
    /// timestamp reader) guarantees this by grouping. `out` is cleared first and
    /// then filled with `hashes.len() * element_count * dtype.size()` bytes laid
    /// out row-major as `[column, *element_shape]`. Reusing the caller's buffer
    /// keeps a per-timestamp read loop allocation-free.
    ///
    /// The default reads each array's one-step slice individually; the NetCDF
    /// backend overrides this to read a whole packed-dataset row per hyperslab.
    fn read_index_into(&self, hashes: &[[u8; 32]], index: usize, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        for hash in hashes {
            let step = self.get_slice(hash, index..index + 1)?;
            out.extend_from_slice(&step.bytes);
        }
        Ok(())
    }

    /// The stored `(dtype, shape)` of an array, ideally without reading its data.
    /// Used by the forecast reader to plan window slicing. The default reads the
    /// whole array; the NetCDF backend overrides it to inspect dimensions only.
    fn array_shape(&self, hash: &[u8; 32]) -> Result<(Dtype, Vec<usize>)> {
        let arr = self.get_array(hash)?;
        Ok((arr.dtype, arr.shape))
    }

    /// Read a single forecast window: the `window_index` slice along `count_axis`
    /// of a standalone array, with that axis removed. `out` is cleared then
    /// filled with the window's row-major, little-endian bytes. Reusing the
    /// caller's buffer keeps a per-timestamp forecast loop allocation-free.
    ///
    /// The default materializes the whole array and copies out one window; the
    /// NetCDF backend overrides this to read just the window with a hyperslab.
    fn read_window_into(
        &self,
        hash: &[u8; 32],
        count_axis: usize,
        window_index: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let arr = self.get_array(hash)?;
        out.clear();
        write_window(&arr, count_axis, window_index, out)
    }

    /// Read `len` consecutive time steps along axis 0 starting at `start`,
    /// filling `out` (cleared first) with their row-major, little-endian bytes.
    /// Backs `DeterministicSingleTimeSeries` window reads, which gather a
    /// contiguous run from the packed underlying `SingleTimeSeries`; on the
    /// NetCDF backend [`Self::get_slice`] is already a single packed hyperslab.
    fn read_range_into(
        &self,
        hash: &[u8; 32],
        start: usize,
        len: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let slice = self.get_slice(hash, start..start + len)?;
        out.clear();
        out.extend_from_slice(&slice.bytes);
        Ok(())
    }

    /// Remove an array. Marks the slot reusable. No-op if `hash` is absent.
    fn remove_array(&mut self, hash: &[u8; 32]) -> Result<()>;

    /// True iff the backend currently stores `hash`.
    fn contains(&self, hash: &[u8; 32]) -> Result<bool>;

    /// Reclaim space from removed arrays.
    fn compact(&mut self) -> Result<CompactionReport>;

    /// Validate stored hashes match recomputed hashes of stored data.
    fn verify(&self) -> Result<IntegrityReport>;

    /// Flush any in-memory state to disk (no-op for in-memory backends).
    fn flush(&mut self) -> Result<()>;

    /// The compression policy applied to newly written arrays. In-memory
    /// backends report [`Compression::None`] since they never compress; the
    /// NetCDF backend reports the policy it was created or reopened with.
    fn compression(&self) -> Compression {
        Compression::None
    }
}

/// Append the bytes of one window — the slice at `w` along `count_axis`, with
/// that axis removed — to `out`. The size-1 axis contributes nothing to the
/// row-major layout, so the gathered bytes are exactly the window in order.
/// Shared by the default [`StorageBackend::read_window_into`].
fn write_window(arr: &TypedArray, count_axis: usize, w: usize, out: &mut Vec<u8>) -> Result<()> {
    if count_axis >= arr.shape.len() {
        return Err(TimeSeriesError::IntegrityError(format!(
            "count axis {count_axis} out of bounds for shape {:?}",
            arr.shape
        )));
    }
    let axis_len = arr.shape[count_axis];
    if w >= axis_len {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "window index {w} out of bounds for axis length {axis_len}"
        )));
    }
    let outer: usize = arr.shape[..count_axis].iter().product();
    let inner_bytes: usize =
        arr.shape[count_axis + 1..].iter().product::<usize>() * arr.dtype.size();
    out.reserve(outer * inner_bytes);
    for o in 0..outer {
        let start = (o * axis_len + w) * inner_bytes;
        out.extend_from_slice(&arr.bytes[start..start + inner_bytes]);
    }
    Ok(())
}
