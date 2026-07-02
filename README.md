# ferrodb

**A relational database written from scratch in Rust** — page-based storage, B+-tree
indexes, write-ahead logging with crash recovery, MVCC transactions, a cost-based query
planner, and the PostgreSQL wire protocol. No third-party SQL parser, storage engine, or
B+-tree crate: the point is to build the machine, not glue one together.

> **Status:** Milestones 1–2 complete and green — it runs real SQL, persisted to
> disk. See the [full design & roadmap](docs/superpowers/specs/2026-07-02-ferrodb-design.md).

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
| M3 | WAL + crash recovery | `kill -9` mid-write, restart, data intact |
| M4 | MVCC transactions | snapshot isolation · `BEGIN`/`COMMIT`/`ROLLBACK` · `VACUUM` |
| M5 | Cost-based optimizer | statistics · join ordering · index selection · `EXPLAIN` |
| M6 | PostgreSQL wire protocol | connect with real `psql` |
| M7 | WASM web playground | in-browser engine + live B+-tree visualizer |
| M8 | Benchmarks + docs | SQLite comparison · mdBook architecture book |

## Layout

```
crates/storage   disk · buffer pool · slotted pages · B+-tree · (later) WAL, recovery
crates/sql       lexer · Pratt parser · AST
crates/engine    catalog · tuple codec · evaluator · executor · Database/execute
crates/cli       ferrodb-kv (raw KV) + ferrodb (SQL shell)
docs/            design spec + implementation plans
```

## Develop

```console
cargo test --workspace                              # all tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Requires stable Rust. CI runs fmt + clippy (warnings-as-errors) + the full test suite on
every push.
