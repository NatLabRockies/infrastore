//! Storage backend abstraction.
//!
//! The [`StorageBackend`] trait is the only seam between the public API and
//! the actual array-storage implementation. v0 ships two implementations:
//! [`MemoryBackend`] (in-memory) and [`NetCdfBackend`] (NetCDF4 on disk).

use std::ops::Range;

use crate::error::{Result, TimeSeriesError};
use crate::types::array::TypedArray;

pub mod memory;
pub mod netcdf;

pub use memory::MemoryBackend;
pub use netcdf::NetCdfBackend;

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
    /// data is reused for content addressing) and `false` is returned; a write
    /// of new content returns `true`. The array's dtype + shape travel with it;
    /// `resolution_ms` keys the packed storage pool.
    ///
    /// `packed = true` column-packs the array with other same-shaped arrays (for
    /// SingleTimeSeries / DST); `packed = false` stores it as a standalone
    /// multi-dimensional variable (for irregular series and native forecasts).
    fn put_array(
        &mut self,
        hash: &[u8; 32],
        data: &TypedArray,
        resolution_ms: i64,
        packed: bool,
    ) -> Result<bool>;

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

    /// The compression policy applied to newly written arrays. In-memory
    /// backends report [`Compression::None`] since they never compress; the
    /// NetCDF backend reports the policy it was created or reopened with.
    fn compression(&self) -> Compression {
        Compression::None
    }
}
