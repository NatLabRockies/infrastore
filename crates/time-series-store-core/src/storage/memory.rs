use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;

use ndarray::{ArrayD, Axis};

use crate::error::{Result, TimeSeriesError};
use crate::hash::array_hash;

use super::{CompactionReport, IntegrityReport, StorageBackend};

/// Pure in-memory storage backend.
///
/// Used for `in_memory=true` stores and as the default test backend. Tracks a
/// "tombstoned" set so the slot-reclamation behaviour can be exercised against
/// the same surface as the NetCDF backend that lands in M1.
#[derive(Debug, Default)]
pub struct MemoryBackend {
    arrays: HashMap<[u8; 32], ArrayD<f64>>,
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
        data: &ArrayD<f64>,
        _length: usize,
        _resolution_seconds: i64,
    ) -> Result<()> {
        // If the slot was tombstoned, "reuse" it by clearing the marker.
        self.tombstoned.remove(hash);
        self.arrays.entry(*hash).or_insert_with(|| data.clone());
        Ok(())
    }

    fn get_array(&self, hash: &[u8; 32]) -> Result<ArrayD<f64>> {
        self.arrays
            .get(hash)
            .cloned()
            .ok_or(TimeSeriesError::NotFound)
    }

    fn get_slice(&self, hash: &[u8; 32], range: Range<usize>) -> Result<ArrayD<f64>> {
        let array = self
            .arrays
            .get(hash)
            .ok_or(TimeSeriesError::NotFound)?;
        let len = array.shape().first().copied().unwrap_or(0);
        if range.start > range.end || range.end > len {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "slice {:?} out of bounds for length {}",
                range, len
            )));
        }
        Ok(array
            .slice_axis(Axis(0), ndarray::Slice::from(range.start..range.end))
            .to_owned())
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
