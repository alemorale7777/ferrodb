use storage::encoding::{decode_i64, encode_i64};

#[test]
fn i64_encoding_is_order_preserving() {
    let mut vals = [-5i64, 0, 3, i64::MIN, i64::MAX, -1, 1];
    let mut enc: Vec<[u8; 8]> = vals.iter().map(|v| encode_i64(*v)).collect();
    vals.sort();
    enc.sort(); // sort by raw bytes
    let decoded: Vec<i64> = enc.iter().map(|b| decode_i64(b)).collect();
    assert_eq!(decoded, vals.to_vec());
}
