//! Element dtype + a runtime-typed N-dimensional array.
//!
//! A time series array is homogeneous in its element dtype but arrays may differ
//! from one another. [`Dtype`] is the stable cross-language contract (integer
//! codes match the FFI / bindings); [`TypedArray`] carries the dtype, the shape
//! (`[length, k1, k2, ...]` — trailing dims encode fixed homogeneous tuples such
//! as the 3 coefficients of a quadratic cost curve), and the row-major,
//! little-endian element bytes.

use serde::{Deserialize, Serialize};

/// Supported physical element types. Codes are part of the public contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Dtype {
    F64,
    F32,
    I64,
    I32,
    U64,
    Bool,
}

impl Dtype {
    pub fn code(self) -> i32 {
        match self {
            Dtype::F64 => 0,
            Dtype::F32 => 1,
            Dtype::I64 => 2,
            Dtype::I32 => 3,
            Dtype::U64 => 4,
            Dtype::Bool => 5,
        }
    }

    pub fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            0 => Dtype::F64,
            1 => Dtype::F32,
            2 => Dtype::I64,
            3 => Dtype::I32,
            4 => Dtype::U64,
            5 => Dtype::Bool,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Dtype::F64 => "f64",
            Dtype::F32 => "f32",
            Dtype::I64 => "i64",
            Dtype::I32 => "i32",
            Dtype::U64 => "u64",
            Dtype::Bool => "bool",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "f64" => Dtype::F64,
            "f32" => Dtype::F32,
            "i64" => Dtype::I64,
            "i32" => Dtype::I32,
            "u64" => Dtype::U64,
            "bool" => Dtype::Bool,
            _ => return None,
        })
    }

    /// Byte width of one element.
    pub fn size(self) -> usize {
        match self {
            Dtype::F64 | Dtype::I64 | Dtype::U64 => 8,
            Dtype::F32 | Dtype::I32 => 4,
            Dtype::Bool => 1,
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, Dtype::F64 | Dtype::F32)
    }
}

/// A runtime-typed, N-dimensional array stored as raw little-endian bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedArray {
    pub dtype: Dtype,
    /// `[length, k1, k2, ...]`. The first dim is time; trailing dims are the
    /// per-step element shape (empty trailing dims = scalar per step).
    pub shape: Vec<usize>,
    /// Row-major, little-endian element bytes. `len() == num_elements * dtype.size()`.
    pub bytes: Vec<u8>,
}

impl TypedArray {
    /// Construct, validating that `bytes` matches `dtype` and `shape`.
    pub fn new(dtype: Dtype, shape: Vec<usize>, bytes: Vec<u8>) -> Result<Self, String> {
        let n: usize = shape.iter().product();
        let expected = n * dtype.size();
        if bytes.len() != expected {
            return Err(format!(
                "TypedArray: {} bytes does not match shape {:?} dtype {} ({} expected)",
                bytes.len(),
                shape,
                dtype.as_str(),
                expected
            ));
        }
        Ok(Self {
            dtype,
            shape,
            bytes,
        })
    }

    /// Number of time steps (`shape[0]`).
    pub fn length(&self) -> usize {
        self.shape.first().copied().unwrap_or(0)
    }

    /// Per-step element shape (trailing dims after time).
    pub fn element_shape(&self) -> &[usize] {
        if self.shape.is_empty() {
            &[]
        } else {
            &self.shape[1..]
        }
    }

    pub fn num_elements(&self) -> usize {
        self.shape.iter().product()
    }

    /// Build an `f64` `TypedArray` from values + shape.
    pub fn from_f64(shape: Vec<usize>, values: &[f64]) -> Self {
        let mut bytes = Vec::with_capacity(values.len() * 8);
        for v in values {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        Self {
            dtype: Dtype::F64,
            shape,
            bytes,
        }
    }

    /// Decode the bytes as `f64`s (errors if the dtype is not `F64`).
    pub fn to_f64_vec(&self) -> Result<Vec<f64>, String> {
        if self.dtype != Dtype::F64 {
            return Err(format!("expected f64, got {}", self.dtype.as_str()));
        }
        Ok(self
            .bytes
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect())
    }
}
