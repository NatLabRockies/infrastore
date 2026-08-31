//! Reference codec between a stored [`TypedArray`] and the per-timestep values
//! its [`ElementType`] describes.
//!
//! The encodings are specified in [`crate::types::element_type`]; this module is
//! the executable form of that spec, and [`conformance`] is the shared corpus
//! every other binding's codec is tested against. Nothing in the write path
//! depends on it — a producer may build the flat array itself — but a consumer
//! that goes through here never has to know the row layouts.
//!
//! Only `f64`-backed arrays carry logical structure. Everything else decodes to
//! [`DecodedValues::Raw`]: the stored elements already *are* the values, and the
//! caller reads them with [`TypedArray::to_vec`] in the physical dtype.

use serde::{Deserialize, Serialize};

use crate::error::{Result, TimeSeriesError};
use crate::types::array::{Dtype, TypedArray};
use crate::types::element_type::ElementType;

pub mod conformance;

/// One `(x, y)` point of a piecewise-linear curve.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct XyPoint {
    pub x: f64,
    pub y: f64,
}

/// `f(x) = proportional * x + constant`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LinearFunction {
    pub proportional: f64,
    pub constant: f64,
}

/// `f(x) = quadratic * x^2 + proportional * x + constant`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QuadraticFunction {
    pub quadratic: f64,
    pub proportional: f64,
    pub constant: f64,
}

/// A step function: `n` x-coordinates and the `n - 1` y-values between them.
/// An empty step function has no coordinates and no values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepFunction {
    pub x: Vec<f64>,
    pub y: Vec<f64>,
}

/// The per-timestep values of one array, decoded according to its
/// [`ElementType`]. Every variant except [`Self::Raw`] holds one entry per
/// timestep, in row-major order over the array's leading dims.
///
/// The serde representation is adjacently tagged and snake_cased, so the JSON
/// form is self-describing across languages:
/// `{"kind": "piecewise_linear", "timesteps": [[{"x": 1.0, "y": 10.0}]]}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "timesteps", rename_all = "snake_case")]
pub enum DecodedValues {
    /// Nothing to decode: the stored elements are the values. Produced for
    /// every scalar element type and for any array whose physical dtype is not
    /// `f64`.
    Raw,
    /// One `arity`-long row per timestep.
    Tuple(Vec<Vec<f64>>),
    LinearFunction(Vec<LinearFunction>),
    QuadraticFunction(Vec<QuadraticFunction>),
    /// The points of one piecewise-linear curve per timestep.
    PiecewiseLinear(Vec<Vec<XyPoint>>),
    PiecewiseStep(Vec<StepFunction>),
}

impl DecodedValues {
    /// How many timesteps these values describe.
    ///
    /// `0` for [`Self::Raw`], which carries no values of its own — the stored
    /// array is the answer there, and its `length` is on the array.
    pub fn len(&self) -> usize {
        match self {
            DecodedValues::Raw => 0,
            DecodedValues::Tuple(rows) => rows.len(),
            DecodedValues::LinearFunction(rows) => rows.len(),
            DecodedValues::QuadraticFunction(rows) => rows.len(),
            DecodedValues::PiecewiseLinear(rows) => rows.len(),
            DecodedValues::PiecewiseStep(rows) => rows.len(),
        }
    }

    /// Whether there are no timesteps. Always true for [`Self::Raw`].
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Decode `array` according to `element_type`.
///
/// `leading_dims` is how many leading axes precede the per-step element shape —
/// see [`crate::TimeSeriesType::leading_dims`]. The decoded entries are in
/// row-major order over those axes, so for a `Deterministic` of `[H, count, k]`
/// entry `i * count + j` is window `j`'s step `i`.
pub fn decode(
    array: &TypedArray,
    element_type: ElementType,
    leading_dims: usize,
) -> Result<DecodedValues> {
    element_type.validate_array(array, leading_dims)?;
    if array.dtype != Dtype::F64 {
        return Ok(DecodedValues::Raw);
    }
    let values = array
        .to_f64_vec()
        .map_err(TimeSeriesError::IntegrityError)?;
    let width = match element_type {
        ElementType::Scalar(_) => return Ok(DecodedValues::Raw),
        ElementType::Tuple { arity, .. } => arity,
        ElementType::LinearFunction => 2,
        ElementType::QuadraticFunction => 3,
        ElementType::PiecewiseLinear | ElementType::PiecewiseStep => array.shape[leading_dims],
    };
    // `chunks_exact` panics on a zero width, and `element_type_of` manufactures
    // exactly that for an empty `Tuple` — which `validate_array` then accepts,
    // since arity 0 expects element dims `[0]`. `ElementType::parse` refuses the
    // `tuple(0,…)` spelling, so such a row cannot arrive from a catalog, but the
    // crate's own `element_type_of` -> `encode` -> `decode` round trip reaches it
    // through the public API.
    if width == 0 {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "element_type {element_type} has no values per timestep, so there is nothing to decode"
        )));
    }
    let rows = values.chunks_exact(width);
    Ok(match element_type {
        ElementType::Scalar(_) => unreachable!("returned above"),
        ElementType::Tuple { .. } => DecodedValues::Tuple(rows.map(<[f64]>::to_vec).collect()),
        ElementType::LinearFunction => DecodedValues::LinearFunction(
            rows.map(|r| LinearFunction {
                proportional: r[0],
                constant: r[1],
            })
            .collect(),
        ),
        ElementType::QuadraticFunction => DecodedValues::QuadraticFunction(
            rows.map(|r| QuadraticFunction {
                quadratic: r[0],
                proportional: r[1],
                constant: r[2],
            })
            .collect(),
        ),
        ElementType::PiecewiseLinear => DecodedValues::PiecewiseLinear(
            rows.map(|r| {
                // Row width and every count were checked by `validate_array`.
                let n = r[0] as usize;
                (0..n)
                    .map(|k| XyPoint {
                        x: r[1 + 2 * k],
                        y: r[2 + 2 * k],
                    })
                    .collect()
            })
            .collect(),
        ),
        ElementType::PiecewiseStep => DecodedValues::PiecewiseStep(
            rows.map(|r| {
                let n = r[0] as usize;
                // `n` x-coords then `n - 1` y-values, so the row's used span is
                // `2n` — except for an empty timestep, whose only slot is the
                // count itself.
                let used = if n == 0 { 1 } else { 2 * n };
                StepFunction {
                    x: r[1..1 + n].to_vec(),
                    y: r[1 + n..used].to_vec(),
                }
            })
            .collect(),
        ),
    })
}

/// Encode per-timestep values into the flat array the store holds.
///
/// `leading_dims` is the shape of the axes that precede the per-step element
/// shape (`[length]` for a static series, `[H, count]` for a `Deterministic`,
/// …); its product must equal the number of decoded entries. The trailing dims
/// are derived from the element type — for the ragged kinds, from the widest
/// timestep.
pub fn encode(values: &DecodedValues, leading_dims: &[usize]) -> Result<TypedArray> {
    let expected_rows: usize = leading_dims.iter().product();
    let (width, flat) = match values {
        DecodedValues::Raw => {
            return Err(TimeSeriesError::InvalidParameter(
                "DecodedValues::Raw carries no values to encode: build the TypedArray directly"
                    .into(),
            ));
        }
        DecodedValues::Tuple(rows) => {
            let width = rows.first().map(Vec::len).unwrap_or(0);
            // A tuple's arity is carried by its rows, so a series with no rows —
            // or rows of no values — cannot say what it is. `ElementType::parse`
            // refuses the `tuple(0,…)` spelling this would produce, so encoding
            // it would write a row that cannot be read back.
            if width == 0 {
                return Err(TimeSeriesError::InvalidParameter(
                    "an empty tuple carries no arity, and tuple(0,…) is not a valid \
                     element type: use encode_as to declare the arity, or build the \
                     TypedArray directly"
                        .into(),
                ));
            }
            if let Some(bad) = rows.iter().position(|r| r.len() != width) {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "tuple rows must all have the same arity: row {bad} has {} values, \
                     expected {width}",
                    rows[bad].len()
                )));
            }
            (width, rows.concat())
        }
        DecodedValues::LinearFunction(rows) => (
            2,
            rows.iter()
                .flat_map(|f| [f.proportional, f.constant])
                .collect(),
        ),
        DecodedValues::QuadraticFunction(rows) => (
            3,
            rows.iter()
                .flat_map(|f| [f.quadratic, f.proportional, f.constant])
                .collect(),
        ),
        DecodedValues::PiecewiseLinear(rows) => {
            let width = 1 + 2 * rows.iter().map(Vec::len).max().unwrap_or(0);
            let mut flat = vec![0.0; rows.len() * width];
            for (row, points) in rows.iter().enumerate() {
                let out = &mut flat[row * width..(row + 1) * width];
                out[0] = points.len() as f64;
                for (k, p) in points.iter().enumerate() {
                    out[1 + 2 * k] = p.x;
                    out[2 + 2 * k] = p.y;
                }
            }
            (width, flat)
        }
        DecodedValues::PiecewiseStep(rows) => {
            for (row, step) in rows.iter().enumerate() {
                let expected = step.x.len().saturating_sub(1);
                if step.y.len() != expected {
                    return Err(TimeSeriesError::InvalidParameter(format!(
                        "piecewise_step row {row} has {} x-coords, so it needs {expected} \
                         y-values, but has {}",
                        step.x.len(),
                        step.y.len()
                    )));
                }
            }
            let width = (2 * rows.iter().map(|s| s.x.len()).max().unwrap_or(0)).max(1);
            let mut flat = vec![0.0; rows.len() * width];
            for (row, step) in rows.iter().enumerate() {
                let out = &mut flat[row * width..(row + 1) * width];
                out[0] = step.x.len() as f64;
                out[1..1 + step.x.len()].copy_from_slice(&step.x);
                out[1 + step.x.len()..1 + step.x.len() + step.y.len()].copy_from_slice(&step.y);
            }
            (width, flat)
        }
    };

    // A zero width means an empty tuple element type, which holds no rows at all.
    let rows = flat.len().checked_div(width).unwrap_or(0);
    if rows != expected_rows {
        return Err(TimeSeriesError::InvalidParameter(format!(
            "{rows} decoded timesteps do not fill leading dims {leading_dims:?} \
             ({expected_rows} expected)"
        )));
    }
    let mut shape = leading_dims.to_vec();
    shape.push(width);
    TypedArray::from_slice(shape, &flat).map_err(TimeSeriesError::InvalidParameter)
}

/// Encode `values` under a *declared* [`ElementType`], and check the result
/// against it.
///
/// [`encode`] infers the layout from the values, which is enough for every kind
/// whose width is implied by its own rows. A tuple is the exception: its arity
/// lives in its rows, so a series with none — a legal, storable, zero-length
/// series — cannot say what it is, and [`encode`] refuses it. Here the arity is
/// not missing: `element_type` declared it, so the empty packing is built from
/// the declaration.
///
/// Everything else defers to [`encode`] and is then validated, so the array that
/// comes back is one `element_type` actually describes — ready to pair with it
/// on a write, the way the `from_values` constructors pair them for a caller
/// whose values can name their own type.
///
/// ```
/// # use infrastore_core::{DecodedValues, Dtype, ElementType, Period, SingleTimeSeries, encode_as};
/// # use chrono::{Duration, TimeZone, Utc};
/// let tuple3 = ElementType::Tuple { arity: 3, dtype: Dtype::F64 };
/// let array = encode_as(&DecodedValues::Tuple(Vec::new()), &[0], tuple3)?;
/// assert_eq!(array.shape, vec![0, 3]);
/// let series = SingleTimeSeries::new(
///     Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
///     Period::Fixed(Duration::hours(1)),
///     array,
///     "variable_cost",
/// )
/// .with_element_type(tuple3);
/// assert_eq!(series.length, 0);
/// # Ok::<(), infrastore_core::TimeSeriesError>(())
/// ```
pub fn encode_as(
    values: &DecodedValues,
    leading_dims: &[usize],
    element_type: ElementType,
) -> Result<TypedArray> {
    let array = match (values, element_type) {
        (DecodedValues::Tuple(rows), ElementType::Tuple { arity, .. }) if rows.is_empty() => {
            let expected_rows: usize = leading_dims.iter().product();
            if expected_rows != 0 {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "0 decoded timesteps do not fill leading dims {leading_dims:?} \
                     ({expected_rows} expected)"
                )));
            }
            let mut shape = leading_dims.to_vec();
            shape.push(arity);
            TypedArray::new(Dtype::F64, shape, Vec::new())
                .map_err(TimeSeriesError::InvalidParameter)?
        }
        _ => encode(values, leading_dims)?,
    };
    // The values decide the packing and `element_type` is what the row is stored
    // under, so the two can disagree: `tuple(3,f64)` given two-value rows packs
    // to width 2. Checking here is what lets a caller pair them without waiting
    // for `Store::add` to reject the pair after the fact.
    element_type.validate_array(&array, leading_dims.len())?;
    Ok(array)
}

/// The element type an encode of `values` produces, for callers that need to
/// declare it on the write request.
///
/// A `Tuple` with no rows has no arity to report, so this names `tuple(0,f64)`,
/// which is not a legal element type. Declare the arity to [`encode_as`] rather
/// than asking here.
pub fn element_type_of(values: &DecodedValues) -> Option<ElementType> {
    Some(match values {
        DecodedValues::Raw => return None,
        DecodedValues::Tuple(rows) => ElementType::Tuple {
            arity: rows.first().map(Vec::len).unwrap_or(0),
            dtype: Dtype::F64,
        },
        DecodedValues::LinearFunction(_) => ElementType::LinearFunction,
        DecodedValues::QuadraticFunction(_) => ElementType::QuadraticFunction,
        DecodedValues::PiecewiseLinear(_) => ElementType::PiecewiseLinear,
        DecodedValues::PiecewiseStep(_) => ElementType::PiecewiseStep,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_and_non_f64_arrays_decode_to_raw() {
        let scalars = TypedArray::from_f64(vec![3], &[1.0, 2.0, 3.0]);
        assert_eq!(
            decode(&scalars, ElementType::Scalar(Dtype::F64), 1).unwrap(),
            DecodedValues::Raw
        );
        let ints = TypedArray::from_slice(vec![2, 3], &[1i32, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(
            decode(
                &ints,
                ElementType::Tuple {
                    arity: 3,
                    dtype: Dtype::I32
                },
                1
            )
            .unwrap(),
            DecodedValues::Raw
        );
    }

    #[test]
    fn decode_rejects_an_array_the_element_type_does_not_describe() {
        let array = TypedArray::from_f64(vec![2, 4], &[0.0; 8]);
        assert!(decode(&array, ElementType::QuadraticFunction, 1).is_err());
        assert!(decode(&array, ElementType::PiecewiseLinear, 1).is_err());
    }

    #[test]
    fn encode_rejects_a_row_count_that_does_not_fill_the_leading_dims() {
        let values = DecodedValues::LinearFunction(vec![
            LinearFunction {
                proportional: 1.0,
                constant: 2.0,
            };
            5
        ]);
        let err = encode(&values, &[2, 3]).unwrap_err();
        assert!(err.to_string().contains("5 decoded timesteps"), "{err}");
        assert!(encode(&values, &[5]).is_ok());
    }

    #[test]
    fn encode_rejects_ragged_tuple_rows() {
        let values = DecodedValues::Tuple(vec![vec![1.0, 2.0], vec![3.0]]);
        let err = encode(&values, &[2]).unwrap_err();
        assert!(err.to_string().contains("same arity"), "{err}");
    }

    #[test]
    fn encode_rejects_a_step_function_with_the_wrong_y_count() {
        let values = DecodedValues::PiecewiseStep(vec![StepFunction {
            x: vec![1.0, 2.0, 3.0],
            y: vec![10.0],
        }]);
        let err = encode(&values, &[1]).unwrap_err();
        assert!(err.to_string().contains("needs 2 y-values"), "{err}");
    }

    #[test]
    fn ragged_encode_pads_to_the_widest_timestep() {
        let values = DecodedValues::PiecewiseLinear(vec![
            vec![XyPoint { x: 1.0, y: 10.0 }],
            vec![
                XyPoint { x: 1.0, y: 10.0 },
                XyPoint { x: 2.0, y: 20.0 },
                XyPoint { x: 3.0, y: 30.0 },
            ],
        ]);
        let array = encode(&values, &[2]).unwrap();
        assert_eq!(array.shape, vec![2, 7]);
        assert_eq!(
            array.to_f64_vec().unwrap(),
            vec![
                1.0, 1.0, 10.0, 0.0, 0.0, 0.0, 0.0, //
                3.0, 1.0, 10.0, 2.0, 20.0, 3.0, 30.0,
            ]
        );
        assert_eq!(
            decode(&array, ElementType::PiecewiseLinear, 1).unwrap(),
            values
        );
    }

    #[test]
    fn an_all_empty_piecewise_step_series_keeps_a_width_of_one() {
        let values = DecodedValues::PiecewiseStep(vec![
            StepFunction {
                x: vec![],
                y: vec![],
            };
            3
        ]);
        let array = encode(&values, &[3]).unwrap();
        assert_eq!(array.shape, vec![3, 1]);
        assert_eq!(
            decode(&array, ElementType::PiecewiseStep, 1).unwrap(),
            values
        );
    }

    const TUPLE3: ElementType = ElementType::Tuple {
        arity: 3,
        dtype: Dtype::F64,
    };

    #[test]
    fn encode_as_takes_an_empty_tuple_series_arity_from_the_declaration() {
        // The case `encode` cannot serve: no rows, so no arity in the values.
        let empty = DecodedValues::Tuple(Vec::new());
        assert!(encode(&empty, &[0]).is_err());

        let array = encode_as(&empty, &[0], TUPLE3).unwrap();
        assert_eq!(array.shape, vec![0, 3]);
        assert!(array.bytes.is_empty());
        // And it reads back as the empty series it is, rather than as a row the
        // decoder has to refuse.
        assert_eq!(
            decode(&array, TUPLE3, 1).unwrap(),
            DecodedValues::Tuple(Vec::new())
        );
    }

    #[test]
    fn encode_as_still_holds_an_empty_tuple_series_to_its_leading_dims() {
        let empty = DecodedValues::Tuple(Vec::new());
        let err = encode_as(&empty, &[4], TUPLE3).unwrap_err();
        assert!(
            err.to_string().contains("do not fill leading dims"),
            "{err}"
        );
        // A forecast whose window geometry multiplies out to zero is still empty.
        assert_eq!(
            encode_as(&empty, &[6, 0], TUPLE3).unwrap().shape,
            vec![6, 0, 3]
        );
    }

    #[test]
    fn encode_as_defers_to_encode_and_checks_the_declaration() {
        let rows = DecodedValues::Tuple(vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]]);
        assert_eq!(
            encode_as(&rows, &[2], TUPLE3)
                .unwrap()
                .to_f64_vec()
                .unwrap(),
            vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]
        );
        // The values pack to width 2, which is not what the declaration says.
        let narrow = DecodedValues::Tuple(vec![vec![1.0, 2.0]]);
        let err = encode_as(&narrow, &[1], TUPLE3).unwrap_err();
        assert!(err.to_string().contains("element dims"), "{err}");
        // A declaration of the wrong *kind* is caught the same way.
        let err = encode_as(&rows, &[2], ElementType::LinearFunction).unwrap_err();
        assert!(err.to_string().contains("element dims"), "{err}");
    }
}
