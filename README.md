# ferrodb

**A relational database written from scratch in Rust** — page-based storage, B+-tree
indexes, write-ahead logging with crash recovery, MVCC transactions, a cost-based query
planner, and the PostgreSQL wire protocol. No third-party SQL parser, storage engine, or
B+-tree crate: the point is to build the machine, not glue one together.

> **Status:** All 8 milestones complete and green — it runs real SQL with joins and
> grouped aggregation, persisted to disk, survives a crash, gives concurrent transactions
> snapshot isolation, plans queries with a cost-based optimizer, speaks the
> **PostgreSQL wire protocol** so `psql` can connect, compiles to **WebAssembly** to run
> in the browser, and ships a **SQLite-comparison benchmark** and an **mdBook architecture
> book**. See the
> [full design & roadmap](docs/superpowers/specs/2026-07-02-ferrodb-design.md).

## Milestone 8 — Benchmarks & architecture book ✅

A benchmark harness runs ferrodb head-to-head against **bundled SQLite** on five in-memory workloads,
driving both engines with the same SQL strings (no statement caching) so the numbers cover the whole
parse → plan → execute path:

```console
$ cargo run -p ferrodb-bench --release --features sqlite
workload                        ferrodb       sqlite      ratio
--------------------------------------------------------------
bulk insert (20000)             133.1ms        7.0ms     18.97x
point lookup (20000)            132.3ms       30.3ms      4.36x
range scan (2000×200)           158.8ms       10.9ms     14.64x
aggregate scan (50×)            397.5ms       48.2ms      8.25x
hash join (10×)                 247.8ms        4.4ms     56.07x
```

The **index-driven** workloads — the whole point of the M5 optimizer — land within a small constant
factor of a mature C database; the **full-scan and join** workloads honestly expose the cost of a
row-at-a-time interpreter that materializes each operator's output (a streaming executor is the
obvious next lever). SQLite sits behind an optional `sqlite` feature so default CI stays
dependency-light.

Running the harness at scale **caught two real optimizer bugs** the correctness tests had passed
over: cardinality estimation was scanning the whole table on every query (a point lookup was secretly
O(n)) — fixed with a persisted `row_count` catalog statistic, ~700× faster point lookups — and PK
range predicates fell back to full scans — fixed with a B+-tree `IndexRange` access path, ~90× faster
range scans. The [architecture book](docs/book/) (`mdbook build docs/book`) walks the whole engine
bottom-up, one chapter per subsystem, closing with these benchmarks.

## Milestone 7 — WebAssembly playground & B+-tree visualizer ✅

The whole engine — storage, SQL, MVCC, the optimizer — compiles to **WebAssembly** and runs
entirely in the browser, with a **live B+-tree visualizer** that shows the tree split as you insert
rows:

```console
$ cd web && ./build.sh && python -m http.server 8000   # then open localhost:8000
```

`crates/wasm` is a `cdylib` over `wasm32-unknown-unknown` with a **hand-written C ABI — no
`wasm-bindgen`, no dependencies**; strings cross the boundary as length-prefixed buffers read
straight out of wasm memory. This is possible because the storage layer was decoupled from the
filesystem: `DiskManager` and the WAL now sit on a `Blob` trait (a real `File` natively, a `Vec<u8>`
`MemBlob` in the browser), so `Database::open_in_memory()` needs no files at all. The ~383 KB module
instantiates with **zero imports** and drives `CREATE`/`INSERT`/`SELECT`/`EXPLAIN` plus a B+-tree
export that `web/index.html` lays out as SVG — inserting rows animates real node splits.

## Milestone 6 — PostgreSQL wire protocol ✅

ferrodb speaks enough of the **PostgreSQL v3 protocol** that the real `psql` client — or any
Postgres driver — can connect over TCP and run SQL:

```console
$ cargo run -p pgwire --bin ferrodb-pg -- mydata.db --port 5432
ferrodb-pg listening on 127.0.0.1:5432 (database: mydata.db)

$ psql -h 127.0.0.1 -p 5432
ferrodb=> SELECT u.name, SUM(o.total) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.name;
```

The startup handshake (including the SSL-request rejection every client sends first), the simple
query cycle (`RowDescription` / `DataRow` / `CommandComplete`), transaction control
(`BEGIN`/`COMMIT`/`ROLLBACK` mapped to the engine's MVCC transactions), and `ErrorResponse` are all
implemented — **by hand, no async runtime and no protocol crate**. A shared `Arc<Mutex<Database>>`
serves each connection on its own thread. The v3 byte framing is validated by in-process wire tests
(`crates/pgwire/tests/wire.rs`) that drive the protocol over a loopback socket.

## Milestone 5 — Joins, aggregates & a cost-based optimizer ✅

A real query engine on a physical **plan tree**: `INNER` / `LEFT` joins, `GROUP BY` / `HAVING`,
`COUNT / SUM / AVG / MIN / MAX`, qualified columns and aliases. The **cost-based optimizer** pushes
single-table predicates down to scans, picks a **PK index seek** over a full scan when a `pk = const`
predicate is available, and orders joins (System-R-style DP over relation subsets) to minimise
estimated intermediate cardinality — so it never leads with the biggest table. Equijoins run as
**hash joins**. `EXPLAIN` prints the chosen plan:

```sql
EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = 1;
```
```
+----------------------------------------------------+
| QUERY PLAN                                         |
+----------------------------------------------------+
| Project [u.name AS name]  (rows≈1)                 |
|   HashJoin [Inner] on u.id = o.user_id  (rows≈1)   |
|     SeqScan orders o  (rows≈3)                     |
|     IndexSeek users u (pk = 1)  (rows≈1)           |
+----------------------------------------------------+
```

The optimizer recognised the `pk = 1` predicate as an index seek and made that one-row relation the
build side of the join. Proven by `crates/engine/tests/query.rs`.

## Milestone 4 — MVCC transactions ✅

`BEGIN` / `COMMIT` / `ROLLBACK` with **snapshot isolation**. Each row is a **version chain**:
`INSERT` appends a version, `UPDATE` is delete-old + append-new, `DELETE` stamps a tombstone —
nothing is overwritten in place. A transaction captures a snapshot at `BEGIN` and sees a
consistent view; **readers never block writers**. Two transactions writing the same row is a
first-updater-wins **write conflict**. Per-version commit **hint bits** are the persisted source
of truth, so a committed version is visible after restart with no separate commit log; a
crashed/rolled-back transaction is simply invisible — no undo pass. `VACUUM` reclaims versions
dead to every live snapshot. Proven by interleaved-transaction tests (`crates/engine/tests/mvcc.rs`).

## Milestone 3 — WAL + crash recovery ✅

Every statement is an autocommit transaction backed by a **write-ahead log**. The buffer pool
is **no-steal** (uncommitted pages never reach the data file), so recovery is redo-only: on
commit we log the modified pages' after-images and `fsync` the WAL *before* touching the data
file; on startup we replay any committed-but-unflushed transaction and discard incomplete ones.

The upshot: a crash **between the WAL commit and the data flush** — the exact window that
otherwise corrupts a multi-page B+-tree split — is repaired on restart, and a statement that
never committed leaves **no trace**. Both guarantees are proven by deterministic crash-injection
tests (`crates/engine/src/lib.rs` → `crash_tests`), not by racing an OS `kill`.

## Milestone 2 — SQL ✅

```sql
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
INSERT INTO users VALUES (1, 'alejandro', 30), (2, 'sam', 25), (3, 'kai', 40);
SELECT name, age FROM users WHERE age > 26 ORDER BY name;
```
```
+-----------+-----+
| name      | age |
+-----------+-----+
| alejandro | 30  |
| kai       | 40  |
+-----------+-----+
```

A hand-written **lexer + Pratt/recursive-descent parser** (no `sqlparser` crate) →
AST → binder → a **volcano-style executor** over the M1 B+-trees. Supports
`CREATE TABLE`/`DROP TABLE`, `INSERT`, `SELECT` (projection, `WHERE`, `ORDER BY`,
`LIMIT`/`OFFSET`), `UPDATE`, `DELETE`; four types with three-valued `NULL` logic;
per-table B+-trees keyed by primary key (or a hidden row id); a self-describing
catalog stored in-file. Run it with the `ferrodb` shell:

```console
$ cargo run -p ferrodb-cli --bin ferrodb -- mydata.db
sql> SELECT * FROM users ORDER BY age DESC LIMIT 2;
```

---

## Milestone 1 — Storage engine ✅

The foundation every database is built on, from the raw file up:

- **Disk manager** — a single file of fixed **4 KiB pages**, each with a **CRC32C** checksum
  verified on every read.
- **Buffer pool** — in-memory page cache with **clock-sweep eviction**, pin/unpin, and a
  WAL-safe dirty-flush path.
- **Slotted pages** — a slot directory + variable-length cells, the standard layout for
  storing rows and index entries inside a page.
- **B+-tree** — an ordered map over the pager: point lookup, **insert with leaf & internal
  node splits and root growth**, ordered **range scans** over a leaf sibling chain, delete,
  and **overflow page chains** for large values.
- **Durability** — a meta page (page 0) checkpoints the tree root, so data survives reopen.

Correctness is proven, not asserted: a **property test** runs hundreds of randomized
insert/delete sequences and asserts the tree matches a `BTreeMap` model exactly, alongside a
2,000-key split-stress test and an overflow (20 KB value) round-trip.

### Try it

```console
$ cargo run -p ferrodb-cli -- mydata.db
kv> put 42 hello
ok
kv> put 7 world
ok
kv> .checkpoint
ok
kv> .exit

$ cargo run -p ferrodb-cli -- mydata.db      # a fresh process
kv> get 42
hello
kv> scan 10 200
42 = hello
```

## Roadmap

Built bottom-up; each milestone is an independently testable, demoable artifact.

| # | Milestone | Headline |
|---|-----------|----------|
| **M1** | **Storage engine** ✅ | pager · buffer pool · B+-tree · overflow · durability |
| **M2** | **SQL frontend + executor** ✅ | lexer → Pratt parser → binder → volcano executor |
| **M3** | **WAL + crash recovery** ✅ | no-steal redo log; crash mid-write, restart, data intact |
| **M4** | **MVCC transactions** ✅ | version chains; snapshot isolation · `BEGIN`/`COMMIT`/`ROLLBACK` · write conflicts · `VACUUM` |
| **M5** | **Joins, aggregates & cost-based optimizer** ✅ | hash/nested-loop joins · `GROUP BY`/`HAVING` · predicate pushdown · PK index seeks · join ordering · `EXPLAIN` |
| **M6** | **PostgreSQL wire protocol** ✅ | connect with real `psql`; simple query · transactions · hand-rolled v3 framing |
| **M7** | **WASM web playground** ✅ | in-browser engine (hand-rolled C ABI) + live B+-tree visualizer |
| **M8** | **Benchmarks + docs** ✅ | SQLite comparison · mdBook architecture book |

## Layout

```
crates/storage   disk · buffer pool · slotted pages · B+-tree · WAL + recovery
crates/sql       lexer · Pratt parser · AST
crates/engine    catalog · tuple codec · evaluator · MVCC · planner + optimizer · executor
crates/pgwire    PostgreSQL wire protocol server (ferrodb-pg)
crates/wasm      WebAssembly bindings (hand-written C ABI, no wasm-bindgen)
crates/bench     SQLite-comparison benchmark harness
crates/cli       ferrodb-kv (raw KV) + ferrodb (SQL shell)
web/             browser playground + live B+-tree visualizer
docs/            design specs + implementation plans + mdBook architecture book
```

## Develop

```console
cargo test --workspace                              # all tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Requires stable Rust. CI runs fmt + clippy (warnings-as-errors) + the full test suite on
every push.
