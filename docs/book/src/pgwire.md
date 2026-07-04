# PostgreSQL wire protocol

A database that only its own CLI can talk to is a toy. M6 makes ferrodb speak enough of the
**PostgreSQL v3 wire protocol** that the real `psql` client — or any Postgres driver — can connect
over TCP and run SQL. It is implemented **by hand: no async runtime, no protocol crate.**

## Why the Postgres protocol

Implementing an existing, documented protocol means ferrodb inherits a whole ecosystem of clients
for free. The v3 protocol is also refreshingly simple to speak at the "simple query" level: it is a
sequence of length-prefixed, type-tagged messages over a plain socket.

## The startup handshake

Every Postgres client opens with an **SSL request** before anything else; ferrodb replies with a
single byte declining TLS, and the client proceeds in cleartext. The client then sends a **startup
packet** with the user and database name, and the server replies `AuthenticationOk`, a few
`ParameterStatus` messages, and `ReadyForQuery`. Now the connection is live.

## The simple query cycle

For each `Query` message (a SQL string), the server runs the statement and replies with:

- **`RowDescription`** — the result columns and their types, for a `SELECT`.
- **`DataRow`** — one message per row, each field encoded per its type.
- **`CommandComplete`** — a tag like `SELECT 3` or `INSERT 0 1`.
- **`ReadyForQuery`** — the server is ready for the next query.

`BEGIN` / `COMMIT` / `ROLLBACK` map straight onto the engine's MVCC transactions (Chapter 5). Any
engine error becomes an **`ErrorResponse`** message, which `psql` prints as a normal SQL error.

## Concurrency model

The server is deliberately simple: **blocking IO, one thread per connection**, with the database
behind a shared `Arc<Mutex<Database>>`. This is not how a high-throughput server would be built, but
it is correct and easy to reason about, and it is enough to serve concurrent `psql` sessions. (The
`Blob` trait carrying a `Send` bound — see Chapter 2 — is what lets the `Database` cross thread
boundaries safely.)

## Verifying the framing

The byte-level framing is validated by in-process **wire tests** that drive the protocol over a
loopback socket — open a connection, perform the handshake, send a query, and assert the exact
sequence of reply messages — so the protocol is tested without depending on an external `psql`
binary.
