use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;

use crate::error::{Result, TimeSeriesError};
use crate::hash::array_hash;
use crate::types::array::TypedArray;

use super::{CompactionReport, IntegrityReport, StorageBackend, slice_axis};

/// Pure in-memory storage backend.
///
/// Used for `in_memory=true` stores and as the default test backend. Tracks a
/// "tombstoned" set so the slot-reclamation behaviour can be exercised against
/// the same surface as the NetCDF backend.
#[derive(Debug, Default)]
pub struct MemoryBackend {
    arrays: HashMap<[u8; 32], TypedArray>,
    tombstoned: HashSet<[u8; 32]>,
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
        _resolution_ms: i64,
        _packed: bool,
    ) -> Result<bool> {
        // If the slot was tombstoned, "reuse" it by clearing the marker.
        self.tombstoned.remove(hash);
        if self.arrays.contains_key(hash) {
            return Ok(false);
        }
        self.arrays.insert(*hash, data.clone());
        Ok(true)
    }

    fn get_array(&self, hash: &[u8; 32]) -> Result<TypedArray> {
        self.arrays
            .get(hash)
            .cloned()
            .ok_or(TimeSeriesError::NotFound)
    }

    fn get_slice(&self, hash: &[u8; 32], range: Range<usize>) -> Result<TypedArray> {
        let array = self.arrays.get(hash).ok_or(TimeSeriesError::NotFound)?;
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

    fn get_axis_slice(
        &self,
        hash: &[u8; 32],
        axis: usize,
        range: Range<usize>,
    ) -> Result<TypedArray> {
        let array = self.arrays.get(hash).ok_or(TimeSeriesError::NotFound)?;
        slice_axis(array, axis, range)
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

    fn compact(&mut self) -> Result<CompactionReport> {
        let reclaimed = self.tombstoned.len();
        self.tombstoned.clear();
        Ok(CompactionReport {
            slots_reclaimed: reclaimed,
            datasets_dropped: 0,
        })
    }

    fn verify(&self) -> Result<IntegrityReport> {
        let mut errors = Vec::new();
        for (hash, data) in &self.arrays {
            let recomputed = array_hash(data);
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
