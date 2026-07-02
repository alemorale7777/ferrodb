//! Row (tuple) encoding: a null-bitmap followed by the non-null column values,
//! plus order-preserving key encodings for primary keys and hidden row ids.

use sql::ast::{DataType, Value};

use crate::EngineError;

/// Encode a row to bytes: `[null-bitmap][value0][value1]…` (nulls omit their value).
pub fn encode_tuple(row: &[Value]) -> Vec<u8> {
    let n = row.len();
    let bitmap_len = n.div_ceil(8);
    let mut out = vec![0u8; bitmap_len];
    for (i, v) in row.iter().enumerate() {
        if let Value::Null = v {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    for v in row {
        match v {
            Value::Null => {}
            Value::Integer(x) => out.extend_from_slice(&x.to_le_bytes()),
            Value::Real(x) => out.extend_from_slice(&x.to_le_bytes()),
            Value::Boolean(b) => out.push(if *b { 1 } else { 0 }),
            Value::Text(s) => {
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
    }
    out
}

/// Decode a row given the column `types` (bytes alone are untyped).
pub fn decode_tuple(types: &[DataType], bytes: &[u8]) -> Result<Vec<Value>, EngineError> {
    let n = types.len();
    let bitmap_len = n.div_ceil(8);
    let bitmap = &bytes[..bitmap_len];
    let mut pos = bitmap_len;
    let mut row = Vec::with_capacity(n);
    let corrupt = || EngineError::Type("corrupt tuple".into());
    for (i, ty) in types.iter().enumerate() {
        if (bitmap[i / 8] >> (i % 8)) & 1 == 1 {
            row.push(Value::Null);
            continue;
        }
        match ty {
            DataType::Integer => {
                let b = bytes.get(pos..pos + 8).ok_or_else(corrupt)?;
                row.push(Value::Integer(i64::from_le_bytes(b.try_into().unwrap())));
                pos += 8;
            }
            DataType::Real => {
                let b = bytes.get(pos..pos + 8).ok_or_else(corrupt)?;
                row.push(Value::Real(f64::from_le_bytes(b.try_into().unwrap())));
                pos += 8;
            }
            DataType::Boolean => {
                let b = *bytes.get(pos).ok_or_else(corrupt)?;
                row.push(Value::Boolean(b != 0));
                pos += 1;
            }
            DataType::Text => {
                let lb = bytes.get(pos..pos + 4).ok_or_else(corrupt)?;
                let len = u32::from_le_bytes(lb.try_into().unwrap()) as usize;
                pos += 4;
                let sb = bytes.get(pos..pos + len).ok_or_else(corrupt)?;
                row.push(Value::Text(
                    String::from_utf8(sb.to_vec()).map_err(|_| corrupt())?,
                ));
                pos += len;
            }
        }
    }
    Ok(row)
}

/// Order-preserving key bytes for a primary-key value.
pub fn value_to_key(v: &Value) -> Result<Vec<u8>, EngineError> {
    match v {
        Value::Integer(x) => Ok(storage::encoding::encode_i64(*x).to_vec()),
        Value::Text(s) => Ok(s.as_bytes().to_vec()),
        Value::Boolean(b) => Ok(vec![if *b { 1 } else { 0 }]),
        Value::Real(_) => Err(EngineError::Unsupported("REAL primary key".into())),
        Value::Null => Err(EngineError::Constraint("primary key cannot be NULL".into())),
    }
}

/// Order-preserving key for a hidden auto-increment row id (unsigned big-endian).
pub fn rowid_key(id: u64) -> Vec<u8> {
    id.to_be_bytes().to_vec()
}
