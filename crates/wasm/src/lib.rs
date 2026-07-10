//! WebAssembly bindings for the ferrodb engine.
//!
//! No `wasm-bindgen` — just a hand-written C ABI over `wasm32-unknown-unknown`.
//! The engine runs on an in-memory database (no filesystem), and JSON crosses
//! the boundary as length-prefixed byte buffers the JS glue reads directly from
//! wasm memory. Build with `cargo build -p ferrodb-wasm --target wasm32-unknown-unknown`.

use engine::{Database, Output, SqlValue as Value};

// ---- result serialization (pure, unit-tested) -----------------------------

/// Render an engine result as JSON for the playground:
/// `{"columns":[...],"rows":[[...]]}` for a result set, `{"message":"…"}`
/// for a write/DDL, or `{"error":"…"}` on failure.
pub fn exec_json(db: &mut Database, sql: &str) -> String {
    match db.execute(sql) {
        Ok(out) => output_to_json(&out),
        Err(e) => format!("{{\"error\":{}}}", json_string(&e.to_string())),
    }
}

/// Render a table's B+-tree, or an error, as JSON.
pub fn tree_json(db: &mut Database, table: &str) -> String {
    match db.table_tree_json(table) {
        Ok(json) => json,
        Err(e) => format!("{{\"error\":{}}}", json_string(&e.to_string())),
    }
}

fn output_to_json(out: &Output) -> String {
    match out {
        Output::Rows { columns, rows } => {
            let cols: Vec<String> = columns.iter().map(|c| json_string(c)).collect();
            let rs: Vec<String> = rows
                .iter()
                .map(|row| {
                    let cells: Vec<String> = row.iter().map(value_to_json).collect();
                    format!("[{}]", cells.join(","))
                })
                .collect();
            format!(
                "{{\"columns\":[{}],\"rows\":[{}]}}",
                cols.join(","),
                rs.join(",")
            )
        }
        Output::Affected(n) => format!(
            "{{\"message\":\"{n} row{} affected\"}}",
            if *n == 1 { "" } else { "s" }
        ),
        Output::Ack(msg) => format!("{{\"message\":{}}}", json_string(msg)),
    }
}

fn value_to_json(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Vector(v) => {
            let inner: Vec<String> = v.iter().map(|x| x.to_string()).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Integer(x) => x.to_string(),
        Value::Real(x) => {
            if x.is_finite() {
                x.to_string()
            } else {
                "null".to_string()
            }
        }
        Value::Boolean(b) => b.to_string(),
        Value::Text(s) => json_string(s),
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- the C ABI (called from JavaScript) -----------------------------------

/// Allocate `len` bytes in wasm memory; JS writes input here. Paired with [`dealloc`].
#[no_mangle]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Free a buffer previously returned by [`alloc`].
///
/// # Safety
/// `ptr`/`len` must come from a matching [`alloc`] call and not be reused after.
#[no_mangle]
pub unsafe extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        let _ = Vec::from_raw_parts(ptr, 0, len);
    }
}

/// Open a fresh in-memory database, returning an opaque handle (or null).
#[no_mangle]
pub extern "C" fn db_new() -> *mut Database {
    match Database::open_in_memory() {
        Ok(db) => Box::into_raw(Box::new(db)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Close a database handle from [`db_new`].
///
/// # Safety
/// `db` must be a handle from [`db_new`] and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn db_free(db: *mut Database) {
    if !db.is_null() {
        drop(Box::from_raw(db));
    }
}

/// Run SQL and return a length-prefixed JSON string (see [`into_wasm_string`]).
///
/// # Safety
/// `db` must be a valid handle; `sql_ptr`/`sql_len` must describe a valid buffer.
#[no_mangle]
pub unsafe extern "C" fn db_exec(db: *mut Database, sql_ptr: *const u8, sql_len: usize) -> *mut u8 {
    let db = &mut *db;
    let sql = String::from_utf8_lossy(std::slice::from_raw_parts(sql_ptr, sql_len)).into_owned();
    into_wasm_string(exec_json(db, &sql))
}

/// Return a table's B+-tree as a length-prefixed JSON string.
///
/// # Safety
/// `db` must be a valid handle; `name_ptr`/`name_len` must describe a valid buffer.
#[no_mangle]
pub unsafe extern "C" fn db_tree(
    db: *mut Database,
    name_ptr: *const u8,
    name_len: usize,
) -> *mut u8 {
    let db = &mut *db;
    let name = String::from_utf8_lossy(std::slice::from_raw_parts(name_ptr, name_len)).into_owned();
    into_wasm_string(tree_json(db, &name))
}

/// Free a string buffer returned by [`db_exec`] / [`db_tree`].
///
/// # Safety
/// `ptr` must be a value returned by [`into_wasm_string`] and freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn free_string(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let len = u32::from_le_bytes([*ptr, *ptr.add(1), *ptr.add(2), *ptr.add(3)]) as usize;
    let _ = Vec::from_raw_parts(ptr, 0, 4 + len);
}

/// Pack a string as `[u32 little-endian length][UTF-8 bytes]` and leak it; the
/// caller (JS) reads the length, then the bytes, then calls [`free_string`].
fn into_wasm_string(s: String) -> *mut u8 {
    let bytes = s.into_bytes();
    let mut buf = Vec::with_capacity(4 + bytes.len());
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&bytes);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_json_shapes() {
        let mut db = Database::open_in_memory().unwrap();
        assert!(exec_json(
            &mut db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)"
        )
        .contains("\"message\""));
        assert_eq!(
            exec_json(&mut db, "INSERT INTO t VALUES (1, 'al')"),
            "{\"message\":\"1 row affected\"}"
        );
        let rows = exec_json(&mut db, "SELECT id, name FROM t");
        assert_eq!(
            rows,
            "{\"columns\":[\"id\",\"name\"],\"rows\":[[1,\"al\"]]}"
        );
        assert!(exec_json(&mut db, "SELECT * FROM nope").contains("\"error\""));
    }

    #[test]
    fn tree_json_is_returned() {
        let mut db = Database::open_in_memory().unwrap();
        exec_json(
            &mut db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
        );
        exec_json(&mut db, "INSERT INTO t VALUES (1, 10), (2, 20)");
        let tree = tree_json(&mut db, "t");
        assert!(tree.contains("\"leaf\":true"));
        assert!(tree.contains("\"1\""));
    }
}
