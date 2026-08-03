//! Canonical SHA-256 hashing for arrays, feature maps, and timestamp vectors.
//!
//! Stability of these hashes is part of the public on-disk contract. Any change
//! here that perturbs a stored hash is a format-breaking change and must bump
//! [`crate::DATA_FORMAT_VERSION`]. The `golden_hash_pin` integration test pins
//! the SHA-256 of one fixed array as a tripwire; it does not cover every dtype,
//! shape, or the feature-map domain, so it is not a substitute for that rule.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::types::array::{Dtype, TypedArray};
use crate::types::metadata::{FeatureValue, Features};

/// Compute the canonical content hash for a [`TypedArray`].
///
/// Domain: dtype tag → shape (rank then each dim as u64 LE) → element bytes in
/// row-major order. For float dtypes, NaN values are normalized to a single
/// canonical quiet-NaN bit pattern before hashing so semantically-equal arrays
/// do not collide on payload bits; integer/bool bytes are hashed verbatim.
/// Identity is therefore `(dtype, shape, content)`: arrays of different dtype or
/// shape never collide.
pub fn array_hash(data: &TypedArray) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data.dtype.as_str().as_bytes());
    hasher.update([0u8]);

    hasher.update((data.shape.len() as u64).to_le_bytes());
    for dim in &data.shape {
        hasher.update((*dim as u64).to_le_bytes());
    }

    match data.dtype {
        Dtype::F64 => update_f64_canonical_nans(&mut hasher, &data.bytes),
        Dtype::F32 => update_f32_canonical_nans(&mut hasher, &data.bytes),
        Dtype::I64
        | Dtype::I32
        | Dtype::I16
        | Dtype::I8
        | Dtype::U64
        | Dtype::U32
        | Dtype::U16
        | Dtype::U8
        | Dtype::Bool => {
            hasher.update(&data.bytes);
        }
    }

    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Feed `f64` bytes to `hasher` with every NaN collapsed to one canonical bit
/// pattern.
///
/// A non-NaN element hashes as exactly the little-endian bytes it is stored as,
/// so runs of them — the overwhelming common case — go in with a single
/// `update` rather than one per element. That is the whole point: `Sha256::update`
/// has real per-call cost, and an 8-byte-at-a-time loop makes hashing an array
/// scale with its element count instead of its size.
///
/// A trailing partial element is ignored, matching `chunks_exact`. It cannot
/// occur for a well-formed [`TypedArray`], whose byte length is validated
/// against `shape × dtype.size()`.
fn update_f64_canonical_nans(hasher: &mut Sha256, bytes: &[u8]) {
    const EXP_MASK: u64 = 0x7ff0_0000_0000_0000;
    const FRAC_MASK: u64 = 0x000f_ffff_ffff_ffff;

    let bytes = &bytes[..bytes.len() - bytes.len() % 8];
    let mut run_start = 0;
    for (index, chunk) in bytes.chunks_exact(8).enumerate() {
        let bits = u64::from_le_bytes(chunk.try_into().unwrap());
        // IEEE NaN: exponent all ones, mantissa non-zero. Covers signaling and
        // sign-negative NaNs, exactly as `f64::is_nan` does.
        if bits & EXP_MASK == EXP_MASK && bits & FRAC_MASK != 0 {
            let offset = index * 8;
            hasher.update(&bytes[run_start..offset]);
            hasher.update(f64::NAN.to_bits().to_le_bytes());
            run_start = offset + 8;
        }
    }
    hasher.update(&bytes[run_start..]);
}

/// [`update_f64_canonical_nans`] for 4-byte elements.
fn update_f32_canonical_nans(hasher: &mut Sha256, bytes: &[u8]) {
    const EXP_MASK: u32 = 0x7f80_0000;
    const FRAC_MASK: u32 = 0x007f_ffff;

    let bytes = &bytes[..bytes.len() - bytes.len() % 4];
    let mut run_start = 0;
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let bits = u32::from_le_bytes(chunk.try_into().unwrap());
        if bits & EXP_MASK == EXP_MASK && bits & FRAC_MASK != 0 {
            let offset = index * 4;
            hasher.update(&bytes[run_start..offset]);
            hasher.update(f32::NAN.to_bits().to_le_bytes());
            run_start = offset + 4;
        }
    }
    hasher.update(&bytes[run_start..]);
}

/// Compute the canonical content hash for a `Features` map.
///
/// Iteration order is the BTreeMap's sorted-by-key order. Each entry contributes
/// a length-prefixed key, a kind tag, and the value bytes. NaNs in `Float`
/// values are canonicalized like `array_hash`.
pub fn features_hash(features: &Features) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"features\0");
    hasher.update((features.len() as u64).to_le_bytes());

    for (key, value) in features.iter() {
        let key_bytes = key.as_bytes();
        hasher.update((key_bytes.len() as u64).to_le_bytes());
        hasher.update(key_bytes);

        match value {
            FeatureValue::Int(v) => {
                hasher.update(b"i");
                hasher.update(v.to_le_bytes());
            }
            FeatureValue::Float(v) => {
                hasher.update(b"f");
                let bits = if v.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    v.to_bits()
                };
                hasher.update(bits.to_le_bytes());
            }
            FeatureValue::Bool(v) => {
                hasher.update(b"b");
                hasher.update([*v as u8]);
            }
            FeatureValue::Str(v) => {
                hasher.update(b"s");
                let v_bytes = v.as_bytes();
                hasher.update((v_bytes.len() as u64).to_le_bytes());
                hasher.update(v_bytes);
            }
        }
    }

    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Compute the canonical content hash for an explicit timestamp vector.
///
/// Domain: the canonical encoding from [`crate::timestamps`], which is exactly
/// what the `timestamp_sets` row holds — so the hash addresses the stored bytes
/// rather than a second, parallel serialization of the same values. Two vectors
/// hash equal iff they hold the same timestamps in the same order.
///
/// This is what lets many `NonSequentialTimeSeries` share one stored time axis,
/// and it doubles as the cohort key the packed on-disk layout groups their
/// arrays by (see [`crate::storage::common`]).
pub fn timestamps_hash(timestamps: &[DateTime<Utc>]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"timestamps\0");
    hasher.update(crate::timestamps::encode(timestamps));
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Hex-encode a 32-byte hash for storage in TEXT columns / HDF5 hash datasets.
pub fn hash_hex(hash: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (byte, pair) in hash.iter().zip(out.chunks_exact_mut(2)) {
        pair[0] = HEX[(byte >> 4) as usize];
        pair[1] = HEX[(byte & 0x0f) as usize];
    }
    // Every byte written above came from `HEX`, so this cannot fail.
    String::from_utf8(out.to_vec()).expect("hex digits are ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f64_array(shape: Vec<usize>, values: &[f64]) -> TypedArray {
        TypedArray::from_f64(shape, values)
    }

    #[test]
    fn equal_arrays_hash_equal() {
        let a = f64_array(vec![3], &[1.0, 2.0, 3.0]);
        let b = f64_array(vec![3], &[1.0, 2.0, 3.0]);
        assert_eq!(array_hash(&a), array_hash(&b));
    }

    #[test]
    fn different_arrays_hash_differ() {
        let a = f64_array(vec![3], &[1.0, 2.0, 3.0]);
        let b = f64_array(vec![3], &[1.0, 2.0, 4.0]);
        assert_ne!(array_hash(&a), array_hash(&b));
    }

    #[test]
    fn different_dtypes_hash_differ() {
        let f = f64_array(vec![2], &[1.0, 2.0]);
        // Same logical values stored as i64 must hash differently.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1i64.to_le_bytes());
        bytes.extend_from_slice(&2i64.to_le_bytes());
        let i = TypedArray::new(Dtype::I64, vec![2], bytes).unwrap();
        assert_ne!(array_hash(&f), array_hash(&i));
    }

    #[test]
    fn nan_canonicalization() {
        let a = f64_array(vec![3], &[1.0, f64::NAN, 3.0]);
        // Inject a different NaN bit pattern into `b` at index 1.
        let alt_nan = f64::from_bits(0x7ff8_0000_0000_0001);
        assert!(alt_nan.is_nan());
        let b = f64_array(vec![3], &[1.0, alt_nan, 3.0]);

        assert_eq!(array_hash(&a), array_hash(&b));
        // Sanity: the two are bitwise different at index 1.
        assert_ne!(a.bytes[8..16], b.bytes[8..16]);

        // Mutating an actual value still changes the hash.
        let c = f64_array(vec![3], &[1.5, alt_nan, 3.0]);
        assert_ne!(array_hash(&c), array_hash(&b));
    }

    /// The pre-optimization loop: decode every element, canonicalize NaNs, and
    /// feed the bits back one element at a time. `array_hash`'s run-batching
    /// must be indistinguishable from this, since the result is on-disk state.
    fn reference_array_hash(data: &TypedArray) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(data.dtype.as_str().as_bytes());
        hasher.update([0u8]);
        hasher.update((data.shape.len() as u64).to_le_bytes());
        for dim in &data.shape {
            hasher.update((*dim as u64).to_le_bytes());
        }
        match data.dtype {
            Dtype::F64 => {
                for c in data.bytes.chunks_exact(8) {
                    let v = f64::from_le_bytes(c.try_into().unwrap());
                    let bits = if v.is_nan() {
                        f64::NAN.to_bits()
                    } else {
                        v.to_bits()
                    };
                    hasher.update(bits.to_le_bytes());
                }
            }
            Dtype::F32 => {
                for c in data.bytes.chunks_exact(4) {
                    let v = f32::from_le_bytes(c.try_into().unwrap());
                    let bits = if v.is_nan() {
                        f32::NAN.to_bits()
                    } else {
                        v.to_bits()
                    };
                    hasher.update(bits.to_le_bytes());
                }
            }
            _ => hasher.update(&data.bytes),
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&hasher.finalize());
        out
    }

    #[test]
    fn float_hashing_matches_the_element_at_a_time_reference() {
        // NaN placement drives the run-batching, so cover the boundaries: none,
        // leading, trailing, adjacent, interior, and all.
        let n = f64::NAN;
        let alt = f64::from_bits(0xfff8_0000_0000_0001); // negative signaling NaN
        let cases: Vec<Vec<f64>> = vec![
            vec![],
            vec![1.0],
            vec![n],
            vec![1.0, 2.0, 3.0, 4.0],
            vec![n, 1.0, 2.0, 3.0],
            vec![1.0, 2.0, 3.0, n],
            vec![1.0, n, alt, 4.0],
            vec![n, n, n, n],
            vec![
                0.0,
                -0.0,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::MIN_POSITIVE,
            ],
        ];
        for values in cases {
            let a = f64_array(vec![values.len()], &values);
            assert_eq!(
                array_hash(&a),
                reference_array_hash(&a),
                "f64 mismatch for {values:?}"
            );

            let f32_values: Vec<f32> = values.iter().map(|v| *v as f32).collect();
            let bytes: Vec<u8> = f32_values.iter().flat_map(|v| v.to_le_bytes()).collect();
            let b = TypedArray::new(Dtype::F32, vec![f32_values.len()], bytes).unwrap();
            assert_eq!(
                array_hash(&b),
                reference_array_hash(&b),
                "f32 mismatch for {values:?}"
            );
        }
    }

    #[test]
    fn hash_hex_encodes_lowercase_and_pads() {
        let mut hash = [0u8; 32];
        hash[0] = 0x00;
        hash[1] = 0x0f;
        hash[2] = 0xa5;
        hash[31] = 0xff;
        let hex = hash_hex(&hash);
        assert_eq!(hex.len(), 64);
        assert!(hex.starts_with("000fa5"));
        assert!(hex.ends_with("ff"));
        // Matches the `format!`-per-byte encoding it replaced.
        let expected: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected);
    }

    #[test]
    fn shape_affects_hash() {
        let flat = f64_array(vec![4], &[1.0, 2.0, 3.0, 4.0]);
        let square = f64_array(vec![2, 2], &[1.0, 2.0, 3.0, 4.0]);
        assert_ne!(array_hash(&flat), array_hash(&square));
    }

    #[test]
    fn features_hash_is_order_independent() {
        let mut a = Features::new();
        a.insert("model_year".into(), FeatureValue::Int(2030));
        a.insert("scenario".into(), FeatureValue::Int(1));

        let mut b = Features::new();
        b.insert("scenario".into(), FeatureValue::Int(1));
        b.insert("model_year".into(), FeatureValue::Int(2030));

        assert_eq!(features_hash(&a), features_hash(&b));
    }

    #[test]
    fn timestamps_hash_addresses_the_vector() {
        use chrono::{Duration, TimeZone};
        let t0 = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
        let a: Vec<DateTime<Utc>> = (0..4).map(|k| t0 + Duration::hours(k)).collect();
        let b = a.clone();
        assert_eq!(timestamps_hash(&a), timestamps_hash(&b));
        // Order is part of the identity, and so is an empty vector's emptiness.
        let mut reversed = a.clone();
        reversed.reverse();
        assert_ne!(timestamps_hash(&a), timestamps_hash(&reversed));
        assert_ne!(timestamps_hash(&a), timestamps_hash(&[]));
        // Domain separation: a timestamp vector never collides with a feature
        // set or an array, whatever the payload bytes.
        assert_ne!(timestamps_hash(&[]), features_hash(&Features::new()));
    }

    #[test]
    fn features_hash_distinguishes_kinds() {
        let mut a = Features::new();
        a.insert("k".into(), FeatureValue::Int(1));
        let mut b = Features::new();
        b.insert("k".into(), FeatureValue::Bool(true));
        assert_ne!(features_hash(&a), features_hash(&b));
    }
}
