# ferrodb Milestone 6 — PostgreSQL Wire Protocol (Design Spec)

> **Status:** Draft for review · **Date:** 2026-07-03 · Builds on M1–M5.

## 1. Goal

Speak enough of the **PostgreSQL v3 frontend/backend protocol** that a real client — `psql`,
`pgcli`, a Postgres driver — can connect to ferrodb over TCP, run SQL, and see results. This is
the milestone that makes ferrodb feel *legitimate*: you point the standard tooling at it.

Scope: the **simple query protocol** (the path `psql` uses for interactive statements), the startup
handshake (including the SSL-request rejection every client sends first), transaction control over
the wire, and clean error reporting. The extended/prepared-statement protocol (Parse/Bind/Execute)
is out of scope for M6.

## 2. Why a new crate, no async framework

A new `crates/pgwire` crate depends on `engine` and owns nothing but the protocol. It uses the
**std blocking-IO + one-thread-per-connection** model — no `tokio`, no `postgres-protocol` crate.
The point of the project is to build the machine: byte-for-byte message framing by hand. A shared
`Arc<Mutex<Database>>` serialises access to the single-file engine; MVCC still gives each statement
a correct snapshot, and each connection keeps its own session transaction.

## 3. Startup handshake

1. The client's first packet is length-prefixed with **no type byte**. It is one of:
   - **SSLRequest** (code `80877103`) or **GSSENCRequest** (`80877104`) — reply with a single byte
     `N` ("no, continue unencrypted") and read the next packet.
   - **StartupMessage** (protocol `196608` = 3.0) — a set of `key\0value\0…\0` parameters
     (`user`, `database`, …). We accept any user; there is no auth.
2. Reply, in order: **AuthenticationOk** (`R` + 0), a few **ParameterStatus** (`S`) messages
   (`server_version`, `client_encoding=UTF8`, `DateStyle`), **BackendKeyData** (`K`), and
   **ReadyForQuery** (`Z` + status).

`ReadyForQuery`'s status byte is `I` (idle), `T` (in a transaction), or `E` (failed transaction).

## 4. Simple query cycle

The client sends **Query** (`Q`) with a `NUL`-terminated SQL string that may contain several
`;`-separated statements. For each statement, in order, the backend emits:

- **SELECT / EXPLAIN** → **RowDescription** (`T`: one field descriptor per column — name, type OID,
  size, text format), one **DataRow** (`D`) per row (each value in **text format**, `NULL` as
  length −1), then **CommandComplete** (`C`) tagged `SELECT <n>`.
- **INSERT / UPDATE / DELETE** → **CommandComplete** tagged `INSERT 0 <n>` / `UPDATE <n>` /
  `DELETE <n>`.
- **CREATE / DROP** → **CommandComplete** with the ack (`CREATE TABLE`, …).
- An **error** → **ErrorResponse** (`E`, fields `S`everity / `C`ode / `M`essage), which ends the
  batch.

After the batch, one **ReadyForQuery**. An empty query string yields **EmptyQueryResponse** (`I`).

Type OIDs are inferred from the first non-null value in each column: `INTEGER`→int8 (20),
`REAL`→float8 (701), `TEXT`→text (25), `BOOLEAN`→bool (16); an all-null column defaults to text.

## 5. Transactions over the wire

`BEGIN` / `COMMIT` / `ROLLBACK` (and the aliases `START TRANSACTION` / `END` / `ABORT`) are
recognised by the server and mapped to `Database::begin` / `commit_txn` / `rollback_txn`, so a
session holds one open transaction across queries. Any other statement runs inside the open
transaction, or autocommits when none is open. On disconnect an open transaction is rolled back.

## 6. Message framing

Every post-startup message is `Int8 type · Int32 length (self-inclusive, excludes the type byte) ·
body`; all integers are big-endian. A small set of hand-written reader/writer helpers
(`read_i32`, `read_message`, `frame(tag, body)`) is the whole surface.

## 7. Files

- `crates/pgwire/src/protocol.rs` — message constants, framing, response builders (RowDescription,
  DataRow, CommandComplete, ErrorResponse, ReadyForQuery), value/type encoding.
- `crates/pgwire/src/lib.rs` — `serve(listener, db)` accept loop + `handle_connection(stream, db)`
  (handshake, query loop, session transaction state).
- `crates/pgwire/src/main.rs` — `ferrodb-pg` binary: parse `[db-path] [--port N]`, bind, serve.

## 8. Testing

An **in-process protocol test** (`tests/wire.rs`): bind the server to `127.0.0.1:0`, serve on a
background thread, connect a plain `TcpStream`, drive the real byte protocol — startup → handshake →
`CREATE` / `INSERT` / `SELECT` — and assert the exact `RowDescription` fields, `DataRow` values, and
`CommandComplete` tags come back. A second test drives `BEGIN` … `ROLLBACK` and checks the change is
discarded. No external client or network fixture required (mirrors the M3 in-process crash tests).

## 9. Success criteria

- [ ] A real `psql` can connect, run `CREATE`/`INSERT`/`SELECT`/joins/aggregates, and see results.
- [ ] Startup (incl. SSL-request rejection), simple query, transactions, and errors are handled.
- [ ] In-process wire tests pass; `cargo test --workspace` green; fmt + clippy clean; CI green.
