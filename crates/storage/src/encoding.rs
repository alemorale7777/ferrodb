//! Order-preserving key encodings: raw byte comparison must equal logical order.

/// Encode an `i64` so that unsigned big-endian byte order equals signed integer order.
/// Flipping the sign bit maps `[i64::MIN, i64::MAX]` onto `[0, u64::MAX]` monotonically.
pub fn encode_i64(v: i64) -> [u8; 8] {
    ((v as u64) ^ (1u64 << 63)).to_be_bytes()
}

/// Inverse of [`encode_i64`].
pub fn decode_i64(b: &[u8]) -> i64 {
    let u = u64::from_be_bytes(b[..8].try_into().unwrap());
    (u ^ (1u64 << 63)) as i64
}

/// Byte strings already compare lexicographically; provided for symmetry.
pub fn encode_bytes(v: &[u8]) -> Vec<u8> {
    v.to_vec()
}
