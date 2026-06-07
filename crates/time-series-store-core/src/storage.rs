//! Storage backend abstraction.
//!
//! The [`StorageBackend`] trait is the only seam between the public API and
//! the actual array-storage implementation. v0 ships two implementations:
//! [`MemoryBackend`] (in-memory) and [`NetCdfBackend`] (NetCDF4 on disk).

use std::ops::Range;

use crate::error::Result;
use crate::types::array::TypedArray;

pub mod memory;
pub mod netcdf;

pub use memory::MemoryBackend;
pub use netcdf::NetCdfBackend;

#[derive(Debug, Default, Clone)]
pub struct CompactionReport {
    pub slots_reclaimed: usize,
    pub datasets_dropped: usize,
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
pub trait StorageBackend: Send + Sync {
    /// Insert an array. If `hash` already exists, this is a no-op (the existing
    /// data is reused for content addressing). The array's dtype + shape travel
    /// with it; `resolution_seconds` keys the packed storage pool.
    ///
    /// `packed = true` column-packs the array with other same-shaped arrays (for
    /// SingleTimeSeries / DST); `packed = false` stores it as a standalone
    /// multi-dimensional variable (for irregular series and native forecasts).
    fn put_array(
        &mut self,
        hash: &[u8; 32],
        data: &TypedArray,
        resolution_seconds: i64,
        packed: bool,
    ) -> Result<()>;

    /// Fetch the full array for `hash`.
    fn get_array(&self, hash: &[u8; 32]) -> Result<TypedArray>;

    /// Fetch a slice of the array along axis 0 (the time axis). End is exclusive.
    fn get_slice(&self, hash: &[u8; 32], range: Range<usize>) -> Result<TypedArray>;

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
}
