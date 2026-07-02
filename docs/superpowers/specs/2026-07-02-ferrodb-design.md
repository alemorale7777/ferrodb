# ferrodb — A Relational Database Engine in Rust (Design Spec)

> **Status:** Draft for review · **Date:** 2026-07-02 · **Author:** Alejandro Morales
> **Working name:** `ferrodb` (iron + db). Trivially renamed later — every reference lives in one workspace `name` field and the docs.

## 1. What we are building & why

A **full ACID relational database**, written from scratch in **Rust**, with no dependency on an
existing storage engine, SQL parser, or query planner. The differentiator is *depth*: this is not a
`Vec<Row>` wrapped in a `match` statement. It is a real page-based storage engine with B+-trees,
write-ahead logging with crash recovery, MVCC snapshot-isolation transactions, and a cost-based query
optimizer — the same subsystems, in the same layering, that PostgreSQL and SQLite are built from.

**Audience & goal.** This is a portfolio-defining project. The success criterion is that a senior
engineer or systems-focused recruiter reads the repo and thinks *"they actually built that."* Three
concrete demo moments carry that message:

1. **`kill -9` durability** — crash the process mid-write, restart, data is intact and atomic.
2. **`psql` connects to it** — a from-scratch database you talk to with the *real* PostgreSQL client.
3. **Watch the B+-tree split** — a WASM web playground that animates the internals live as you type SQL.

**Non-goals (YAGNI).** Not distributed. Not replicated. No planner for exotic SQL (CTEs, window
functions, subqueries beyond scalar/`IN`). No user auth beyond `trust`. No stored procedures. Single
node, single database file (plus its WAL). These are explicitly out of scope to keep the project
finishable; the depth goes into the core engine, not surface-area breadth.

## 2. Architecture (bottom-up layers)

```
┌──────────────────────────────────────────────────────────────────────┐
│  Interfaces:   REPL/CLI  │  pgwire server (psql)  │  WASM + web UI     │
├──────────────────────────────────────────────────────────────────────┤
│  Engine:       Database · Session · execute(sql) -> ResultSet          │
├──────────────────────────────────────────────────────────────────────┤
│  SQL frontend: Lexer → Pratt Parser → AST → Binder (name/type resolve) │
├──────────────────────────────────────────────────────────────────────┤
│  Planner:      Logical plan → Cost-based optimizer → Physical plan     │
│  Executor:     Volcano/iterator operators (scan/filter/join/agg/sort)  │
├──────────────────────────────────────────────────────────────────────┤
│  Txn/MVCC:     Transaction manager · snapshots · visibility · vacuum   │
├──────────────────────────────────────────────────────────────────────┤
│  Catalog:      System tables describing tables/columns/indexes/stats   │
├──────────────────────────────────────────────────────────────────────┤
│  Storage:      B+-tree (tables + indexes) · slotted pages              │
│                WAL (redo/undo, checkpoints) · Buffer pool · Disk mgr    │
└──────────────────────────────────────────────────────────────────────┘
```

Each layer depends only on the layers below it and talks to them through a narrow interface, so any
one subsystem can be understood and tested in isolation.

### 2.1 Storage layer (`crates/storage`)

- **Disk manager.** Single data file. Fixed **4 KiB pages** addressed by `PageId(u32)`. Page 0 is the
  meta page (magic, version, page size, free-list head, catalog root page id, WAL state). A **free
  list** tracks reclaimed pages for reuse.
- **Buffer pool.** In-memory cache of pages with a **clock-sweep** eviction policy, pin/unpin ref
  counts, and a dirty flag. Dirty pages flush to disk only after their WAL records are durable
  (WAL-before-data invariant, enforced by the pager checking each page's `page_lsn` against the log's
  flushed LSN before writeback).
- **Page checksums.** Every page carries a CRC32C over its bytes; verified on read to catch torn/
  corrupt pages.
- **B+-tree.** A key-ordered map built on the pager. Internal and leaf node pages use a **slotted-page
  layout** (a slot directory of `(offset,len)` growing down, cell data growing up) to hold variable-
  length keys and values. Operations: `search`, `insert` (with node **split** and parent propagation),
  `delete` (with **merge/redistribute** on underflow), and forward/backward **range scans** via leaf
  sibling pointers. Large values spill to **overflow page** chains. The same B+-tree backs both **table
  heaps** (keyed by an internal 8-byte `RowId`) and **secondary indexes** (keyed by index-key bytes →
  `RowId`). Keys are encoded with an **order-preserving byte encoding** so `memcmp` matches SQL
  ordering.

### 2.2 Write-ahead log & recovery (`crates/storage::wal`)

- **ARIES-lite.** Physiological **redo** log records (page id + before/after or logical redo) plus
  logical **undo** records, an in-memory **transaction table** and **dirty-page table**, and periodic
  **fuzzy checkpoints**.
- **Durability.** Log records are appended to the WAL file and **`fsync`'d on `COMMIT`**; a commit
  record with the txn id makes the transaction durable. `page_lsn` on each page records the last log
  record that modified it.
- **Recovery** (on startup, three passes): **Analysis** (rebuild txn/dirty-page tables from the last
  checkpoint), **Redo** (replay all logged changes ≥ the dirty-page recovery LSN, making the DB
  reflect the log), **Undo** (roll back transactions with no commit record, using CLRs so undo is
  itself idempotent under a second crash).
- **Proof-of-correctness demo.** A crash-injection test harness runs a workload in a child process,
  `kill -9`s it at randomized points, restarts, and asserts atomicity + durability invariants.

### 2.3 Transactions & MVCC (`crates/txn`)

- **Model.** Multi-version concurrency control, PostgreSQL-style. Each stored tuple has a header with
  `(xmin, xmax)` — the txn that created it and the txn that deleted/superseded it. `UPDATE` is
  delete-old + insert-new-version.
- **Snapshots.** A monotonically increasing `TxnId`. On `BEGIN` a transaction captures a **snapshot**
  (its own id + the set of in-progress txn ids). A tuple is **visible** to a snapshot iff `xmin` is
  committed-and-in-snapshot and `xmax` is not. This gives **snapshot isolation**; readers never block
  and never see uncommitted data.
- **Commit status.** A commit log (`CLOG`-style bitmap: in-progress / committed / aborted) records the
  fate of each txn, checkpointed and recovered alongside the WAL.
- **Write conflicts.** First-committer-wins: on `UPDATE`/`DELETE` of a row whose version changed since
  the snapshot, the later txn aborts with a serialization error.
- **API.** `BEGIN` / `COMMIT` / `ROLLBACK`, plus an implicit single-statement transaction (autocommit)
  when no explicit `BEGIN` is open.
- **Vacuum.** A background/`VACUUM`-command pass reclaims tuple versions no longer visible to any live
  snapshot, returning space to the free list.

### 2.4 Catalog (`crates/catalog`)

Schema is stored **in the database itself** as system tables (bootstrapped on first open):
`ferro_tables`, `ferro_columns`, `ferro_indexes`, `ferro_stats`. The catalog exposes typed lookups
(table by name → columns, types, root page id, indexes, row-count/stat estimates) to the binder and
planner. Self-describing: the catalog tables are themselves rows in the catalog.

### 2.5 SQL frontend (`crates/sql`)

- **Lexer** → tokens. **Pratt (precedence-climbing) parser** → AST. Hand-written, no `sqlparser` crate
  (writing the parser is part of the point).
- **Supported grammar:** `CREATE TABLE` (with `PRIMARY KEY`, `NOT NULL`, `UNIQUE`), `DROP TABLE`,
  `CREATE INDEX` / `DROP INDEX`, `INSERT`, `SELECT` (projection & `*`, `WHERE`, `ORDER BY`, `LIMIT`/
  `OFFSET`, `GROUP BY` + `HAVING`, aggregates `COUNT/SUM/AVG/MIN/MAX`, `INNER`/`LEFT JOIN ... ON`),
  `UPDATE`, `DELETE`, `BEGIN`/`COMMIT`/`ROLLBACK`, `EXPLAIN`, `VACUUM`.
- **Types:** `INTEGER` (i64), `REAL` (f64), `TEXT`, `BOOLEAN`, with SQL three-valued `NULL` logic.
- **Binder/analyzer:** resolves table/column names against the catalog, type-checks expressions,
  desugars `*`, assigns column ids. Produces a typed logical plan.

### 2.6 Planner & optimizer (`crates/planner`)

- **Logical plan:** relational-algebra tree (Scan, Filter, Project, Join, Aggregate, Sort, Limit).
- **Cost-based optimizer (System-R style):**
  - **Statistics** from `ANALYZE`: per-table row counts, per-column NDV (distinct values) and
    min/max/histogram, used to estimate selectivities.
  - **Access-path selection:** choose sequential scan vs. index scan per table from estimated cost.
  - **Join ordering:** bottom-up dynamic programming over join sets for small N (≤ ~8 relations),
    heuristic greedy ordering above that; choose nested-loop vs. hash join by cost.
  - **Rewrites:** predicate pushdown, projection pruning, constant folding.
- **`EXPLAIN`** prints the chosen physical plan as a tree with estimated rows and cost per node.

### 2.7 Executor (`crates/executor`)

**Volcano/iterator model** — every operator implements `next() -> Option<Tuple>` and pulls from its
children. Operators: `SeqScan`, `IndexScan`, `Filter`, `Project`, `NestedLoopJoin`, `HashJoin`,
`HashAggregate`, `Sort` (external merge sort if it spills), `Limit`, `Insert`, `Update`, `Delete`.
Expression evaluation is a small typed interpreter over the bound AST. Every scan/mutation goes through
the MVCC visibility check and the WAL.

### 2.8 Engine & interfaces

- **`crates/engine`** — the public façade: `Database::open(path)`, `Session`, `session.execute(sql) ->
  ResultSet`. Everything above composes here.
- **`crates/cli`** — `ferrodb <file.db>` interactive REPL (rustyline): multiline SQL, `.tables`,
  `.schema`, `.explain`, pretty-printed result tables, timing.
- **`crates/pgwire`** — a TCP server implementing enough of the **PostgreSQL v3 wire protocol**
  (startup/`trust` auth, simple query, extended query: Parse/Bind/Describe/Execute, `RowDescription`,
  `DataRow`, `CommandComplete`, `ErrorResponse`, `ReadyForQuery`) that **real `psql`, DBeaver, and
  standard Postgres drivers connect and run queries.** Hand-rolled, no `pgwire` crate.
- **`crates/wasm`** — `wasm-bindgen` bindings compiling the engine (with an in-memory pager backend)
  to WebAssembly for the browser playground, exposing `execute`, plus hooks to introspect B+-tree
  structure, the WAL tail, and MVCC version chains for visualization.

### 2.9 Web playground (`web/`)

A browser SQL playground (React + TypeScript + Vite) running the WASM engine entirely client-side:
Monaco SQL editor, result grid, and three live visualizers — **B+-tree** (D3, animating splits/merges
as you `INSERT`/`DELETE`), **query plan** (the `EXPLAIN` tree), and an **MVCC/WAL inspector** showing
version chains and the log tail. Deployed to Vercel. This is the teaching artifact and the most
shareable demo.

### 2.10 Benchmarks (`bench/`) & docs (`docs/book/`)

- **Benchmarks:** a Criterion + custom harness comparing ferrodb to **SQLite** (`rusqlite`) on
  micro-ops (point lookup, range scan, bulk insert) and a small TPC-ish workload, output as committed
  charts with an honest write-up of where we win, lose, and why.
- **Docs:** an **mdBook** ("Build a Database") walking through every subsystem with diagrams — the
  "research" deliverable that proves understanding, not just output.

## 3. Workspace layout

A Cargo workspace. Core engine crates stay **dependency-light** (that austerity is the credibility
signal); heavier deps are confined to the edges (CLI, bench, wasm).

```
ferrodb/
  Cargo.toml                # workspace
  crates/
    storage/    # disk mgr, buffer pool, slotted pages, B+-tree, WAL, recovery
    catalog/    # system tables & typed schema lookups
    txn/        # transaction manager, snapshots, MVCC visibility, CLOG, vacuum
    sql/        # lexer, Pratt parser, AST, binder
    planner/    # logical plan, statistics, cost-based optimizer, physical plan
    executor/   # volcano operators, expression evaluator
    engine/     # Database / Session / execute() façade tying it together
    cli/        # ferrodb REPL binary
    pgwire/     # PostgreSQL v3 wire-protocol server binary
    wasm/       # wasm-bindgen bindings + introspection hooks
  bench/        # SQLite comparison harness + charts
  web/          # React + Vite WASM playground (B+-tree / plan / MVCC visualizers)
  docs/book/    # mdBook architecture book
```

**Allowed dependencies (core):** `thiserror`/`anyhow` (errors), `crc32fast` (checksums). *Not* allowed
in core: any SQL parser, storage engine, or B+-tree crate. **Edges:** `rustyline` (CLI), `criterion` +
`rusqlite` (bench, dev-only), `wasm-bindgen` (wasm), `tokio` *only if needed* for the pgwire server
(std threads acceptable). **Dev:** `proptest` (property tests), `tempfile`.

## 4. Testing strategy

Depth is only credible if it's *verified*. Every milestone ships with:

- **Unit tests** per module (page encode/decode, tree ops, encodings, visibility rules).
- **Property tests** (`proptest`): B+-tree invariants (sorted, balanced, round-trip vs. a
  `BTreeMap` model), order-preserving key encoding, slotted-page compaction.
- **Crash-injection tests:** child process runs a workload, gets `kill`ed at randomized points,
  restarts; assert atomicity + durability (M3).
- **Concurrency/isolation tests:** concurrent readers/writers assert snapshot isolation, no dirty/
  non-repeatable reads, write-conflict aborts (M4).
- **SQL logic tests:** `sqllogictest`-style `.test` files (query → expected rows) as end-to-end
  golden tests; **golden `EXPLAIN`** tests for the optimizer (M5).
- **Protocol tests:** a real Postgres driver / `psql` script connects and runs a query set (M6).

## 5. Milestone roadmap (each is its own spec → plan → implementation cycle)

This system is too large for one implementation plan, and each layer is independently valuable and
testable. We build **bottom-up**, and *every milestone ends in a working, demoable artifact*. We write
the implementation plan for one milestone at a time.

| # | Milestone | Deliverable / demo | Depends on |
|---|-----------|--------------------|-----------|
| **M1** | **Storage engine** | Pager + buffer pool + slotted pages + B+-tree (insert/search/range/delete) + free list + checksums. A key-value library + tiny `put/get/scan` CLI, fully property-tested. | — |
| **M2** | **SQL frontend + catalog + executor** | Lexer/Pratt parser/AST/binder, catalog system tables, volcano executor (seq scan/filter/project/insert), REPL. `CREATE TABLE`/`INSERT`/`SELECT … WHERE` persisted to disk. **"A usable database."** | M1 |
| **M3** | **WAL + crash recovery** | Log manager, redo/undo, checkpoints, 3-pass recovery. Crash-injection harness proving atomicity + durability. **`kill -9` demo.** | M2 |
| **M4** | **MVCC transactions** | Tuple versioning, snapshots, visibility, CLOG, `BEGIN/COMMIT/ROLLBACK`, `VACUUM`. Concurrency/isolation test suite. | M3 |
| **M5** | **Cost-based optimizer + joins/aggregates** | Statistics/`ANALYZE`, index vs. seq-scan selection, join ordering, hash join, hash aggregate, external sort, `EXPLAIN`. | M4 |
| **M6** | **PostgreSQL wire protocol** | pg v3 server; **real `psql` connects and queries.** | M5 |
| **M7** | **WASM web playground** | Client-side WASM engine + Monaco editor + live B+-tree / plan / MVCC visualizers, deployed. | M5 |
| **M8** | **Benchmarks + mdBook docs** | SQLite comparison charts + write-up; architecture book with diagrams. | M6 |

**"Impressive commit history"** falls out of this naturally: bottom-up, TDD, one focused subsystem per
milestone means a clean, legible, green-CI commit graph that reads like a syllabus for building a
database — which is exactly the story we want a reviewer to see.

## 6. Success criteria

- [ ] `cargo test --workspace` green; property + crash + concurrency suites pass in CI (GitHub Actions).
- [ ] `kill -9` mid-transaction, restart → data atomic & durable (M3 harness).
- [ ] `psql -h localhost -p 5433` connects and runs `SELECT`/`INSERT`/`JOIN`/`EXPLAIN` (M6).
- [ ] Web playground live on Vercel; B+-tree animates on insert (M7).
- [ ] Benchmark charts + honest write-up committed; mdBook published (M8).
- [ ] Core engine crates depend on **no** third-party SQL/storage/btree crate.

## 7. Open questions for review

1. **Name** — keep `ferrodb`, or prefer something else? (One-line change.)
2. **Milestone order for M7 vs M6** — both depend only on M5 and are independent; either can come
   first. Default: M6 (psql) before M7 (web), since the wire protocol is the bigger flex.
3. **pgwire async** — hand-rolled std-thread server (fewer deps, more "from scratch") vs. `tokio`
   (cleaner concurrency). Default: **std threads** to keep the dependency-austerity story intact.
