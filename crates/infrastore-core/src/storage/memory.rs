use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;

use chrono::{DateTime, Utc};

use crate::error::{Result, TimeSeriesError};
use crate::hash::array_hash;
use crate::types::array::{Dtype, TypedArray};

use super::{ArrayLayout, CompactionReport, IntegrityReport, PackGroup, StorageBackend};

/// Pure in-memory storage backend.
///
/// Used for `in_memory=true` stores and as the default test backend. Tracks a
/// "tombstoned" set so the slot-reclamation behavior can be exercised against
/// the same surface as the HDF5 backend.
#[derive(Debug, Default)]
pub(crate) struct MemoryBackend {
    arrays: HashMap<[u8; 32], TypedArray>,
    tombstoned: HashSet<[u8; 32]>,
    /// Explicit timestamp vectors, held in the stored form the on-disk backend
    /// writes (unix milliseconds) rather than as `DateTime`s, so an in-memory
    /// store and a persisted one hand back byte-identical values.
    timestamps: HashMap<[u8; 32], Vec<i64>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl StorageBackend for MemoryBackend {
    fn put_array(
        &mut self,
        hash: &[u8; 32],
        data: &TypedArray,
        _group: PackGroup,
        _layout: ArrayLayout,
    ) -> Result<bool> {
        // If the slot was tombstoned, "reuse" it by clearing the marker.
        self.tombstoned.remove(hash);
        if self.arrays.contains_key(hash) {
            return Ok(false);
        }
        self.arrays.insert(*hash, data.clone());
        Ok(true)
    }

    fn get_array(&self, hash: &[u8; 32], dtype: Dtype) -> Result<TypedArray> {
        let array = self.arrays.get(hash).ok_or(TimeSeriesError::NotFound)?;
        // This backend keeps whole `TypedArray`s, so it knows the dtype itself;
        // the caller's is an assertion that the catalog agrees.
        super::check_dtype(hash, array.dtype, dtype)?;
        Ok(array.clone())
    }

    fn array_shape(&self, hash: &[u8; 32]) -> Result<Vec<usize>> {
        self.arrays
            .get(hash)
            .map(|a| a.shape.clone())
            .ok_or(TimeSeriesError::NotFound)
    }

    fn get_slice(&self, hash: &[u8; 32], dtype: Dtype, range: Range<usize>) -> Result<TypedArray> {
        let array = self.arrays.get(hash).ok_or(TimeSeriesError::NotFound)?;
        super::check_dtype(hash, array.dtype, dtype)?;
        let len = array.length();
        if range.start > range.end || range.end > len {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "slice {:?} out of bounds for length {}",
                range, len
            )));
        }
        // Bytes per time step = product(element_shape) * element_size.
        let row_bytes = array.element_shape().iter().product::<usize>() * array.dtype.size();
        let bytes = array.bytes[range.start * row_bytes..range.end * row_bytes].to_vec();
        let mut shape = array.shape.clone();
        if let Some(first) = shape.first_mut() {
            *first = range.end - range.start;
        }
        Ok(TypedArray {
            dtype: array.dtype,
            shape,
            bytes,
        })
    }

    fn remove_array(&mut self, hash: &[u8; 32]) -> Result<()> {
        if self.arrays.remove(hash).is_some() {
            self.tombstoned.insert(*hash);
        }
        Ok(())
    }

    fn contains(&self, hash: &[u8; 32]) -> Result<bool> {
        Ok(self.arrays.contains_key(hash))
    }

    fn put_timestamps(&mut self, hash: &[u8; 32], timestamps: &[DateTime<Utc>]) -> Result<bool> {
        if self.timestamps.contains_key(hash) {
            return Ok(false);
        }
        self.timestamps
            .insert(*hash, crate::timestamps::to_millis(timestamps));
        Ok(true)
    }

    fn get_timestamps(&self, hash: &[u8; 32]) -> Result<Vec<DateTime<Utc>>> {
        let millis = self.timestamps.get(hash).ok_or(TimeSeriesError::NotFound)?;
        crate::timestamps::from_millis(millis)
    }

    fn remove_timestamps(&mut self, hash: &[u8; 32]) -> Result<()> {
        self.timestamps.remove(hash);
        Ok(())
    }

    fn timestamp_hashes(&self) -> Result<Vec<[u8; 32]>> {
        Ok(self.timestamps.keys().copied().collect())
    }

    fn compact(&mut self) -> Result<CompactionReport> {
        let reclaimed = self.tombstoned.len();
        self.tombstoned.clear();
        Ok(CompactionReport {
            slots_reclaimed: reclaimed,
            datasets_dropped: 0,
            // The catalog is not the backend's to sweep; `Store::compact` fills
            // this in after the array side is done.
            feature_sets_reclaimed: 0,
            timestamp_sets_reclaimed: 0,
            // Nothing on disk to shrink.
            bytes_reclaimed: 0,
        })
    }

    fn verify(&self, arrays: &[([u8; 32], Dtype)]) -> Result<IntegrityReport> {
        let mut errors = Vec::new();
        for (hash, dtype) in arrays {
            let data = match self.get_array(hash, *dtype) {
                Ok(data) => data,
                Err(TimeSeriesError::NotFound) => {
                    errors.push(format!(
                        "dangling reference: the catalog references array {} but the array \
                         store does not hold it",
                        crate::hash::hash_hex(hash),
                    ));
                    continue;
                }
                Err(e) => {
                    errors.push(format!(
                        "read error for array {}: {e}",
                        crate::hash::hash_hex(hash)
                    ));
                    continue;
                }
            };
            let recomputed = array_hash(&data);
            if &recomputed != hash {
                errors.push(format!(
                    "hash mismatch: stored={} computed={}",
                    crate::hash::hash_hex(hash),
                    crate::hash::hash_hex(&recomputed),
                ));
            }
        }
        Ok(IntegrityReport { errors })
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
