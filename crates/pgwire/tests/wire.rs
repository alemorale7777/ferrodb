//! In-process PostgreSQL-wire tests: a real TCP client drives the byte protocol
//! against a background `handle_connection`, no external `psql` required.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use engine::Database;

fn spawn_server() -> TcpStream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wire.db");
    std::mem::forget(dir);
    let db = Arc::new(Mutex::new(Database::open(path).unwrap()));

    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        pgwire::handle_connection(stream, db).unwrap();
    });

    let mut client = TcpStream::connect(addr).unwrap();
    send_startup(&mut client);
    read_until_ready(&mut client); // consume the handshake
    client
}

fn send_startup(s: &mut TcpStream) {
    let params = b"user\0ferrodb\0\0";
    let len = (4 + 4 + params.len()) as i32;
    let mut msg = Vec::new();
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(&196608i32.to_be_bytes());
    msg.extend_from_slice(params);
    s.write_all(&msg).unwrap();
    s.flush().unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    let mut msg = vec![b'Q'];
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(&body);
    s.write_all(&msg).unwrap();
    s.flush().unwrap();
}

fn read_message(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut tag = [0u8; 1];
    s.read_exact(&mut tag).unwrap();
    let mut lb = [0u8; 4];
    s.read_exact(&mut lb).unwrap();
    let len = i32::from_be_bytes(lb) as usize;
    let mut body = vec![0u8; len - 4];
    s.read_exact(&mut body).unwrap();
    (tag[0], body)
}

/// Read messages up to and including `ReadyForQuery`; return them plus the
/// transaction-status byte (`I` idle / `T` in-txn).
fn read_until_ready(s: &mut TcpStream) -> (Vec<(u8, Vec<u8>)>, u8) {
    let mut out = Vec::new();
    loop {
        let (tag, body) = read_message(s);
        if tag == b'Z' {
            let status = body[0];
            return (out, status);
        }
        out.push((tag, body));
    }
}

fn parse_row_description(body: &[u8]) -> Vec<String> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut names = Vec::with_capacity(n);
    for _ in 0..n {
        let end = body[pos..].iter().position(|&b| b == 0).unwrap() + pos;
        names.push(String::from_utf8_lossy(&body[pos..end]).into_owned());
        pos = end + 1 + 18; // NUL + 6 fixed fields (4+2+4+2+4+2)
    }
    names
}

fn parse_data_row(body: &[u8]) -> Vec<Option<String>> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut vals = Vec::with_capacity(n);
    for _ in 0..n {
        let len = i32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;
        if len < 0 {
            vals.push(None);
        } else {
            let len = len as usize;
            vals.push(Some(
                String::from_utf8_lossy(&body[pos..pos + len]).into_owned(),
            ));
            pos += len;
        }
    }
    vals
}

fn command_tag(msgs: &[(u8, Vec<u8>)]) -> String {
    let (_, body) = msgs
        .iter()
        .find(|(t, _)| *t == b'C')
        .expect("CommandComplete");
    let end = body.iter().position(|&b| b == 0).unwrap();
    String::from_utf8_lossy(&body[..end]).into_owned()
}

#[test]
fn create_insert_select_round_trip() {
    let mut c = spawn_server();

    send_query(&mut c, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)");
    let (msgs, status) = read_until_ready(&mut c);
    assert_eq!(command_tag(&msgs), "CREATE TABLE");
    assert_eq!(status, b'I');

    send_query(&mut c, "INSERT INTO t VALUES (1, 'al'), (2, 'sam')");
    let (msgs, _) = read_until_ready(&mut c);
    assert_eq!(command_tag(&msgs), "INSERT 0 2");

    send_query(&mut c, "SELECT id, name FROM t ORDER BY id");
    let (msgs, _) = read_until_ready(&mut c);

    let (_, desc) = msgs
        .iter()
        .find(|(t, _)| *t == b'T')
        .expect("RowDescription");
    assert_eq!(parse_row_description(desc), vec!["id", "name"]);

    let data: Vec<Vec<Option<String>>> = msgs
        .iter()
        .filter(|(t, _)| *t == b'D')
        .map(|(_, b)| parse_data_row(b))
        .collect();
    assert_eq!(
        data,
        vec![
            vec![Some("1".into()), Some("al".into())],
            vec![Some("2".into()), Some("sam".into())],
        ]
    );
    assert_eq!(command_tag(&msgs), "SELECT 2");
}

#[test]
fn transaction_rollback_over_the_wire() {
    let mut c = spawn_server();

    send_query(&mut c, "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)");
    read_until_ready(&mut c);
    send_query(&mut c, "INSERT INTO t VALUES (1, 10)");
    read_until_ready(&mut c);

    send_query(&mut c, "BEGIN");
    let (msgs, status) = read_until_ready(&mut c);
    assert_eq!(command_tag(&msgs), "BEGIN");
    assert_eq!(status, b'T', "ReadyForQuery should report in-transaction");

    send_query(&mut c, "INSERT INTO t VALUES (2, 20)");
    read_until_ready(&mut c);
    send_query(&mut c, "ROLLBACK");
    let (_, status) = read_until_ready(&mut c);
    assert_eq!(status, b'I');

    // the rolled-back insert is gone
    send_query(&mut c, "SELECT v FROM t");
    let (msgs, _) = read_until_ready(&mut c);
    let data: Vec<_> = msgs.iter().filter(|(t, _)| *t == b'D').collect();
    assert_eq!(data.len(), 1, "only the committed row should remain");
}

#[test]
fn error_response_for_bad_sql() {
    let mut c = spawn_server();
    send_query(&mut c, "SELECT * FROM nonexistent");
    let (msgs, _) = read_until_ready(&mut c);
    assert!(
        msgs.iter().any(|(t, _)| *t == b'E'),
        "expected an ErrorResponse"
    );
}
