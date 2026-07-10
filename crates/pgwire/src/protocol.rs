//! PostgreSQL v3 wire-protocol message framing and builders.
//!
//! Every post-startup message is `Int8 type · Int32 length · body`, where the
//! length is self-inclusive but excludes the type byte, and every integer is
//! big-endian. These helpers read and build those bytes by hand.

use std::io::{self, Read};

use engine::SqlValue as Value;

/// Protocol version 3.0, sent in the `StartupMessage`.
pub const PROTOCOL_V3: i32 = 196608;
/// `SSLRequest` sentinel (a client's very first packet).
pub const SSL_REQUEST: i32 = 80877103;
/// `GSSENCRequest` sentinel.
pub const GSSENC_REQUEST: i32 = 80877104;

// Postgres type OIDs used in `RowDescription`.
const OID_INT8: i32 = 20;
const OID_FLOAT8: i32 = 701;
const OID_TEXT: i32 = 25;
const OID_BOOL: i32 = 16;

/// A resolved column type for `RowDescription`.
#[derive(Clone, Copy)]
pub struct PgType {
    pub oid: i32,
    pub size: i16,
}

fn pg_type(v: &Value) -> PgType {
    match v {
        Value::Integer(_) => PgType {
            oid: OID_INT8,
            size: 8,
        },
        Value::Real(_) => PgType {
            oid: OID_FLOAT8,
            size: 8,
        },
        Value::Boolean(_) => PgType {
            oid: OID_BOOL,
            size: 1,
        },
        // Vectors render as their pgvector-style text form '[a, b, ...]'.
        Value::Text(_) | Value::Vector(_) | Value::Null => PgType {
            oid: OID_TEXT,
            size: -1,
        },
    }
}

/// Infer a column type per output column from the first non-null value it holds.
pub fn infer_types(columns: &[String], rows: &[Vec<Value>]) -> Vec<PgType> {
    (0..columns.len())
        .map(|i| {
            rows.iter()
                .map(|r| &r[i])
                .find(|v| !matches!(v, Value::Null))
                .map(pg_type)
                .unwrap_or(PgType {
                    oid: OID_TEXT,
                    size: -1,
                })
        })
        .collect()
}

/// The text-format encoding of a value; `None` means SQL `NULL`.
pub fn encode_value(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Integer(x) => Some(x.to_string().into_bytes()),
        Value::Real(x) => Some(x.to_string().into_bytes()),
        Value::Boolean(b) => Some(if *b { b"t".to_vec() } else { b"f".to_vec() }),
        Value::Text(s) => Some(s.clone().into_bytes()),
        Value::Vector(v) => {
            let inner: Vec<String> = v.iter().map(|x| x.to_string()).collect();
            Some(format!("[{}]", inner.join(", ")).into_bytes())
        }
    }
}

// ---- reading --------------------------------------------------------------

pub fn read_i32(r: &mut impl Read) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}

fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

/// A raw post-startup message: a type byte and its body.
pub struct Message {
    pub tag: u8,
    pub body: Vec<u8>,
}

/// Read one framed message, or `None` at a clean end-of-stream.
pub fn read_message(r: &mut impl Read) -> io::Result<Option<Message>> {
    let tag = match read_u8(r) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = read_i32(r)?;
    let body_len = (len as usize).saturating_sub(4);
    let mut body = vec![0u8; body_len];
    r.read_exact(&mut body)?;
    Ok(Some(Message { tag, body }))
}

/// The result of the startup handshake read.
pub enum Startup {
    Ssl,
    GssEnc,
    Start,
    Unsupported(i32),
}

/// Read one startup-phase packet (no type byte).
pub fn read_startup_packet(r: &mut impl Read) -> io::Result<Startup> {
    let len = read_i32(r)?;
    let code = read_i32(r)?;
    let remaining = (len as usize).saturating_sub(8);
    let mut body = vec![0u8; remaining];
    r.read_exact(&mut body)?;
    Ok(match code {
        SSL_REQUEST => Startup::Ssl,
        GSSENC_REQUEST => Startup::GssEnc,
        PROTOCOL_V3 => Startup::Start,
        other => Startup::Unsupported(other),
    })
}

// ---- writing --------------------------------------------------------------

/// Frame a message: `tag · Int32(len) · body`.
pub fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(5 + body.len());
    m.push(tag);
    m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    m.extend_from_slice(body);
    m
}

fn cstr(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

/// `AuthenticationOk`.
pub fn auth_ok() -> Vec<u8> {
    frame(b'R', &0i32.to_be_bytes())
}

/// A `ParameterStatus` message.
pub fn parameter_status(key: &str, value: &str) -> Vec<u8> {
    let mut b = Vec::new();
    cstr(&mut b, key);
    cstr(&mut b, value);
    frame(b'S', &b)
}

/// `BackendKeyData` (fixed dummy pid/secret — we never honour CancelRequest).
pub fn backend_key_data() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&1234i32.to_be_bytes());
    b.extend_from_slice(&5678i32.to_be_bytes());
    frame(b'K', &b)
}

/// `ReadyForQuery` with a transaction-status byte (`I` idle / `T` in-txn).
pub fn ready_for_query(in_txn: bool) -> Vec<u8> {
    frame(b'Z', &[if in_txn { b'T' } else { b'I' }])
}

/// `RowDescription` for a result set.
pub fn row_description(columns: &[String], types: &[PgType]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(columns.len() as i16).to_be_bytes());
    for (name, ty) in columns.iter().zip(types) {
        cstr(&mut b, name);
        b.extend_from_slice(&0i32.to_be_bytes()); // table OID
        b.extend_from_slice(&0i16.to_be_bytes()); // column attribute number
        b.extend_from_slice(&ty.oid.to_be_bytes());
        b.extend_from_slice(&ty.size.to_be_bytes());
        b.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        b.extend_from_slice(&0i16.to_be_bytes()); // format code: text
    }
    frame(b'T', &b)
}

/// A `DataRow` (all values in text format).
pub fn data_row(row: &[Value]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(row.len() as i16).to_be_bytes());
    for v in row {
        match encode_value(v) {
            None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                b.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                b.extend_from_slice(&bytes);
            }
        }
    }
    frame(b'D', &b)
}

/// `CommandComplete` with a command tag (e.g. `SELECT 3`).
pub fn command_complete(tag: &str) -> Vec<u8> {
    let mut b = Vec::new();
    cstr(&mut b, tag);
    frame(b'C', &b)
}

/// `EmptyQueryResponse`.
pub fn empty_query_response() -> Vec<u8> {
    frame(b'I', &[])
}

/// `ErrorResponse` with severity/code/message fields.
pub fn error_response(message: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(b'S');
    cstr(&mut b, "ERROR");
    b.push(b'C');
    cstr(&mut b, "XX000");
    b.push(b'M');
    cstr(&mut b, message);
    b.push(0);
    frame(b'E', &b)
}
