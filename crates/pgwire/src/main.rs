//! `ferrodb-pg` — serve ferrodb over the PostgreSQL wire protocol.
//!
//! Usage: `ferrodb-pg [db-path] [--port N]` (defaults: `ferrodb.db`, port 5432).
//! Connect with any Postgres client, e.g. `psql -h 127.0.0.1 -p 5432`.

use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use engine::Database;
use pgwire::serve;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut path = "ferrodb.db".to_string();
    let mut port: u16 = 5432;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" | "-p" => {
                port = args
                    .next()
                    .ok_or("--port requires a value")?
                    .parse()
                    .map_err(|_| "invalid port")?;
            }
            other => path = other.to_string(),
        }
    }

    let db = Arc::new(Mutex::new(Database::open(&path)?));
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    println!("ferrodb-pg listening on 127.0.0.1:{port} (database: {path})");
    println!("connect with:  psql -h 127.0.0.1 -p {port}");
    serve(listener, db)?;
    Ok(())
}
