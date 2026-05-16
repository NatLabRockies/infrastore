//! Canonical SHA-256 hashing for arrays and feature maps.
//!
//! Stability of these hashes is part of the public on-disk contract. The
//! `hash_golden` integration test pins the SHA-256 of representative inputs;
//! any change here that perturbs those values is a format-breaking change and
//! must bump [`crate::DATA_FORMAT_VERSION`].

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
        Dtype::I64 | Dtype::I32 | Dtype::U64 | Dtype::Bool => {
            hasher.update(&data.bytes);
        }
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
    fn features_hash_distinguishes_kinds() {
        let mut a = Features::new();
        a.insert("k".into(), FeatureValue::Int(1));
        let mut b = Features::new();
        b.insert("k".into(), FeatureValue::Bool(true));
        assert_ne!(features_hash(&a), features_hash(&b));
    }
}
