//! The shared corpus every binding's codec is tested against.
//!
//! Each vector pins one `element_type` at one array layout: the stored shape,
//! the little-endian bytes, and the values a correct decoder produces. The Rust
//! test below round-trips them; the same corpus is exported to
//! `conformance/element_type_vectors.json` at the repo root, which the Python,
//! Julia, and TypeScript codec tests read so all four implementations are held
//! to one definition of the encoding rather than to each other.
//!
//! Adding an encoding means adding a vector here and regenerating that file
//! (`UPDATE_CONFORMANCE_VECTORS=1 cargo test -p infrastore-core conformance`).

use serde::Serialize;

use super::{DecodedValues, LinearFunction, QuadraticFunction, StepFunction, XyPoint};
use crate::types::element_type::ElementType;
use crate::types::time_series::TimeSeriesType;

/// One conformance case: an array as stored, plus what it must decode to.
#[derive(Debug, Clone, Serialize)]
pub struct ConformanceVector {
    /// Stable identifier, used in test failure messages across bindings.
    pub name: &'static str,
    /// Canonical `element_type` string.
    pub element_type: String,
    /// The time-series type whose layout the shape follows; it fixes
    /// `leading_dims`.
    pub time_series_type: &'static str,
    /// How many leading axes precede the per-step element shape.
    pub leading_dims: usize,
    /// Full array shape, `leading_dims` axes then the per-step element shape.
    pub shape: Vec<usize>,
    /// The stored elements as `f64`s in row-major order — the flat form a
    /// binding builds before encoding to bytes.
    pub values: Vec<f64>,
    /// The same elements as little-endian bytes, hex-encoded. This is the
    /// byte-level contract: a binding that produces different bytes for the
    /// same values has a byte-order or padding bug.
    pub bytes_hex: String,
    /// What a correct decoder returns.
    pub decoded: DecodedValues,
}

fn vector(
    name: &'static str,
    element_type: ElementType,
    time_series_type: TimeSeriesType,
    leading_dims_shape: &[usize],
    decoded: DecodedValues,
) -> ConformanceVector {
    let array = super::encode(&decoded, leading_dims_shape)
        .unwrap_or_else(|e| panic!("vector {name} does not encode: {e}"));
    ConformanceVector {
        name,
        element_type: element_type.to_string(),
        time_series_type: time_series_type.as_str(),
        leading_dims: leading_dims_shape.len(),
        shape: array.shape.clone(),
        values: array.to_f64_vec().expect("vectors are f64"),
        bytes_hex: hex(&array.bytes),
        decoded,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every conformance vector, in a stable order.
pub fn vectors() -> Vec<ConformanceVector> {
    vec![
        vector(
            "linear_function_static",
            ElementType::LinearFunction,
            TimeSeriesType::SingleTimeSeries,
            &[3],
            DecodedValues::LinearFunction(vec![
                LinearFunction {
                    proportional: 2.5,
                    constant: -1.0,
                },
                LinearFunction {
                    proportional: 0.0,
                    constant: 0.0,
                },
                LinearFunction {
                    proportional: 1e30,
                    constant: -0.5,
                },
            ]),
        ),
        vector(
            "quadratic_function_static",
            ElementType::QuadraticFunction,
            TimeSeriesType::SingleTimeSeries,
            &[2],
            DecodedValues::QuadraticFunction(vec![
                QuadraticFunction {
                    quadratic: 0.5,
                    proportional: 2.0,
                    constant: 3.0,
                },
                QuadraticFunction {
                    quadratic: -1.25,
                    proportional: 0.0,
                    constant: 7.5,
                },
            ]),
        ),
        vector(
            "quadratic_function_deterministic",
            ElementType::QuadraticFunction,
            TimeSeriesType::Deterministic,
            // [H = 2, count = 3, 3]
            &[2, 3],
            DecodedValues::QuadraticFunction(
                (0..6)
                    .map(|i| QuadraticFunction {
                        quadratic: i as f64,
                        proportional: i as f64 + 0.5,
                        constant: -(i as f64),
                    })
                    .collect(),
            ),
        ),
        vector(
            "tuple3_static",
            ElementType::Tuple {
                arity: 3,
                dtype: crate::types::array::Dtype::F64,
            },
            TimeSeriesType::SingleTimeSeries,
            &[2],
            DecodedValues::Tuple(vec![vec![1.0, 2.0, 3.0], vec![-4.0, 5.5, 6.25]]),
        ),
        vector(
            "piecewise_linear_static_ragged",
            ElementType::PiecewiseLinear,
            TimeSeriesType::SingleTimeSeries,
            &[3],
            DecodedValues::PiecewiseLinear(vec![
                vec![
                    XyPoint { x: 0.0, y: 1.0 },
                    XyPoint { x: 1.0, y: 3.0 },
                    XyPoint { x: 2.0, y: 8.0 },
                ],
                vec![XyPoint { x: 0.0, y: 2.0 }],
                // An empty timestep: leading count 0, the rest padding.
                vec![],
            ]),
        ),
        // Ragged, but with every timestep wide enough for the domain types that
        // require at least two points (InfrastructureSystems.jl's
        // `PiecewiseLinearData` / `PiecewiseStepData`). The store itself allows
        // the narrower rows above; this vector is what every binding can check.
        vector(
            "piecewise_linear_static_two_widths",
            ElementType::PiecewiseLinear,
            TimeSeriesType::SingleTimeSeries,
            &[2],
            DecodedValues::PiecewiseLinear(vec![
                vec![
                    XyPoint { x: 0.0, y: 1.0 },
                    XyPoint { x: 1.5, y: 3.0 },
                    XyPoint { x: 4.0, y: 8.25 },
                ],
                vec![XyPoint { x: 0.0, y: 2.0 }, XyPoint { x: 2.0, y: 6.0 }],
            ]),
        ),
        vector(
            "piecewise_step_static_two_widths",
            ElementType::PiecewiseStep,
            TimeSeriesType::SingleTimeSeries,
            &[2],
            DecodedValues::PiecewiseStep(vec![
                StepFunction {
                    x: vec![0.0, 1.0, 2.5],
                    y: vec![10.0, 20.0],
                },
                StepFunction {
                    x: vec![0.0, 5.0],
                    y: vec![7.5],
                },
            ]),
        ),
        vector(
            "piecewise_linear_probabilistic",
            ElementType::PiecewiseLinear,
            TimeSeriesType::Probabilistic,
            // [P = 2, H = 2, count = 1, width]
            &[2, 2, 1],
            DecodedValues::PiecewiseLinear(vec![
                vec![XyPoint { x: 0.0, y: 1.0 }, XyPoint { x: 1.0, y: 2.0 }],
                vec![XyPoint { x: 0.0, y: 3.0 }],
                vec![XyPoint { x: 0.0, y: 4.0 }, XyPoint { x: 1.0, y: 5.0 }],
                vec![],
            ]),
        ),
        vector(
            "piecewise_step_static_ragged",
            ElementType::PiecewiseStep,
            TimeSeriesType::SingleTimeSeries,
            &[3],
            DecodedValues::PiecewiseStep(vec![
                StepFunction {
                    x: vec![0.0, 1.0, 2.0],
                    y: vec![10.0, 20.0],
                },
                StepFunction {
                    x: vec![0.0, 5.0],
                    y: vec![7.5],
                },
                StepFunction {
                    x: vec![],
                    y: vec![],
                },
            ]),
        ),
        vector(
            "piecewise_step_all_empty",
            ElementType::PiecewiseStep,
            TimeSeriesType::SingleTimeSeries,
            &[2],
            DecodedValues::PiecewiseStep(vec![
                StepFunction {
                    x: vec![],
                    y: vec![],
                },
                StepFunction {
                    x: vec![],
                    y: vec![],
                },
            ]),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::types::array::TypedArray;

    /// The checked-in corpus every other binding reads.
    fn vectors_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../conformance/element_type_vectors.json")
    }

    fn rendered() -> String {
        let mut json = serde_json::to_string_pretty(&serde_json::json!({
            "comment": "Generated by infrastore-core's codec::conformance tests. \
                        Do not edit by hand; regenerate with \
                        UPDATE_CONFORMANCE_VECTORS=1 cargo test -p infrastore-core conformance.",
            "vectors": vectors(),
        }))
        .expect("vectors serialize");
        json.push('\n');
        json
    }

    /// Each vector's bytes must decode back to exactly the values it declares,
    /// and re-encoding those values must reproduce the same bytes.
    #[test]
    fn every_vector_round_trips() {
        for v in vectors() {
            let element_type: ElementType = v.element_type.parse().unwrap();
            let array = TypedArray::from_f64(v.shape.clone(), &v.values);
            assert_eq!(hex(&array.bytes), v.bytes_hex, "{}: bytes", v.name);

            let decoded = super::super::decode(&array, element_type, v.leading_dims)
                .unwrap_or_else(|e| panic!("{}: decode failed: {e}", v.name));
            assert_eq!(decoded, v.decoded, "{}: decoded values", v.name);

            let leading = &v.shape[..v.leading_dims];
            let re_encoded = super::super::encode(&decoded, leading)
                .unwrap_or_else(|e| panic!("{}: re-encode failed: {e}", v.name));
            assert_eq!(re_encoded, array, "{}: re-encode", v.name);
        }
    }

    #[test]
    fn vector_names_are_unique() {
        let mut names: Vec<&str> = vectors().iter().map(|v| v.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "vector names must be unique");
    }

    /// The exported JSON is what the Python, Julia, and TypeScript codec tests
    /// read, so it has to track the Rust corpus. Regenerate with
    /// `UPDATE_CONFORMANCE_VECTORS=1`.
    #[test]
    fn exported_vectors_file_is_current() {
        let path = vectors_path();
        let want = rendered();
        if std::env::var_os("UPDATE_CONFORMANCE_VECTORS").is_some() {
            std::fs::create_dir_all(path.parent().expect("path has a parent"))
                .expect("create conformance dir");
            std::fs::write(&path, &want).expect("write conformance vectors");
            return;
        }
        let have = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{} is missing ({e}); regenerate with UPDATE_CONFORMANCE_VECTORS=1 \
                 cargo test -p infrastore-core conformance",
                path.display()
            )
        });
        // A Windows checkout with `core.autocrlf` on hands back CRLF for the LF
        // this test wrote, and that difference says nothing about whether the
        // corpus is stale — so compare the content, not the line endings.
        assert_eq!(
            have.replace("\r\n", "\n"),
            want,
            "{} is out of date; regenerate with UPDATE_CONFORMANCE_VECTORS=1 \
             cargo test -p infrastore-core conformance",
            path.display()
        );
    }
}
