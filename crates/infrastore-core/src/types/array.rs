//! Element dtype + a runtime-typed N-dimensional array.
//!
//! A time series array is homogeneous in its element dtype but arrays may differ
//! from one another. [`Dtype`] is the stable cross-language contract (integer
//! codes match the FFI / bindings); [`TypedArray`] carries the dtype, the shape
//! (`[length, k1, k2, ...]` — trailing dims encode fixed homogeneous tuples such
//! as the 3 coefficients of a quadratic cost curve), and the row-major,
//! little-endian element bytes.

use serde::{Deserialize, Serialize};

/// Supported physical element types. Codes are part of the public contract:
/// 0-5 were the original set and never move; new widths are appended.
///
/// `Ord` follows declaration order, which is also code order. It carries no
/// meaning beyond giving layout-grouping code a stable, allocation-free sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Dtype {
    F64,
    F32,
    I64,
    I32,
    U64,
    Bool,
    I16,
    I8,
    U32,
    U16,
    U8,
}

impl Dtype {
    /// Every supported dtype, in code order. Handy for exhaustive tests and for
    /// rendering the accepted vocabulary in an error message.
    pub const ALL: &'static [Dtype] = &[
        Dtype::F64,
        Dtype::F32,
        Dtype::I64,
        Dtype::I32,
        Dtype::U64,
        Dtype::Bool,
        Dtype::I16,
        Dtype::I8,
        Dtype::U32,
        Dtype::U16,
        Dtype::U8,
    ];

    pub fn code(self) -> i32 {
        match self {
            Dtype::F64 => 0,
            Dtype::F32 => 1,
            Dtype::I64 => 2,
            Dtype::I32 => 3,
            Dtype::U64 => 4,
            Dtype::Bool => 5,
            Dtype::I16 => 6,
            Dtype::I8 => 7,
            Dtype::U32 => 8,
            Dtype::U16 => 9,
            Dtype::U8 => 10,
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
            6 => Dtype::I16,
            7 => Dtype::I8,
            8 => Dtype::U32,
            9 => Dtype::U16,
            10 => Dtype::U8,
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
            Dtype::I16 => "i16",
            Dtype::I8 => "i8",
            Dtype::U32 => "u32",
            Dtype::U16 => "u16",
            Dtype::U8 => "u8",
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
            "i16" => Dtype::I16,
            "i8" => Dtype::I8,
            "u32" => Dtype::U32,
            "u16" => Dtype::U16,
            "u8" => Dtype::U8,
            _ => return None,
        })
    }

    /// Byte width of one element.
    pub fn size(self) -> usize {
        match self {
            Dtype::F64 | Dtype::I64 | Dtype::U64 => 8,
            Dtype::F32 | Dtype::I32 | Dtype::U32 => 4,
            Dtype::I16 | Dtype::U16 => 2,
            Dtype::Bool | Dtype::I8 | Dtype::U8 => 1,
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, Dtype::F64 | Dtype::F32)
    }
}

/// A runtime-typed, N-dimensional array stored as raw little-endian bytes.
///
/// `Eq` is sound because `PartialEq` compares the raw byte buffers: float
/// element comparison is bitwise (NaN ≠ NaN by bits, `+0.0` ≠ `-0.0`), which is
/// the reflexive total equality `Eq` requires. This matches the store's
/// content-addressing, where two arrays are "the same" iff their bytes hash
/// identically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedArray {
    pub dtype: Dtype,
    /// `[length, k1, k2, ...]`. The first dim is time; trailing dims are the
    /// per-step element shape (empty trailing dims = scalar per step).
    pub shape: Vec<usize>,
    /// Row-major, little-endian element bytes. `len() == num_elements * dtype.size()`.
    pub bytes: Vec<u8>,
}

/// The number of elements a shape describes, or an error if the product does
/// not fit a `usize`.
///
/// Checked because these are the crate's only validating array constructors, and
/// an unchecked `product()` defeats the validation it feeds: in a release build,
/// where the workspace profile leaves `overflow-checks` off, `[2^61, 8]` wraps to
/// an element count of 0, so an empty byte buffer "matches" and the array reports
/// a length of 2^61 with nothing behind it. In a debug build it panics, which the
/// documented `Result<_, String>` contract says it should not.
fn element_count(shape: &[usize]) -> Result<usize, String> {
    shape
        .iter()
        .try_fold(1usize, |acc, &d| acc.checked_mul(d))
        .ok_or_else(|| format!("TypedArray: shape {shape:?} has more elements than usize can hold"))
}

/// The byte length a `dtype` + `shape` describes, checked the same way.
fn expected_bytes(dtype: Dtype, shape: &[usize]) -> Result<usize, String> {
    element_count(shape)?
        .checked_mul(dtype.size())
        .ok_or_else(|| {
            format!(
                "TypedArray: shape {:?} of {} needs more bytes than usize can hold",
                shape,
                dtype.as_str()
            )
        })
}

impl TypedArray {
    /// Construct, validating that `bytes` matches `dtype` and `shape`.
    pub fn new(dtype: Dtype, shape: Vec<usize>, bytes: Vec<u8>) -> Result<Self, String> {
        let expected = expected_bytes(dtype, &shape)?;
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

    /// Build a `TypedArray` from a typed slice + shape, validating that
    /// `values.len()` equals the shape's element count. The array's dtype is
    /// `T`'s dtype ([`Element::DTYPE`]). Values are encoded little-endian in
    /// row-major order, matching the on-disk layout.
    pub fn from_slice<T: Element>(shape: Vec<usize>, values: &[T]) -> Result<Self, String> {
        let n = element_count(&shape)?;
        if values.len() != n {
            return Err(format!(
                "TypedArray::from_slice: {} values does not match shape {:?} ({} expected)",
                values.len(),
                shape,
                n
            ));
        }
        let mut bytes = Vec::with_capacity(values.len() * T::DTYPE.size());
        for &v in values {
            v.push_le_bytes(&mut bytes);
        }
        Ok(Self {
            dtype: T::DTYPE,
            shape,
            bytes,
        })
    }

    /// Decode the bytes as a `Vec<T>` (errors if the array's dtype is not `T`'s).
    ///
    /// Decoding copies element by element via `from_le_bytes`; it never casts the
    /// `&[u8]` buffer to `&[T]`, so it is sound regardless of buffer alignment.
    pub fn to_vec<T: Element>(&self) -> Result<Vec<T>, String> {
        if self.dtype != T::DTYPE {
            return Err(format!(
                "expected {}, got {}",
                T::DTYPE.as_str(),
                self.dtype.as_str()
            ));
        }
        Ok(self
            .bytes
            .chunks_exact(T::DTYPE.size())
            .map(T::from_le_bytes)
            .collect())
    }

    /// Build an `f64` `TypedArray` from values + shape. Convenience over
    /// [`Self::from_slice`]; panics on a shape/length mismatch (callers that
    /// build the shape from the values never hit this).
    pub fn from_f64(shape: Vec<usize>, values: &[f64]) -> Self {
        Self::from_slice(shape, values).expect("from_f64: shape does not match value count")
    }

    /// Decode the bytes as `f64`s (errors if the dtype is not `F64`).
    pub fn to_f64_vec(&self) -> Result<Vec<f64>, String> {
        self.to_vec::<f64>()
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A physical element type a [`TypedArray`] can hold, carrying its [`Dtype`] and
/// little-endian codec. Sealed: only the supported types (`f64`, `f32`, the
/// signed and unsigned integer widths, and `bool`) implement it, so the mapping
/// to [`Dtype`] stays exhaustive and closed.
pub trait Element: sealed::Sealed + Copy {
    /// The [`Dtype`] a `TypedArray` built from this element carries.
    const DTYPE: Dtype;
    /// Append this value's little-endian encoding to `out`.
    fn push_le_bytes(self, out: &mut Vec<u8>);
    /// Decode one element from its little-endian bytes. `bytes.len()` is
    /// `DTYPE.size()` (the chunk width [`TypedArray::to_vec`] feeds it).
    fn from_le_bytes(bytes: &[u8]) -> Self;
}

macro_rules! impl_numeric_element {
    ($t:ty, $dtype:expr) => {
        impl sealed::Sealed for $t {}
        impl Element for $t {
            const DTYPE: Dtype = $dtype;
            fn push_le_bytes(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            fn from_le_bytes(bytes: &[u8]) -> Self {
                <$t>::from_le_bytes(bytes.try_into().expect("chunk width matches dtype size"))
            }
        }
    };
}

impl_numeric_element!(f64, Dtype::F64);
impl_numeric_element!(f32, Dtype::F32);
impl_numeric_element!(i64, Dtype::I64);
impl_numeric_element!(i32, Dtype::I32);
impl_numeric_element!(i16, Dtype::I16);
impl_numeric_element!(i8, Dtype::I8);
impl_numeric_element!(u64, Dtype::U64);
impl_numeric_element!(u32, Dtype::U32);
impl_numeric_element!(u16, Dtype::U16);
impl_numeric_element!(u8, Dtype::U8);

impl sealed::Sealed for bool {}
impl Element for bool {
    const DTYPE: Dtype = Dtype::Bool;
    fn push_le_bytes(self, out: &mut Vec<u8>) {
        // One byte per bool, matching the on-disk 1-byte width.
        out.push(self as u8);
    }
    fn from_le_bytes(bytes: &[u8]) -> Self {
        bytes[0] != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_round_trip_every_dtype() {
        let a = TypedArray::from_slice(vec![2, 2], &[1.0f64, 2.5, -3.0, 4.0]).unwrap();
        assert_eq!(a.dtype, Dtype::F64);
        assert_eq!(a.to_vec::<f64>().unwrap(), vec![1.0, 2.5, -3.0, 4.0]);

        let a = TypedArray::from_slice(vec![3], &[1.5f32, 2.5, 3.5]).unwrap();
        assert_eq!(a.dtype, Dtype::F32);
        assert_eq!(a.to_vec::<f32>().unwrap(), vec![1.5, 2.5, 3.5]);

        let a = TypedArray::from_slice(vec![3], &[-1i64, 0, 9_000_000_000]).unwrap();
        assert_eq!(a.to_vec::<i64>().unwrap(), vec![-1, 0, 9_000_000_000]);

        let a = TypedArray::from_slice(vec![2], &[-7i32, 42]).unwrap();
        assert_eq!(a.to_vec::<i32>().unwrap(), vec![-7, 42]);

        let a = TypedArray::from_slice(vec![2], &[1u64, u64::MAX]).unwrap();
        assert_eq!(a.to_vec::<u64>().unwrap(), vec![1, u64::MAX]);

        let a = TypedArray::from_slice(vec![4], &[true, false, true, true]).unwrap();
        assert_eq!(a.dtype, Dtype::Bool);
        assert_eq!(a.bytes, vec![1, 0, 1, 1]);
        assert_eq!(a.to_vec::<bool>().unwrap(), vec![true, false, true, true]);
    }

    #[test]
    fn to_vec_wrong_dtype_errors() {
        let a = TypedArray::from_slice(vec![2], &[1.0f64, 2.0]).unwrap();
        assert!(a.to_vec::<i64>().is_err());
        assert!(a.to_vec::<f32>().is_err());
        assert!(a.to_vec::<bool>().is_err());
    }

    #[test]
    fn from_slice_length_mismatch_errors() {
        assert!(TypedArray::from_slice(vec![2, 2], &[1.0f64, 2.0]).is_err());
    }
}
