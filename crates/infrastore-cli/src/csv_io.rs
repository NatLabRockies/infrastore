//! CSV <-> [`TypedArray`] conversion.
//!
//! Reading flattens all value cells in row-major order into the dtype's
//! little-endian bytes; the full array shape comes from the descriptor. Writing
//! decodes the bytes back to per-dtype text.

use std::path::Path;

use infrastore_core::{Dtype, TypedArray};

/// Cells read from a data CSV.
pub struct CsvData {
    /// Raw strings from the stripped leading columns, one row at a time. For a
    /// timestamped file this is the timestamp column; for a forecast exported by
    /// `export` it is `issue_time` and `target_time`.
    pub leading: Vec<Vec<String>>,
    /// Value cells flattened row-major (left-to-right, top-to-bottom).
    pub values: Vec<String>,
    /// Value cells per row, i.e. the width of the value block.
    pub row_width: usize,
    /// Number of data rows read.
    pub rows: usize,
}

impl CsvData {
    /// The first stripped leading column — the timestamps, for the callers that
    /// strip exactly one.
    pub fn timestamps(&self) -> Vec<String> {
        self.leading
            .iter()
            .filter_map(|row| row.first().cloned())
            .collect()
    }
}

/// Read a data CSV's header row, so a caller can decide how many leading
/// columns to strip.
///
/// Every data CSV must have one. The header is not decoration: it is what
/// [`crate::descriptor::Descriptor::csv_layout`] reads to tell a hand-authored
/// value-only file from one `export` wrote, and getting that wrong reorders a
/// forecast's axes without failing. A file with no header row is therefore an
/// error here rather than a silent fall back to the flat layout.
pub fn read_header(path: &Path) -> Result<Vec<String>, String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .trim(csv::Trim::All)
        .from_path(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    let header: Vec<String> = match reader.headers() {
        Ok(h) => h.iter().map(|s| s.to_string()).collect(),
        Err(e) => return Err(format!("reading the header of {}: {e}", path.display())),
    };
    if header.is_empty() {
        return Err(format!(
            "{} is empty; every data CSV must start with a header row \
             (e.g. `value`, or `timestamp,value`)",
            path.display()
        ));
    }
    Ok(header)
}

/// Read a data CSV, stripping `leading_cols` non-value columns from the left of
/// every row and flattening the rest row-major. The first row is always the
/// header — see [`read_header`].
pub fn read_csv(path: &Path, leading_cols: usize) -> Result<CsvData, String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .trim(csv::Trim::All)
        .from_path(path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;

    let mut leading = Vec::new();
    let mut values = Vec::new();
    let mut row_width = 0usize;
    let mut rows = 0usize;
    for (row, record) in reader.records().enumerate() {
        let record =
            record.map_err(|e| format!("reading {} row {}: {e}", path.display(), row + 1))?;
        if record.len() < leading_cols {
            return Err(format!(
                "{} row {} has {} columns, fewer than the {leading_cols} leading \
                 column(s) expected",
                path.display(),
                row + 1,
                record.len()
            ));
        }
        let mut iter = record.iter();
        let mut lead = Vec::with_capacity(leading_cols);
        for _ in 0..leading_cols {
            lead.push(iter.next().unwrap_or_default().to_string());
        }
        leading.push(lead);
        let before = values.len();
        for cell in iter {
            values.push(cell.to_string());
        }
        // `flexible(false)` guarantees a constant width, so the first row's
        // width is the file's width.
        if rows == 0 {
            row_width = values.len() - before;
        }
        rows += 1;
    }
    Ok(CsvData {
        leading,
        values,
        row_width,
        rows,
    })
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

/// Whether a cell would be accepted as a value of `dtype`.
///
/// Used to tell a header row from a first row of data, so the same grammar that
/// reads values decides it — a cell this accepts is a value, whatever column it
/// happens to sit in.
pub fn parses_as(dtype: Dtype, cell: &str) -> bool {
    let mut sink = Vec::new();
    encode_cell(dtype, cell, &mut sink).is_ok()
}

fn encode_cell(dtype: Dtype, raw: &str, out: &mut Vec<u8>) -> Result<(), String> {
    let s = raw.trim();
    match dtype {
        Dtype::F64 => out.extend_from_slice(&parse_num::<f64>(s)?.to_le_bytes()),
        Dtype::F32 => out.extend_from_slice(&parse_num::<f32>(s)?.to_le_bytes()),
        Dtype::I64 => out.extend_from_slice(&parse_num::<i64>(s)?.to_le_bytes()),
        Dtype::I32 => out.extend_from_slice(&parse_num::<i32>(s)?.to_le_bytes()),
        Dtype::I16 => out.extend_from_slice(&parse_num::<i16>(s)?.to_le_bytes()),
        Dtype::I8 => out.extend_from_slice(&parse_num::<i8>(s)?.to_le_bytes()),
        Dtype::U64 => out.extend_from_slice(&parse_num::<u64>(s)?.to_le_bytes()),
        Dtype::U32 => out.extend_from_slice(&parse_num::<u32>(s)?.to_le_bytes()),
        Dtype::U16 => out.extend_from_slice(&parse_num::<u16>(s)?.to_le_bytes()),
        Dtype::U8 => out.extend_from_slice(&parse_num::<u8>(s)?.to_le_bytes()),
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
            Dtype::I16 => i16::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::I8 => i8::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::U64 => u64::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::U32 => u32::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::U16 => u16::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::U8 => u8::from_le_bytes(c.try_into().unwrap()).to_string(),
            Dtype::Bool => (c[0] != 0).to_string(),
        })
        .collect()
}

/// Decode every element to a JSON scalar of its own type.
///
/// Distinct from [`array_to_strings`], which is for text output. JSON consumers
/// get numbers as numbers and booleans as booleans, so nothing downstream has to
/// re-parse `"101.5"` back into a float.
///
/// `f64`/`f32` values that JSON cannot represent (NaN, ±inf) become `null`:
/// JSON has no spelling for them, and `null` is the one encoding every parser
/// accepts. `u64` values above 2^53 stay exact — `serde_json` carries them as
/// integers, not floats.
pub fn array_to_json_values(arr: &TypedArray) -> Vec<serde_json::Value> {
    use serde_json::{Value, json};
    let size = arr.dtype.size();
    arr.bytes
        .chunks_exact(size)
        .map(|c| match arr.dtype {
            Dtype::F64 => finite_json(f64::from_le_bytes(c.try_into().unwrap())),
            Dtype::F32 => finite_json(f32::from_le_bytes(c.try_into().unwrap()) as f64),
            Dtype::I64 => json!(i64::from_le_bytes(c.try_into().unwrap())),
            Dtype::I32 => json!(i32::from_le_bytes(c.try_into().unwrap())),
            Dtype::I16 => json!(i16::from_le_bytes(c.try_into().unwrap())),
            Dtype::I8 => json!(i8::from_le_bytes(c.try_into().unwrap())),
            Dtype::U64 => json!(u64::from_le_bytes(c.try_into().unwrap())),
            Dtype::U32 => json!(u32::from_le_bytes(c.try_into().unwrap())),
            Dtype::U16 => json!(u16::from_le_bytes(c.try_into().unwrap())),
            Dtype::U8 => json!(u8::from_le_bytes(c.try_into().unwrap())),
            Dtype::Bool => json!(c[0] != 0),
        })
        .collect::<Vec<Value>>()
}

fn finite_json(v: f64) -> serde_json::Value {
    if v.is_finite() {
        serde_json::json!(v)
    } else {
        serde_json::Value::Null
    }
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
            Dtype::I16 => i16::from_le_bytes(c.try_into().unwrap()) as f64,
            Dtype::I8 => i8::from_le_bytes(c.try_into().unwrap()) as f64,
            Dtype::U64 => u64::from_le_bytes(c.try_into().unwrap()) as f64,
            Dtype::U32 => u32::from_le_bytes(c.try_into().unwrap()) as f64,
            Dtype::U16 => u16::from_le_bytes(c.try_into().unwrap()) as f64,
            Dtype::U8 => u8::from_le_bytes(c.try_into().unwrap()) as f64,
            Dtype::Bool => (c[0] != 0) as u8 as f64,
        })
        .collect()
}
