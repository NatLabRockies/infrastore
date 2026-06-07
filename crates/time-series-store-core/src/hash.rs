//! Canonical SHA-256 hashing for arrays and feature maps.
//!
//! Stability of these hashes is part of the public on-disk contract. The
//! `hash_golden` integration test pins the SHA-256 of representative inputs;
//! any change here that perturbs those values is a format-breaking change and
//! must bump [`crate::DATA_FORMAT_VERSION`].

use ndarray::ArrayD;
use sha2::{Digest, Sha256};

use crate::types::metadata::{FeatureValue, Features};

/// Tag identifying the element dtype. Currently only f64 is supported.
const DTYPE_TAG_F64: &[u8] = b"f64\0";

/// Compute the canonical content hash for an `ArrayD<f64>`.
///
/// Domain: dtype tag → shape (rank then each dim as u64 LE) → elements (each
/// `f64::to_le_bytes`) in row-major iteration order. NaN values are normalized
/// to a single canonical quiet-NaN bit pattern before hashing so that
/// semantically-equal arrays do not collide on payload bits.
pub fn array_hash(data: &ArrayD<f64>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DTYPE_TAG_F64);

    let shape = data.shape();
    hasher.update((shape.len() as u64).to_le_bytes());
    for dim in shape {
        hasher.update((*dim as u64).to_le_bytes());
    }

    for element in data.iter() {
        let bits = if element.is_nan() {
            f64::NAN.to_bits()
        } else {
            element.to_bits()
        };
        hasher.update(bits.to_le_bytes());
    }

    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
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

/// Hex-encode a 32-byte hash for storage in TEXT columns / NetCDF string vars.
pub fn hash_hex(hash: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for byte in hash {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::array;

    #[test]
    fn equal_arrays_hash_equal() {
        let a = array![1.0_f64, 2.0, 3.0].into_dyn();
        let b = array![1.0_f64, 2.0, 3.0].into_dyn();
        assert_eq!(array_hash(&a), array_hash(&b));
    }

    #[test]
    fn different_arrays_hash_differ() {
        let a = array![1.0_f64, 2.0, 3.0].into_dyn();
        let b = array![1.0_f64, 2.0, 4.0].into_dyn();
        assert_ne!(array_hash(&a), array_hash(&b));
    }

    #[test]
    fn nan_canonicalization() {
        // Two arrays with NaNs that may or may not have different payload bits
        // must still hash to the same value.
        let mut a = array![1.0_f64, f64::NAN, 3.0].into_dyn();
        let mut b = array![1.0_f64, f64::NAN, 3.0].into_dyn();

        // Inject a different NaN bit pattern into `b`.
        let alt_nan = f64::from_bits(0x7ff8_0000_0000_0001);
        assert!(alt_nan.is_nan());
        b[1] = alt_nan;

        assert_eq!(array_hash(&a), array_hash(&b));
        // Sanity: a and b are still bitwise different at index 1.
        assert_ne!(a[1].to_bits(), b[1].to_bits());

        // Mutating an actual value still changes the hash.
        a[0] = 1.5;
        assert_ne!(array_hash(&a), array_hash(&b));
    }

    #[test]
    fn shape_affects_hash() {
        let flat = ArrayD::from_shape_vec(vec![4], vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        let square = ArrayD::from_shape_vec(vec![2, 2], vec![1.0, 2.0, 3.0, 4.0]).unwrap();
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
    fn features_hash_distinguishes_kinds() {
        let mut a = Features::new();
        a.insert("k".into(), FeatureValue::Int(1));
        let mut b = Features::new();
        b.insert("k".into(), FeatureValue::Bool(true));
        assert_ne!(features_hash(&a), features_hash(&b));
    }
}
