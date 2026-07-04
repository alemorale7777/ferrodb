//! A PostgreSQL-wire-protocol front end for ferrodb.
//!
//! Blocking IO, one thread per connection, a shared [`engine::Database`] behind
//! a mutex. Speaks the simple query protocol — enough for `psql` and Postgres
//! drivers to connect and run SQL. See `protocol` for the byte framing.

pub mod protocol;

use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use engine::{Database, Output, TxnId};

use protocol::{
    auth_ok, backend_key_data, command_complete, data_row, empty_query_response, error_response,
    infer_types, parameter_status, read_message, read_startup_packet, ready_for_query,
    row_description, Startup,
};

/// A shared database handle. The mutex serialises engine access; MVCC still
/// gives each statement a correct snapshot.
pub type SharedDb = Arc<Mutex<Database>>;

/// Per-connection session state.
struct Session {
    /// The open transaction, if the client issued `BEGIN`.
    txn: Option<TxnId>,
}

/// Accept connections forever, serving each on its own thread.
pub fn serve(listener: TcpListener, db: SharedDb) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, db) {
                if e.kind() != io::ErrorKind::UnexpectedEof {
                    eprintln!("connection error: {e}");
                }
            }
        });
    }
    Ok(())
}

/// Run one client connection start-to-finish.
pub fn handle_connection(mut stream: TcpStream, db: SharedDb) -> io::Result<()> {
    // Startup: reject SSL/GSS, then accept the StartupMessage.
    loop {
        match read_startup_packet(&mut stream)? {
            Startup::Ssl | Startup::GssEnc => stream.write_all(b"N")?,
            Startup::Start => break,
            Startup::Unsupported(code) => {
                stream.write_all(&error_response(&format!(
                    "unsupported protocol or request code {code}"
                )))?;
                return Ok(());
            }
        }
    }

    // Handshake.
    stream.write_all(&auth_ok())?;
    stream.write_all(&parameter_status("server_version", "ferrodb-0.1"))?;
    stream.write_all(&parameter_status("client_encoding", "UTF8"))?;
    stream.write_all(&parameter_status("DateStyle", "ISO, MDY"))?;
    stream.write_all(&backend_key_data())?;

    let mut session = Session { txn: None };
    stream.write_all(&ready_for_query(false))?;
    stream.flush()?;

    // Message loop.
    while let Some(msg) = read_message(&mut stream)? {
        match msg.tag {
            b'Q' => handle_query(&mut stream, &db, &mut session, &msg.body)?,
            b'X' => break,
            // Extended-protocol / unknown messages: not supported.
            other => {
                stream.write_all(&error_response(&format!(
                    "unsupported message type '{}'",
                    other as char
                )))?;
                stream.write_all(&ready_for_query(session.txn.is_some()))?;
                stream.flush()?;
            }
        }
    }

    // Roll back any transaction left open at disconnect.
    if let Some(t) = session.txn.take() {
        let _ = db.lock().unwrap().rollback_txn(t);
    }
    Ok(())
}

/// Handle one simple `Query` message (possibly several `;`-separated statements).
fn handle_query(
    stream: &mut TcpStream,
    db: &SharedDb,
    session: &mut Session,
    body: &[u8],
) -> io::Result<()> {
    let sql = cstr_to_str(body);
    let statements: Vec<&str> = sql
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if statements.is_empty() {
        stream.write_all(&empty_query_response())?;
        stream.write_all(&ready_for_query(session.txn.is_some()))?;
        stream.flush()?;
        return Ok(());
    }

    for stmt in statements {
        if !run_statement(stream, db, session, stmt)? {
            break; // an error ends the batch
        }
    }

    stream.write_all(&ready_for_query(session.txn.is_some()))?;
    stream.flush()?;
    Ok(())
}

/// Run one statement; returns `false` if it errored (which ends the batch).
fn run_statement(
    stream: &mut TcpStream,
    db: &SharedDb,
    session: &mut Session,
    sql: &str,
) -> io::Result<bool> {
    // Transaction control is handled here (the parser doesn't cover it).
    match sql.trim().to_ascii_uppercase().as_str() {
        "BEGIN" | "BEGIN TRANSACTION" | "START TRANSACTION" => {
            if session.txn.is_none() {
                session.txn = Some(db.lock().unwrap().begin());
            }
            stream.write_all(&command_complete("BEGIN"))?;
            return Ok(true);
        }
        "COMMIT" | "COMMIT TRANSACTION" | "END" => {
            if let Some(t) = session.txn.take() {
                if let Err(e) = db.lock().unwrap().commit_txn(t) {
                    stream.write_all(&error_response(&e.to_string()))?;
                    return Ok(false);
                }
            }
            stream.write_all(&command_complete("COMMIT"))?;
            return Ok(true);
        }
        "ROLLBACK" | "ABORT" => {
            if let Some(t) = session.txn.take() {
                let _ = db.lock().unwrap().rollback_txn(t);
            }
            stream.write_all(&command_complete("ROLLBACK"))?;
            return Ok(true);
        }
        _ => {}
    }

    let result = {
        let mut g = db.lock().unwrap();
        match session.txn {
            Some(t) => g.execute_in(t, sql),
            None => g.execute(sql),
        }
    };

    match result {
        Ok(out) => {
            write_output(stream, sql, &out)?;
            Ok(true)
        }
        Err(e) => {
            stream.write_all(&error_response(&e.to_string()))?;
            Ok(false)
        }
    }
}

fn write_output(stream: &mut TcpStream, sql: &str, out: &Output) -> io::Result<()> {
    match out {
        Output::Rows { columns, rows } => {
            let types = infer_types(columns, rows);
            stream.write_all(&row_description(columns, &types))?;
            for row in rows {
                stream.write_all(&data_row(row))?;
            }
            stream.write_all(&command_complete(&format!("SELECT {}", rows.len())))?;
        }
        Output::Affected(n) => {
            stream.write_all(&command_complete(&command_tag(sql, *n)))?;
        }
        Output::Ack(msg) => {
            stream.write_all(&command_complete(msg))?;
        }
    }
    Ok(())
}

/// The Postgres command tag for a row-count result, keyed off the leading verb.
fn command_tag(sql: &str, n: usize) -> String {
    let verb = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match verb.as_str() {
        "INSERT" => format!("INSERT 0 {n}"),
        "UPDATE" => format!("UPDATE {n}"),
        "DELETE" => format!("DELETE {n}"),
        _ => format!("OK {n}"),
    }
}

/// Interpret a `NUL`-terminated protocol string.
fn cstr_to_str(body: &[u8]) -> String {
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    String::from_utf8_lossy(&body[..end]).into_owned()
}
