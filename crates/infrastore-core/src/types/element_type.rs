//! The logical element type of a stored array.
//!
//! [`Dtype`] says how many bytes an element occupies and how to decode them.
//! [`ElementType`] says what the elements *mean* and, for the composite kinds,
//! how one timestep's values are laid out across the trailing array dims. It is
//! the store's own vocabulary, not a binding's: a Julia
//! `PiecewiseLinearData` and a TypeScript `{x, y}[]` are both
//! [`ElementType::PiecewiseLinear`] here, so a consumer written in either
//! language can decode an array without knowing which language wrote it.
//!
//! The canonical string form is what travels: it is the `element_type` column
//! in the SQLite catalog, a UTF-8 string across the C ABI, and a `string` field
//! over gRPC. A parameterized grammar (`tuple(3,f64)`) does not fit an integer
//! code, so unlike [`Dtype`] there is no numeric encoding.
//!
//! ```text
//! f64 | f32 | i64 | i32 | i16 | i8 | u64 | u32 | u16 | u8 | bool
//! tuple(N,dtype)          e.g. tuple(3,f64)
//! linear_function
//! quadratic_function
//! piecewise_linear
//! piecewise_step
//! ```

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::array::{Dtype, TypedArray};
use crate::error::{Result, TimeSeriesError};

/// What the elements of a stored array mean, and how one timestep's values are
/// laid out across the trailing dims.
///
/// Serializes as its canonical string form (see the module docs), which is also
/// the on-disk and over-the-wire spelling.
///
/// `Ord` follows declaration order and carries no meaning of its own; it exists
/// so code that groups arrays by physical layout can sort without allocating
/// the string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum ElementType {
    /// One value per timestep of the given dtype. Trailing dims are free: a
    /// scalar element type still allows a dense per-step array (e.g. one column
    /// per percentile).
    Scalar(Dtype),
    /// A fixed-arity homogeneous tuple per timestep, laid out as `[arity]`.
    Tuple { arity: usize, dtype: Dtype },
    /// `f(x) = proportional * x + constant`, laid out as `[2]`:
    /// `proportional, constant`.
    LinearFunction,
    /// `f(x) = quadratic * x^2 + proportional * x + constant`, laid out as
    /// `[3]`: `quadratic, proportional, constant`.
    QuadraticFunction,
    /// A ragged list of `(x, y)` points, laid out as `[1 + 2 * w]`:
    /// `n, x1, y1, ..., xn, yn`, zero-padded to the row width.
    PiecewiseLinear,
    /// Ragged x-coordinates plus step y-values, laid out as `[max(1, 2 * w)]`:
    /// `n, x1 ... xn, y1 ... y(n-1)`, zero-padded to the row width.
    PiecewiseStep,
}

impl Default for ElementType {
    /// `f64` scalars — the overwhelmingly common case, and the SQLite column
    /// default.
    fn default() -> Self {
        ElementType::Scalar(Dtype::F64)
    }
}

impl ElementType {
    /// The physical dtype the stored bytes are encoded in. Every function-data
    /// kind is `f64`; scalars and tuples carry their own.
    pub fn physical_dtype(self) -> Dtype {
        match self {
            ElementType::Scalar(dtype) | ElementType::Tuple { dtype, .. } => dtype,
            ElementType::LinearFunction
            | ElementType::QuadraticFunction
            | ElementType::PiecewiseLinear
            | ElementType::PiecewiseStep => Dtype::F64,
        }
    }

    /// Whether one timestep's values occupy a variable number of the row's
    /// slots, with the used count stored in the row's leading element.
    pub fn is_ragged(self) -> bool {
        matches!(
            self,
            ElementType::PiecewiseLinear | ElementType::PiecewiseStep
        )
    }

    /// The exact trailing dims a timestep occupies, for the kinds whose width
    /// is fixed. `None` for scalars (any dense per-step shape is allowed) and
    /// for the ragged kinds (the width depends on the widest timestep).
    pub fn fixed_element_dims(self) -> Option<Vec<usize>> {
        match self {
            ElementType::Scalar(_) => None,
            ElementType::Tuple { arity, .. } => Some(vec![arity]),
            ElementType::LinearFunction => Some(vec![2]),
            ElementType::QuadraticFunction => Some(vec![3]),
            ElementType::PiecewiseLinear | ElementType::PiecewiseStep => None,
        }
    }

    /// Parse the canonical string form. `None` if `s` is not a valid spelling.
    pub fn parse(s: &str) -> Option<Self> {
        if let Some(dtype) = Dtype::parse(s) {
            return Some(ElementType::Scalar(dtype));
        }
        Some(match s {
            "linear_function" => ElementType::LinearFunction,
            "quadratic_function" => ElementType::QuadraticFunction,
            "piecewise_linear" => ElementType::PiecewiseLinear,
            "piecewise_step" => ElementType::PiecewiseStep,
            _ => return parse_tuple(s),
        })
    }

    /// Validate one timestep's trailing dims against this element type.
    ///
    /// `element_dims` is the per-step shape: the array's trailing dims after
    /// the time axis and, for a forecast, after the count (and percentile /
    /// scenario) axes.
    pub fn validate_element_dims(self, element_dims: &[usize]) -> Result<()> {
        if let Some(expected) = self.fixed_element_dims() {
            if element_dims != expected.as_slice() {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "element_type {self} requires per-step element dims {expected:?}, got \
                     {element_dims:?}"
                )));
            }
            return Ok(());
        }
        if !self.is_ragged() {
            // Scalars accept any dense per-step shape.
            return Ok(());
        }
        let [width] = element_dims else {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "element_type {self} requires exactly one per-step element dim (the row \
                 width), got {element_dims:?}"
            )));
        };
        if *width < 1 {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "element_type {self} requires a row width of at least 1 (the leading count), \
                 got {width}"
            )));
        }
        // `piecewise_linear` rows are `n, x1, y1, ... xn, yn` (odd);
        // `piecewise_step` rows are `n, x1..xn, y1..y(n-1)` (2n, or 1 when the
        // widest timestep is empty). A width outside those sets cannot have
        // been produced by the encoder.
        let ok = match self {
            ElementType::PiecewiseLinear => width % 2 == 1,
            ElementType::PiecewiseStep => *width == 1 || width % 2 == 0,
            _ => true,
        };
        if !ok {
            let why = if self == ElementType::PiecewiseLinear {
                "a row holds 1 + 2*points values, so the width is odd"
            } else {
                "a row holds 2*points values, or 1 when every timestep is empty"
            };
            return Err(TimeSeriesError::InvalidParameter(format!(
                "element_type {self} cannot have row width {width}: {why}"
            )));
        }
        Ok(())
    }

    /// Validate a whole array against this element type: physical dtype, the
    /// per-step dims, and — for the ragged kinds — that every row's leading
    /// count fits the row.
    ///
    /// `leading_dims` is how many leading axes are *not* part of the per-step
    /// element shape: 1 for a static series (`[length, ...]`), 2 for a
    /// `Deterministic` (`[H, count, ...]`), 3 for a `Probabilistic` or
    /// `Scenarios` (`[P, H, count, ...]`).
    pub fn validate_array(self, array: &TypedArray, leading_dims: usize) -> Result<()> {
        let expected_dtype = self.physical_dtype();
        if array.dtype != expected_dtype {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "element_type {self} stores {} values, but the array's dtype is {}",
                expected_dtype.as_str(),
                array.dtype.as_str()
            )));
        }
        if array.shape.len() < leading_dims {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "shape {:?} has fewer than the {leading_dims} leading dims its time-series \
                 type requires",
                array.shape
            )));
        }
        let element_dims = &array.shape[leading_dims..];
        self.validate_element_dims(element_dims)?;
        if self.is_ragged() {
            self.validate_ragged_rows(array, element_dims[0])?;
        }
        Ok(())
    }

    /// Check each ragged row's leading count against the row width. Cheap (one
    /// element read per timestep) and it catches an encoder that padded to the
    /// wrong width or wrote a count it did not have the slots for.
    fn validate_ragged_rows(self, array: &TypedArray, width: usize) -> Result<()> {
        debug_assert_eq!(array.dtype, Dtype::F64);
        let stride = width * Dtype::F64.size();
        for (row, chunk) in array.bytes.chunks_exact(stride).enumerate() {
            let raw = f64::from_le_bytes(chunk[..8].try_into().expect("f64 row is at least 8B"));
            if !(raw.is_finite() && raw >= 0.0 && raw.fract() == 0.0) {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "element_type {self}: row {row} leading count is {raw}, which is not a \
                     non-negative whole number"
                )));
            }
            let n = raw as usize;
            let needed = match self {
                ElementType::PiecewiseLinear => 1 + 2 * n,
                // `n` x-coords and `n - 1` y-values, plus the count itself.
                _ if n == 0 => 1,
                _ => 2 * n,
            };
            if needed > width {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "element_type {self}: row {row} declares {n} points, which needs {needed} \
                     slots but the row width is {width}"
                )));
            }
        }
        Ok(())
    }
}

fn parse_tuple(s: &str) -> Option<ElementType> {
    let inner = s.strip_prefix("tuple(")?.strip_suffix(')')?;
    let (arity, dtype) = inner.split_once(',')?;
    let arity: usize = arity.trim().parse().ok()?;
    if arity == 0 {
        return None;
    }
    Some(ElementType::Tuple {
        arity,
        dtype: Dtype::parse(dtype.trim())?,
    })
}

impl fmt::Display for ElementType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ElementType::Scalar(dtype) => f.write_str(dtype.as_str()),
            ElementType::Tuple { arity, dtype } => write!(f, "tuple({arity},{})", dtype.as_str()),
            ElementType::LinearFunction => f.write_str("linear_function"),
            ElementType::QuadraticFunction => f.write_str("quadratic_function"),
            ElementType::PiecewiseLinear => f.write_str("piecewise_linear"),
            ElementType::PiecewiseStep => f.write_str("piecewise_step"),
        }
    }
}

impl FromStr for ElementType {
    type Err = TimeSeriesError;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s).ok_or_else(|| {
            TimeSeriesError::InvalidParameter(format!(
                "unknown element_type {s:?}; expected a dtype ({}), tuple(N,dtype), \
                 linear_function, quadratic_function, piecewise_linear, or piecewise_step",
                Dtype::ALL
                    .iter()
                    .map(|d| d.as_str())
                    .collect::<Vec<_>>()
                    .join("/")
            ))
        })
    }
}

impl From<ElementType> for String {
    fn from(value: ElementType) -> Self {
        value.to_string()
    }
}

impl TryFrom<String> for ElementType {
    type Error = TimeSeriesError;
    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One canonical spelling per kind, and the round trip through it.
    #[test]
    fn canonical_strings_round_trip() {
        let cases = [
            (ElementType::Scalar(Dtype::F64), "f64"),
            (ElementType::Scalar(Dtype::U8), "u8"),
            (ElementType::Scalar(Dtype::Bool), "bool"),
            (
                ElementType::Tuple {
                    arity: 3,
                    dtype: Dtype::F64,
                },
                "tuple(3,f64)",
            ),
            (
                ElementType::Tuple {
                    arity: 4,
                    dtype: Dtype::I32,
                },
                "tuple(4,i32)",
            ),
            (ElementType::LinearFunction, "linear_function"),
            (ElementType::QuadraticFunction, "quadratic_function"),
            (ElementType::PiecewiseLinear, "piecewise_linear"),
            (ElementType::PiecewiseStep, "piecewise_step"),
        ];
        for (element_type, spelling) in cases {
            assert_eq!(element_type.to_string(), spelling);
            assert_eq!(ElementType::parse(spelling), Some(element_type));
            let json = serde_json::to_string(&element_type).unwrap();
            assert_eq!(json, format!("\"{spelling}\""));
            assert_eq!(
                serde_json::from_str::<ElementType>(&json).unwrap(),
                element_type
            );
        }
    }

    #[test]
    fn every_dtype_is_a_scalar_element_type() {
        for &dtype in Dtype::ALL {
            let parsed = ElementType::parse(dtype.as_str()).expect("dtype spells a scalar");
            assert_eq!(parsed, ElementType::Scalar(dtype));
            assert_eq!(parsed.physical_dtype(), dtype);
        }
    }

    #[test]
    fn function_data_kinds_are_f64() {
        for kind in [
            ElementType::LinearFunction,
            ElementType::QuadraticFunction,
            ElementType::PiecewiseLinear,
            ElementType::PiecewiseStep,
        ] {
            assert_eq!(kind.physical_dtype(), Dtype::F64);
        }
    }

    #[test]
    fn parse_rejects_malformed_spellings() {
        for bad in [
            "",
            "float64",
            "PiecewiseLinearData",
            "tuple",
            "tuple()",
            "tuple(3)",
            "tuple(0,f64)",
            "tuple(-1,f64)",
            "tuple(3,f65)",
            "tuple(3,f64",
            "Tuple(3,f64)",
            "piecewise",
        ] {
            assert!(
                ElementType::parse(bad).is_none(),
                "{bad:?} should not parse"
            );
            assert!(bad.parse::<ElementType>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn fixed_width_kinds_require_their_exact_dims() {
        assert!(
            ElementType::LinearFunction
                .validate_element_dims(&[2])
                .is_ok()
        );
        assert!(
            ElementType::QuadraticFunction
                .validate_element_dims(&[3])
                .is_ok()
        );
        let tuple = ElementType::Tuple {
            arity: 5,
            dtype: Dtype::F64,
        };
        assert!(tuple.validate_element_dims(&[5]).is_ok());

        for (kind, dims) in [
            (ElementType::LinearFunction, &[3][..]),
            (ElementType::LinearFunction, &[][..]),
            (ElementType::LinearFunction, &[2, 1][..]),
            (ElementType::QuadraticFunction, &[2][..]),
            (tuple, &[4][..]),
        ] {
            let err = kind.validate_element_dims(dims).unwrap_err();
            assert!(err.to_string().contains("per-step element dims"), "{err}");
        }
    }

    #[test]
    fn scalars_accept_any_dense_per_step_shape() {
        let scalar = ElementType::Scalar(Dtype::F64);
        for dims in [&[][..], &[3][..], &[2, 4][..]] {
            assert!(scalar.validate_element_dims(dims).is_ok(), "{dims:?}");
        }
    }

    #[test]
    fn ragged_kinds_constrain_the_row_width() {
        // piecewise_linear rows are 1 + 2*points wide, so odd.
        for width in [1usize, 3, 5, 101] {
            assert!(
                ElementType::PiecewiseLinear
                    .validate_element_dims(&[width])
                    .is_ok(),
                "{width}"
            );
        }
        for width in [0usize, 2, 4] {
            assert!(
                ElementType::PiecewiseLinear
                    .validate_element_dims(&[width])
                    .is_err(),
                "{width}"
            );
        }
        // piecewise_step rows are 2*points wide, or 1 when every step is empty.
        for width in [1usize, 2, 4, 100] {
            assert!(
                ElementType::PiecewiseStep
                    .validate_element_dims(&[width])
                    .is_ok(),
                "{width}"
            );
        }
        for width in [0usize, 3, 5] {
            assert!(
                ElementType::PiecewiseStep
                    .validate_element_dims(&[width])
                    .is_err(),
                "{width}"
            );
        }
        // Exactly one trailing dim.
        assert!(
            ElementType::PiecewiseLinear
                .validate_element_dims(&[3, 2])
                .is_err()
        );
        assert!(
            ElementType::PiecewiseLinear
                .validate_element_dims(&[])
                .is_err()
        );
    }

    #[test]
    fn validate_array_checks_the_physical_dtype() {
        let array = TypedArray::from_slice(vec![2, 2], &[1i64, 2, 3, 4]).unwrap();
        let err = ElementType::LinearFunction
            .validate_array(&array, 1)
            .unwrap_err();
        assert!(err.to_string().contains("stores f64 values"), "{err}");

        let array = TypedArray::from_f64(vec![2, 2], &[1.0, 2.0, 3.0, 4.0]);
        assert!(
            ElementType::LinearFunction
                .validate_array(&array, 1)
                .is_ok()
        );
    }

    #[test]
    fn validate_array_checks_ragged_row_counts() {
        // Two timesteps, width 5 = 1 + 2*2 points.
        let ok = TypedArray::from_f64(
            vec![2, 5],
            &[
                2.0, 1.0, 10.0, 2.0, 20.0, // 2 points
                1.0, 3.0, 30.0, 0.0, 0.0, // 1 point, zero-padded
            ],
        );
        assert!(ElementType::PiecewiseLinear.validate_array(&ok, 1).is_ok());

        // A count of 3 needs 7 slots but the row is 5 wide.
        let overflowing = TypedArray::from_f64(
            vec![2, 5],
            &[2.0, 1.0, 10.0, 2.0, 20.0, 3.0, 3.0, 30.0, 0.0, 0.0],
        );
        let err = ElementType::PiecewiseLinear
            .validate_array(&overflowing, 1)
            .unwrap_err();
        assert!(err.to_string().contains("row 1 declares 3 points"), "{err}");

        for bad_count in [-1.0, 1.5, f64::NAN, f64::INFINITY] {
            let bad = TypedArray::from_f64(vec![1, 5], &[bad_count, 1.0, 10.0, 2.0, 20.0]);
            let err = ElementType::PiecewiseLinear
                .validate_array(&bad, 1)
                .unwrap_err();
            assert!(
                err.to_string().contains("not a non-negative whole number"),
                "{bad_count}: {err}"
            );
        }
    }

    #[test]
    fn validate_array_uses_the_leading_dims_of_the_time_series_type() {
        // A Deterministic of quadratic curves: [H = 2, count = 3, 3].
        let array = TypedArray::from_f64(vec![2, 3, 3], &[0.0; 18]);
        assert!(
            ElementType::QuadraticFunction
                .validate_array(&array, 2)
                .is_ok()
        );
        // With the static leading-dim count the per-step dims read as [3, 3].
        assert!(
            ElementType::QuadraticFunction
                .validate_array(&array, 1)
                .is_err()
        );
        let err = ElementType::QuadraticFunction
            .validate_array(&array, 4)
            .unwrap_err();
        assert!(err.to_string().contains("fewer than the 4 leading dims"));
    }

    #[test]
    fn default_is_f64_scalars() {
        assert_eq!(ElementType::default(), ElementType::Scalar(Dtype::F64));
        assert_eq!(ElementType::default().to_string(), "f64");
    }
}
