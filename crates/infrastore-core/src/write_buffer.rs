//! The packed arrays an open transaction has accepted and not yet written.
//!
//! Nothing a transaction writes is durable until its outermost commit, so a
//! packed single add inside one owes the file nothing yet. Instead of filling
//! one slot of a growth pool — a read-modify-write of every timestamp-row chunk
//! in it, for one column — the array joins a block for its pool, and the block
//! is written with the same block writer a bulk add uses. A loop of single adds
//! inside one transaction therefore produces the datasets one bulk add of the
//! same items would: same names, same widths, same chunking.
//!
//! The buffer is the store's, not the backend's. A backend knows physical
//! positions; "accepted but not yet positioned" is transaction state, and the
//! store is what opens, commits, and rolls transactions back. The backend
//! never sees a buffered array until the block it belongs to is written.
//!
//! The arrays are owned copies, so an open transaction holds its unwritten
//! arrays in memory. That is bounded twice: per pool at the width the block
//! writer spills a batch at ([`WriteBuffer::width_cap`]), and across every pool
//! at a byte budget ([`MAX_PENDING_BYTES`] by default,
//! [`crate::Store::set_write_buffer_bytes`] to move it). Crossing either writes
//! whole blocks out early, which costs an extra dataset and nothing else — the
//! same spill a batch wider than one chunk already performs.

use std::collections::HashMap;

use crate::error::Result;
use crate::storage::common::{MAX_PENDING_BYTES, element_block_bytes, resolve_dataset_cols};
use crate::storage::{ArrayLayout, PackGroup, StorageBackend};
use crate::types::array::{Dtype, TypedArray};

/// What a packed pool is keyed by: the array's physical shape plus the time
/// axis it lies on. Two arrays land in the same HDF5 dataset iff these match.
pub(crate) type PoolKey = (Dtype, Vec<usize>, usize, PackGroup);

pub(crate) fn pool_key(array: &TypedArray, group: PackGroup) -> PoolKey {
    (
        array.dtype,
        array.element_shape().to_vec(),
        array.length(),
        group,
    )
}

/// One pool's unwritten arrays, in arrival order — which is the column order
/// they take in the dataset, so N single adds and one bulk add of the same N
/// items produce identical files.
#[derive(Debug, Default)]
struct Block {
    hashes: Vec<[u8; 32]>,
    arrays: Vec<TypedArray>,
    /// Running sum of `arrays[i].bytes.len()`.
    bytes: usize,
}

#[derive(Debug)]
pub(crate) struct WriteBuffer {
    blocks: HashMap<PoolKey, Block>,
    /// Each buffered hash's pool and its position in that pool's block.
    by_hash: HashMap<[u8; 32], (PoolKey, usize)>,
    /// `blocks.values().map(|b| b.bytes).sum()`, maintained rather than
    /// recomputed.
    bytes: usize,
    /// The ceiling `bytes` is held to.
    max_bytes: usize,
}

impl Default for WriteBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteBuffer {
    pub(crate) fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            by_hash: HashMap::new(),
            bytes: 0,
            max_bytes: MAX_PENDING_BYTES,
        }
    }

    pub(crate) fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Set the byte budget. A buffer already over the new figure is written
    /// out before this returns, so the bound holds from here rather than from
    /// the next add. That is the one case this does I/O, and the one case it
    /// can fail.
    pub(crate) fn set_max_bytes(
        &mut self,
        bytes: usize,
        backend: &mut dyn StorageBackend,
    ) -> Result<()> {
        self.max_bytes = bytes;
        self.evict_over_budget(backend)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    pub(crate) fn contains(&self, hash: &[u8; 32]) -> bool {
        self.by_hash.contains_key(hash)
    }

    /// The buffered array for `hash`, if it is buffered.
    pub(crate) fn get(&self, hash: &[u8; 32]) -> Option<&TypedArray> {
        let (key, idx) = self.by_hash.get(hash)?;
        self.blocks.get(key)?.arrays.get(*idx)
    }

    /// Accept an array into its pool's block. The caller has established that
    /// neither the buffer nor the backend holds `hash`.
    ///
    /// The width cap is checked first, so a pool that is full writes *its own*
    /// block rather than whichever one the byte budget happens to pick. If
    /// either write fails, the array is taken back out wherever it ended up —
    /// still buffered if its own block failed, or written if its block landed
    /// and some other pool's eviction is what failed — so a call that reports
    /// an error has stored nothing. Arrays buffered by earlier calls stay.
    pub(crate) fn push(
        &mut self,
        hash: [u8; 32],
        array: &TypedArray,
        group: PackGroup,
        backend: &mut dyn StorageBackend,
    ) -> Result<()> {
        let key = pool_key(array, group);
        let cap = self.width_cap(key.0, &key.1, key.2);
        let bytes = array.bytes.len();
        let block = self.blocks.entry(key.clone()).or_default();
        let idx = block.hashes.len();
        block.hashes.push(hash);
        block.arrays.push(array.clone());
        block.bytes += bytes;
        let full = block.hashes.len() >= cap;
        self.bytes += bytes;
        self.by_hash.insert(hash, (key.clone(), idx));
        let spilled = if full {
            self.write_block(&key, backend)
        } else {
            Ok(())
        };
        if let Err(e) = spilled.and_then(|()| self.evict_over_budget(backend)) {
            if !self.remove(&hash) {
                let _ = backend.remove_array(&hash);
            }
            return Err(e);
        }
        Ok(())
    }

    /// Drop a buffered array. `false` if `hash` was not buffered.
    ///
    /// `swap_remove` rather than `remove`: a rollback removes a block's arrays
    /// one at a time, and shifting the tail on each would be quadratic in the
    /// transaction's size. It reorders the survivors, which only decides which
    /// column they land in — and a rolled-back span has no column order to
    /// preserve.
    pub(crate) fn remove(&mut self, hash: &[u8; 32]) -> bool {
        let Some((key, idx)) = self.by_hash.remove(hash) else {
            return false;
        };
        let Some(block) = self.blocks.get_mut(&key) else {
            return true;
        };
        block.hashes.swap_remove(idx);
        let dropped = block.arrays.swap_remove(idx).bytes.len();
        block.bytes = block.bytes.saturating_sub(dropped);
        self.bytes = self.bytes.saturating_sub(dropped);
        if let Some(&moved) = block.hashes.get(idx) {
            self.by_hash.insert(moved, (key.clone(), idx));
        }
        if block.hashes.is_empty() {
            self.blocks.remove(&key);
        }
        true
    }

    /// Write every block. Stops at the first failure with the rest of the
    /// buffer intact, for the reason [`Self::write_block`] gives.
    pub(crate) fn write_all(&mut self, backend: &mut dyn StorageBackend) -> Result<()> {
        while let Some(key) = self.blocks.keys().next().cloned() {
            self.write_block(&key, backend)?;
        }
        Ok(())
    }

    /// Write the blocks holding any of `hashes`, so the caller can resolve them
    /// to a physical position.
    pub(crate) fn write_holding(
        &mut self,
        hashes: impl IntoIterator<Item = [u8; 32]>,
        backend: &mut dyn StorageBackend,
    ) -> Result<()> {
        let mut keys: Vec<PoolKey> = Vec::new();
        for hash in hashes {
            if let Some((key, _)) = self.by_hash.get(&hash)
                && !keys.contains(key)
            {
                keys.push(key.clone());
            }
        }
        for key in &keys {
            self.write_block(key, backend)?;
        }
        Ok(())
    }

    /// The width one block grows to before it is written out: the lower of the
    /// per-chunk byte budget over one column's element block — the width the
    /// block writer itself spills a batch at, so a span produces the datasets a
    /// bulk add would — and this pool's share of the byte budget, since a
    /// column's cost is `length × element_block` and a thousand columns of a
    /// multi-year series is hundreds of megabytes.
    ///
    /// Not capped at `DEFAULT_COLS_PER_DATASET`, the growth pool's width: that
    /// would cut a bulk add issued inside a transaction into growth-pool-sized
    /// pieces, and a columnar read pays one chunk per dataset per timestep.
    pub(crate) fn width_cap(&self, dtype: Dtype, element_shape: &[usize], length: usize) -> usize {
        let per_column = length
            .saturating_mul(element_block_bytes(dtype, element_shape))
            .max(1);
        let by_bytes = (self.max_bytes / per_column).max(1);
        resolve_dataset_cols(Some(usize::MAX), dtype, element_shape).min(by_bytes)
    }

    /// Write one pool's block and drop it from the buffer. A no-op if the pool
    /// has no block.
    ///
    /// A block of two or more goes to the block writer, which sizes a fresh dataset
    /// to it — the point of buffering. **A block of one fills a growth-pool slot
    /// instead**, exactly as an un-transactioned single add does, and for the
    /// reason `add_time_series_bulk` gives for a batch of one: a dataset sized to
    /// one column is a dataset and a hash companion per series, and a columnar read
    /// pays one hyperslab per dataset per timestep, where series sharing a growth
    /// pool share one. **An irregular block of one is a standalone array** unless
    /// the file already holds a pool for its axis: packing is a bet the pool will
    /// be wider than one column, and this is where the block's final membership
    /// settles it for a span.
    ///
    /// On failure the block goes back exactly as it was: the transaction that
    /// owns these writes is still open, and both committing again and rolling
    /// back have to be able to find them.
    fn write_block(&mut self, key: &PoolKey, backend: &mut dyn StorageBackend) -> Result<()> {
        let Some(block) = self.blocks.remove(key) else {
            return Ok(());
        };
        self.bytes = self.bytes.saturating_sub(block.bytes);
        let group = key.3;
        let outcome = if block.hashes.len() == 1 {
            let layout = if matches!(group, PackGroup::Irregular(_))
                && !backend.has_pack_group(key.0, &key.1, key.2, group)
            {
                ArrayLayout::Standalone
            } else {
                ArrayLayout::Packed
            };
            backend
                .put_array(&block.hashes[0], &block.arrays[0], group, layout)
                .map(|_| ())
        } else {
            let arrays: Vec<&TypedArray> = block.arrays.iter().collect();
            backend
                .put_packed_block(&block.hashes, &arrays, group)
                .map(|_| ())
        };
        match outcome {
            Ok(()) => {
                for hash in &block.hashes {
                    self.by_hash.remove(hash);
                }
                Ok(())
            }
            Err(e) => {
                self.bytes += block.bytes;
                self.blocks.insert(key.clone(), block);
                Err(e)
            }
        }
    }

    /// Write out whole blocks, widest first, until the buffer is back inside
    /// its byte budget. Widest first because it frees the most memory for the
    /// fewest datasets, and a wide block is exactly the one worth its own
    /// dataset; the narrow ones are left to keep growing.
    fn evict_over_budget(&mut self, backend: &mut dyn StorageBackend) -> Result<()> {
        while self.bytes > self.max_bytes {
            let Some(key) = self
                .blocks
                .iter()
                .max_by_key(|(_, block)| block.bytes)
                .map(|(key, _)| key.clone())
            else {
                return Ok(());
            };
            self.write_block(&key, backend)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::array_hash;
    use crate::storage::common::{
        DEFAULT_COLS_PER_DATASET, MAX_CHUNK_BYTES, ROOT_GROUP, SINGLE_GROUP, dataset_base_name,
    };
    use crate::storage::hdf5::Hdf5Backend;
    use crate::storage::{ArrayLocation, Compression};
    use crate::types::period::Period;

    fn f64_array(shape: Vec<usize>, seed: f64) -> TypedArray {
        let n: usize = shape.iter().product();
        let values: Vec<f64> = (0..n).map(|i| seed + i as f64).collect();
        TypedArray::from_f64(shape, &values)
    }

    fn res() -> PackGroup {
        PackGroup::Regular(Period::fixed(chrono::Duration::hours(1)))
    }

    fn backend(dir: &std::path::Path, name: &str) -> Hdf5Backend {
        Hdf5Backend::create(&dir.join(name), Compression::None).unwrap()
    }

    fn occupy_dataset_name(be: &Hdf5Backend, dtype: Dtype, shape: &[usize], length: usize) {
        be.occupy_name_for_test(&dataset_base_name(dtype, shape, length, res()));
    }

    /// A block whose write fails stays buffered, readable, and removable, and
    /// nothing of it reaches the file.
    #[test]
    fn a_failed_write_keeps_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = backend(dir.path(), "s.h5");
        let a = f64_array(vec![4], 0.0);
        let ha = array_hash(&a);
        let mut buf = WriteBuffer::new();
        buf.push(ha, &a, res(), &mut be).unwrap();
        assert!(buf.contains(&ha));
        assert_eq!(buf.get(&ha), Some(&a));
        assert!(!be.contains(&ha).unwrap());

        occupy_dataset_name(&be, Dtype::F64, &[], 4);
        assert!(buf.write_all(&mut be).is_err(), "the dataset name is taken");

        assert_eq!(buf.blocks.len(), 1);
        assert_eq!(buf.get(&ha), Some(&a));
        assert!(buf.remove(&ha));
        assert!(buf.is_empty());
        assert_eq!(buf.bytes, 0);
        buf.write_all(&mut be).unwrap();
    }

    /// A push whose block has to spill at once, and cannot, reports an error
    /// and has stored nothing — the caller stages for rollback only what a put
    /// said it wrote. An element block of exactly the chunk budget caps the
    /// pool at one column, so the very first push writes.
    #[test]
    fn a_push_that_cannot_spill_stores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = backend(dir.path(), "s.h5");
        let elements = MAX_CHUNK_BYTES / std::mem::size_of::<f64>();
        let a = f64_array(vec![1, elements], 0.0);
        let ha = array_hash(&a);
        occupy_dataset_name(&be, Dtype::F64, &[elements], 1);
        let mut buf = WriteBuffer::new();
        assert!(buf.push(ha, &a, res(), &mut be).is_err());
        assert!(!buf.contains(&ha));
        assert!(buf.is_empty());
        assert!(!be.contains(&ha).unwrap());
    }

    /// Pushes accumulate into one dataset sized to the block, columns in
    /// arrival order, the same as a bulk block of the same arrays.
    #[test]
    fn pushes_accumulate_into_one_block_sized_dataset() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = backend(dir.path(), "s.h5");
        let arrays: Vec<TypedArray> = (0..5).map(|i| f64_array(vec![6], i as f64)).collect();
        let hashes: Vec<[u8; 32]> = arrays.iter().map(array_hash).collect();
        let mut buf = WriteBuffer::new();
        for (hash, array) in hashes.iter().zip(&arrays) {
            buf.push(*hash, array, res(), &mut be).unwrap();
        }
        buf.write_all(&mut be).unwrap();
        assert!(buf.is_empty());

        let name = dataset_base_name(Dtype::F64, &[], 6, res());
        assert_eq!(
            be.dataset_shape_for_test(&name),
            (vec![6, 5], Some(vec![6, 5]))
        );
        for (i, (hash, array)) in hashes.iter().zip(&arrays).enumerate() {
            assert_eq!(
                be.locate(hash).unwrap(),
                ArrayLocation::Packed {
                    dataset: format!("/{ROOT_GROUP}/{SINGLE_GROUP}/{name}"),
                    column: i,
                }
            );
            assert_eq!(&be.get_array(hash, Dtype::F64).unwrap(), array);
        }
    }

    /// A block that never grew past one array fills a growth-pool slot instead
    /// of claiming a dataset sized to it; from two up it gets its own.
    #[test]
    fn a_block_of_one_fills_a_slot_and_a_block_of_two_sizes_a_dataset() {
        let dir = tempfile::tempdir().unwrap();
        let name = dataset_base_name(Dtype::F64, &[], 6, res());

        let mut be = backend(dir.path(), "one.h5");
        let a = f64_array(vec![6], 0.0);
        let ha = array_hash(&a);
        let mut buf = WriteBuffer::new();
        buf.push(ha, &a, res(), &mut be).unwrap();
        buf.write_all(&mut be).unwrap();
        assert_eq!(
            be.dataset_shape_for_test(&name).0,
            vec![6, DEFAULT_COLS_PER_DATASET],
            "the growth pool, not a one-column dataset"
        );
        assert_eq!(be.get_array(&ha, Dtype::F64).unwrap(), a);

        let mut be2 = backend(dir.path(), "two.h5");
        let mut buf = WriteBuffer::new();
        for i in 0..2 {
            let array = f64_array(vec![6], i as f64);
            buf.push(array_hash(&array), &array, res(), &mut be2)
                .unwrap();
        }
        buf.write_all(&mut be2).unwrap();
        assert_eq!(be2.dataset_shape_for_test(&name).0, vec![6, 2]);
    }

    /// An irregular block of one is a standalone array unless the file already
    /// holds a pool for its axis.
    #[test]
    fn an_irregular_block_of_one_stands_alone_unless_a_pool_exists() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = backend(dir.path(), "s.h5");
        let axis = PackGroup::Irregular([7u8; 32]);
        let a = f64_array(vec![6], 0.0);
        let ha = array_hash(&a);
        let mut buf = WriteBuffer::new();
        buf.push(ha, &a, axis, &mut be).unwrap();
        buf.write_all(&mut be).unwrap();
        assert!(matches!(
            be.locate(&ha).unwrap(),
            ArrayLocation::Standalone { .. }
        ));

        // Two make a pool; a third, alone in its block, then joins it.
        let (b, c) = (f64_array(vec![6], 10.0), f64_array(vec![6], 20.0));
        for array in [&b, &c] {
            buf.push(array_hash(array), array, axis, &mut be).unwrap();
        }
        buf.write_all(&mut be).unwrap();
        let d = f64_array(vec![6], 30.0);
        let hd = array_hash(&d);
        buf.push(hd, &d, axis, &mut be).unwrap();
        buf.write_all(&mut be).unwrap();
        assert!(matches!(
            be.locate(&hd).unwrap(),
            ArrayLocation::Packed { .. }
        ));
    }

    /// The width cap is the chunk budget's own — never the growth pool's
    /// width — and shrinks below it only for a series long enough that a chunk
    /// row's worth of columns would not fit the byte budget.
    #[test]
    fn the_width_cap_is_the_chunk_budget_bounded_by_bytes() {
        let buf = WriteBuffer::new();
        assert_eq!(
            buf.width_cap(Dtype::F64, &[], 24),
            MAX_CHUNK_BYTES / std::mem::size_of::<f64>()
        );
        assert!(buf.width_cap(Dtype::F64, &[], 24) > DEFAULT_COLS_PER_DATASET);
        assert_eq!(buf.width_cap(Dtype::F64, &[1024], 2), 128);
        let long = MAX_PENDING_BYTES / std::mem::size_of::<f64>();
        assert_eq!(buf.width_cap(Dtype::F64, &[], long), 1);
        assert_eq!(
            buf.width_cap(Dtype::F64, &[], long / 4),
            4,
            "the byte budget divided by one column's bytes"
        );
    }

    /// Crossing the byte budget writes the widest block out, and the running
    /// total is exact through every path that touches the buffer.
    #[test]
    fn the_byte_budget_evicts_the_widest_block_and_the_total_stays_exact() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = backend(dir.path(), "s.h5");
        // Two pools: length 4 (32 bytes an array) and length 6 (48). Against a
        // 200-byte budget their width caps are 6 and 4, so neither pool fills
        // below — what fires here is the budget, not the width.
        let mut buf = WriteBuffer::new();
        buf.set_max_bytes(200, &mut be).unwrap();

        let short: Vec<TypedArray> = (0..4).map(|i| f64_array(vec![4], i as f64)).collect();
        let long: Vec<TypedArray> = (0..2)
            .map(|i| f64_array(vec![6], 50.0 + i as f64))
            .collect();
        let short_h: Vec<[u8; 32]> = short.iter().map(array_hash).collect();
        let long_h: Vec<[u8; 32]> = long.iter().map(array_hash).collect();

        for (hash, array) in short_h.iter().zip(&short).take(3) {
            buf.push(*hash, array, res(), &mut be).unwrap();
        }
        for (hash, array) in long_h.iter().zip(&long) {
            buf.push(*hash, array, res(), &mut be).unwrap();
        }
        assert_eq!(buf.bytes, 3 * 32 + 2 * 48);

        // 224 bytes against a 200-byte budget: the widest pool — now four
        // length-4 arrays — is written out, and the other is left to grow.
        buf.push(short_h[3], &short[3], res(), &mut be).unwrap();
        assert_eq!(buf.bytes, 2 * 48);
        assert_eq!(buf.blocks.len(), 1);
        assert_eq!(
            be.locate(&short_h[0]).unwrap(),
            ArrayLocation::Packed {
                dataset: format!(
                    "/{ROOT_GROUP}/{SINGLE_GROUP}/{}",
                    dataset_base_name(Dtype::F64, &[], 4, res())
                ),
                column: 0,
            }
        );
        assert_eq!(be.get_array(&short_h[3], Dtype::F64).unwrap(), short[3]);

        // Unwinding a buffered array gives its bytes back...
        assert!(buf.remove(&long_h[1]));
        assert_eq!(buf.bytes, 48);
        // ...and writing the rest out returns the total to zero.
        buf.write_all(&mut be).unwrap();
        assert_eq!(buf.bytes, 0);
        assert!(buf.is_empty());
    }

    /// Removing from the middle of a block keeps every survivor addressable.
    #[test]
    fn a_removal_keeps_the_survivors_addressable() {
        let dir = tempfile::tempdir().unwrap();
        let mut be = backend(dir.path(), "s.h5");
        let arrays: Vec<TypedArray> = (0..4).map(|i| f64_array(vec![6], i as f64)).collect();
        let hashes: Vec<[u8; 32]> = arrays.iter().map(array_hash).collect();
        let mut buf = WriteBuffer::new();
        for (hash, array) in hashes.iter().zip(&arrays) {
            buf.push(*hash, array, res(), &mut be).unwrap();
        }
        assert!(buf.remove(&hashes[1]));
        assert!(!buf.remove(&hashes[1]));
        for i in [0, 2, 3] {
            assert_eq!(buf.get(&hashes[i]), Some(&arrays[i]));
        }
        buf.write_holding([hashes[3]], &mut be).unwrap();
        assert!(buf.is_empty(), "one pool, so one block held them all");
        for i in [0, 2, 3] {
            assert_eq!(be.get_array(&hashes[i], Dtype::F64).unwrap(), arrays[i]);
        }
    }
}
