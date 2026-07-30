//! Layout policy shared by the on-disk backend and its readers: dataset
//! naming, packed-column sizing, and standalone chunk shapes.
//!
//! Two storage modes, both natively typed + fixed-dimension:
//!
//! * **Packed** (used for SingleTimeSeries and the underlying array of a
//!   DeterministicSingleTimeSeries): many same-shaped arrays are column-packed
//!   into a dataset `sts_{dtype}_{shape}_{length}_{res}` of shape
//!   `(length, cols, *element_shape)`, chunked `(1, cols, *element_shape)` — one
//!   timestamp row across every column per chunk, so a read-by-timestamp gathers
//!   one chunk. `cols` is sized per dataset to the batch that created it (capped
//!   so a chunk stays within a byte budget); a group spills into a new dataset
//!   once full. A companion `{name}_h` dataset holds the per-column hex hash
//!   (free slots are empty). Removal frees a slot.
//!
//! * **Standalone** (used for NonSequentialTimeSeries and native forecasts):
//!   each array is its own typed multi-dim dataset `arr_{hexhash}` of shape
//!   `[length, k1, ...]`. Irregular series are chunked as one whole-array chunk;
//!   dense forecasts are chunked in bounded blocks along their count (window)
//!   axis (see [`window_block_cols`]) so a single-window read decompresses one
//!   block rather than the whole array.
//!
//! `shape` encodes the element shape: `s` = scalar, `3` = `[3]`, `3x2` = `[3, 2]`.

use crate::error::{Result, TimeSeriesError};
use crate::types::array::{Dtype, TypedArray};
use crate::types::period::Period;

/// Default column width for a packed dataset created by an un-managed
/// (one-at-a-time) write. A buffered bulk-add sizes its datasets to the batch
/// instead; either way a group spills into a new dataset once full.
pub const DEFAULT_COLS_PER_DATASET: usize = 1000;

/// Target upper bound on the bytes of one packed chunk. A dataset is chunked
/// `(1, cols, *element_shape)` — one timestamp row across every column — so the
/// column count is capped to keep that chunk at or below this budget. Batches
/// wider than the cap spill into additional datasets.
pub(crate) const MAX_CHUNK_BYTES: usize = 1 << 20; // 1 MiB

/// Bytes in one column's element block at a single timestep.
pub(crate) fn element_block_bytes(dtype: Dtype, element_shape: &[usize]) -> usize {
    element_shape.iter().product::<usize>() * dtype.size()
}

/// Resolve a packed dataset's column width: the `requested` count (defaulting to
/// [`DEFAULT_COLS_PER_DATASET`] for un-managed writes) clamped to at least one
/// column and to the [`MAX_CHUNK_BYTES`] budget, so a `(1, cols, *element_shape)`
/// timestamp-row chunk stays bounded regardless of dtype or element shape.
pub(crate) fn resolve_dataset_cols(
    requested: Option<usize>,
    dtype: Dtype,
    element_shape: &[usize],
) -> usize {
    let block = element_block_bytes(dtype, element_shape).max(1);
    let cap = (MAX_CHUNK_BYTES / block).max(1);
    requested.unwrap_or(DEFAULT_COLS_PER_DATASET).clamp(1, cap)
}

/// Windows per chunk block for a standalone forecast array chunked along
/// `count_axis`. One block spans every index of every *other* axis and
/// `cols` windows along `count_axis`, so its bytes are
/// `product(shape without count_axis) * cols * dtype.size()`; `cols` is the
/// largest count keeping that at or below [`MAX_CHUNK_BYTES`], clamped to
/// `[1, shape[count_axis]]`. A single-window read then decompresses one block,
/// and the [`ForecastReader`](crate::reader::ForecastReader) aligns its cache to
/// the same width so a window sweep decompresses each block once.
pub(crate) fn window_block_cols(dtype: Dtype, shape: &[usize], count_axis: usize) -> usize {
    let axis_len = shape.get(count_axis).copied().unwrap_or(0);
    if axis_len == 0 {
        return 1;
    }
    // Bytes of one window (the array with the count axis removed).
    let window_block: usize = shape
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != count_axis)
        .map(|(_, &d)| d)
        .product::<usize>()
        .saturating_mul(dtype.size())
        .max(1);
    let cap = (MAX_CHUNK_BYTES / window_block).max(1);
    cap.min(axis_len)
}

/// The HDF5 chunk shape for a standalone array. `None` → one whole-array chunk
/// (the historical layout, used for irregular series). `Some(axis)` → full on
/// every axis except `axis`, which is blocked to [`window_block_cols`] windows.
pub(crate) fn standalone_chunks(data: &TypedArray, window_axis: Option<usize>) -> Vec<usize> {
    match window_axis {
        Some(axis) if axis < data.shape.len() && !data.shape.contains(&0) => {
            let cols = window_block_cols(data.dtype, &data.shape, axis);
            let mut chunks = data.shape.clone();
            chunks[axis] = cols;
            chunks
        }
        // Whole-array chunk: no window axis, an out-of-range axis, or a zero
        // dimension (HDF5 rejects a zero-length chunk edge).
        _ => data.shape.clone(),
    }
}

pub(crate) const ROOT_GROUP: &str = "time_series";
pub(crate) const SINGLE_GROUP: &str = "single";
pub(crate) const HASH_SUFFIX: &str = "_h";
pub(crate) const STANDALONE_PREFIX: &str = "arr_";
/// Global attribute recording the compression policy a store was created with.
pub(crate) const COMPRESSION_ATTR: &str = "compression";

pub(crate) fn encode_shape(element_shape: &[usize]) -> String {
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

pub(crate) fn decode_shape(s: &str) -> Result<Vec<usize>> {
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

pub(crate) fn dataset_base_name(
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

pub(crate) fn spill_name(base: &str, n: usize) -> String {
    if n == 0 {
        base.to_string()
    } else {
        format!("{base}__{n}")
    }
}

pub(crate) fn parse_dataset_name(name: &str) -> Result<(Dtype, Vec<usize>, usize, Period)> {
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

pub(crate) fn hex_to_hash(s: &str) -> Result<[u8; 32]> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_block_cols_bounds_the_chunk_to_the_byte_budget() {
        // Small window -> the whole count axis fits in one chunk block.
        assert_eq!(window_block_cols(Dtype::F64, &[2, 5], 1), 5);
        // A window that is itself ~1 MiB forces a single-window block.
        let big_h = MAX_CHUNK_BYTES / std::mem::size_of::<f64>();
        assert_eq!(window_block_cols(Dtype::F64, &[big_h, 8], 1), 1);
        // A degenerate zero-length count axis is clamped to 1 (never a 0 edge).
        assert_eq!(window_block_cols(Dtype::F64, &[3, 0], 1), 1);
        // Probabilistic `[P, H, count, *E]` blocks along axis 2.
        assert_eq!(window_block_cols(Dtype::F64, &[2, 3, 7], 2), 7);
    }
}
