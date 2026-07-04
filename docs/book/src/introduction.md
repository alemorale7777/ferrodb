# Introduction

**ferrodb** is a relational database written from scratch in Rust. It is a page-based storage
engine, B+-tree indexes, write-ahead logging with crash recovery, MVCC transactions, a hand-written
SQL frontend, a cost-based query optimizer, the PostgreSQL wire protocol, and a WebAssembly build —
with **no third-party SQL parser, storage engine, or B+-tree crate**. The point is to build the
machine, not to glue one together.

This book is an architecture tour. Each chapter takes one subsystem and explains *what it does, how
it works, and why it is built that way*, following the same bottom-up order the database was built
in: you cannot understand the executor until you understand the B+-tree, and you cannot understand
the B+-tree until you understand the pager.

## The milestone map

ferrodb was built as eight milestones, each an independently testable, demoable artifact:

| # | Milestone | Headline |
|---|-----------|----------|
| M1 | Storage engine | pager · buffer pool · B+-tree · overflow · durability |
| M2 | SQL frontend + executor | lexer → Pratt parser → binder → volcano executor |
| M3 | WAL + crash recovery | no-steal redo log; crash mid-write, restart, data intact |
| M4 | MVCC transactions | version chains; snapshot isolation; write conflicts; `VACUUM` |
| M5 | Joins, aggregates & optimizer | hash/nested-loop joins; predicate pushdown; PK index access; `EXPLAIN` |
| M6 | PostgreSQL wire protocol | connect with real `psql`; hand-rolled v3 framing |
| M7 | WASM playground | in-browser engine (hand-rolled C ABI) + live B+-tree visualizer |
| M8 | Benchmarks + docs | SQLite comparison; this book |

## The crate layout

```
crates/storage   disk · buffer pool · slotted pages · B+-tree · WAL + recovery
crates/sql       lexer · Pratt parser · AST
crates/engine    catalog · tuple codec · evaluator · MVCC · planner + optimizer · executor
crates/pgwire    PostgreSQL wire protocol server (ferrodb-pg)
crates/wasm      WebAssembly bindings (hand-written C ABI, no wasm-bindgen)
crates/cli       ferrodb-kv (raw KV) + ferrodb (SQL shell)
crates/bench     SQLite-comparison benchmark harness
```

A recurring theme worth watching for: **each layer is an abstraction the layer above can rely on
without knowing its internals.** The B+-tree talks to the buffer pool, never to the file. The
executor talks to the B+-tree, never to a page. This is what makes it possible to swap the file for
a `Vec<u8>` and run the whole engine in a browser (Chapter 9) without touching the SQL layer.
