//! CSV <-> [`TypedArray`] conversion.
//!
//! Reading flattens all value cells in row-major order into the dtype's
//! little-endian bytes; the full array shape comes from the sidecar. Writing
//! decodes the bytes back to per-dtype text.

use std::path::Path;

use time_series_store_core::{Dtype, TypedArray};

/// Cells read from a data CSV.
pub struct CsvData {
    /// Raw timestamp strings from the first column (only when requested).
    pub timestamps: Vec<String>,
    /// Value cells flattened row-major (left-to-right, top-to-bottom).
    pub values: Vec<String>,
}

/// Read a data CSV. When `timestamp_col` is true the first column is treated as
/// timestamps and the remaining columns as values; otherwise every column is a
/// value.
pub fn read_csv(path: &Path, has_header: bool, timestamp_col: bool) -> Result<CsvData, String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(has_header)
        .flexible(false)
        .trim(csv::Trim::All)
        .from_path(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;

    let mut timestamps = Vec::new();
    let mut values = Vec::new();
    for (row, record) in reader.records().enumerate() {
        let record =
            record.map_err(|e| format!("reading {} row {}: {e}", path.display(), row + 1))?;
        let mut iter = record.iter();
        if timestamp_col {
            let ts = iter.next().ok_or_else(|| {
                format!("{} row {} has no timestamp column", path.display(), row + 1)
            })?;
            timestamps.push(ts.to_string());
        }
        for cell in iter {
            values.push(cell.to_string());
        }
    }
    Ok(CsvData { timestamps, values })
}

/// Build a [`TypedArray`] of the given dtype/shape from row-major value cells.
pub fn build_typed_array(
    dtype: Dtype,
    shape: Vec<usize>,
    cells: &[String],
) -> Result<TypedArray, String> {
    let expected: usize = shape.iter().product();
    if cells.len() != expected {
        return Err(format!(
            "expected {expected} values for shape {shape:?}, found {}",
            cells.len()
        ));
    }
    let mut bytes = Vec::with_capacity(expected * dtype.size());
    for (i, cell) in cells.iter().enumerate() {
        encode_cell(dtype, cell, &mut bytes)
            .map_err(|e| format!("value #{} ('{cell}'): {e}", i + 1))?;
    }
    TypedArray::new(dtype, shape, bytes)
}

fn encode_cell(dtype: Dtype, raw: &str, out: &mut Vec<u8>) -> Result<(), String> {
    let s = raw.trim();
    match dtype {
        Dtype::F64 => out.extend_from_slice(&parse_num::<f64>(s)?.to_le_bytes()),
        Dtype::F32 => out.extend_from_slice(&parse_num::<f32>(s)?.to_le_bytes()),
        Dtype::I64 => out.extend_from_slice(&parse_num::<i64>(s)?.to_le_bytes()),
        Dtype::I32 => out.extend_from_slice(&parse_num::<i32>(s)?.to_le_bytes()),
        Dtype::U64 => out.extend_from_slice(&parse_num::<u64>(s)?.to_le_bytes()),
        Dtype::Bool => out.push(parse_bool(s)? as u8),
    }
    Ok(())
}

fn parse_num<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.parse::<T>()
        .map_err(|_| format!("could not parse as {}", std::any::type_name::<T>()))
}

fn parse_bool(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err("expected true/false/1/0".to_string()),
    }
}

/// Decode every element of an array to a display string, per dtype.
pub fn array_to_strings(arr: &TypedArray) -> Vec<String> {
    let size = arr.dtype.size();
    arr.bytes
        .chunks_exact(size)
        .map(|c| match arr.dtype {
            Dtype::F64 => f64::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::F32 => f32::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::I64 => i64::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::I32 => i32::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::U64 => u64::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::Bool => (c[0] != 0).to_string(),
        })
        .collect()
}

/// Decode every element to `f64` (lossy for wide integer types), for stats.
pub fn array_to_f64_lossy(arr: &TypedArray) -> Vec<f64> {
    let size = arr.dtype.size();
    arr.bytes
        .chunks_exact(size)
        .map(|c| match arr.dtype {
            Dtype::F64 => f64::from_le_bytes(c.try_into().unwrap()),
            Dtype::F32 => f32::from_le_bytes(c.try_into().unwrap()) as f64,
            Dtype::I64 => i64::from_le_bytes(c.try_into().unwrap()) as f64,
            Dtype::I32 => i32::from_le_bytes(c.try_into().unwrap()) as f64,
            Dtype::U64 => u64::from_le_bytes(c.try_into().unwrap()) as f64,
            Dtype::Bool => (c[0] != 0) as u8 as f64,
        })
        .collect()
}
