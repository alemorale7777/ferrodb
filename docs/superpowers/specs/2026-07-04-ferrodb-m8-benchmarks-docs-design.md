# ferrodb Milestone 8 — Benchmarks & Architecture Book (Design Spec)

> **Status:** Shipped · **Date:** 2026-07-04 · Builds on M1–M7.

## 1. Goal

Close the project with two artifacts that make the engine *legible* to an outside reader:

1. A **benchmark harness** that runs ferrodb head-to-head against **bundled SQLite** on the same
   in-memory workloads, so the numbers are honest and reproducible.
2. An **mdBook architecture book** that walks the whole machine bottom-up — pager, B+-tree, WAL,
   MVCC, SQL, optimizer, wire protocol, WASM — with the benchmark results as its closing chapter.

The benchmark is also a *forcing function*: running it at scale exposes performance bugs that the
correctness tests never would.

## 2. The benchmark harness (`crates/bench`)

A standalone binary that times five workloads over a 20 000-row dataset, fully in memory, driving
both engines with **the same SQL strings** and no statement caching — so each measurement covers the
entire parse → plan → execute path, not a prepared-statement fast path.

| Workload | Query shape | What it exercises |
|----------|-------------|-------------------|
| bulk insert | batched multi-row `INSERT` | write path, WAL commit, B+-tree splits |
| point lookup | `SELECT v FROM t WHERE id = k` | PK **index seek** |
| range scan | `SELECT COUNT(*) … WHERE id >= a AND id < b` | PK **index range scan** |
| aggregate | `SELECT COUNT(*), SUM(v), AVG(v) … WHERE v >= 0` | full scan + aggregation |
| hash join | `SELECT COUNT(*) FROM t JOIN u ON t.id = u.id` | 20k × 20k **hash join** |

**Fairness rules.** SQLite ships behind an optional `sqlite` feature (via `rusqlite` with the
`bundled` build), so the default workspace build and CI stay dependency-light. Both engines use the
same batched multi-row inserts — ferrodb's no-steal buffer pool bounds a single transaction's dirty
set, so a 20k-row load is committed in batches rather than one giant transaction. The aggregate uses
`WHERE v >= 0` (which matches every row) to force a genuine scan on both sides instead of measuring
SQLite's bare-`COUNT(*)` metadata shortcut.

## 3. Optimizer fixes the benchmark exposed

Running the harness at scale surfaced two real planner gaps, both fixed as part of M8:

- **Cardinality from a statistic, not a scan.** `gather_rels()` estimated each relation's row count
  by calling `count_visible()` — a *full B+-tree walk* — on every query. A PK point lookup therefore
  did O(n) work at plan time regardless of the index. `TableSchema` now carries a `row_count`
  statistic (analogous to PostgreSQL's `reltuples`), persisted in the catalog and maintained
  incrementally on `INSERT`/`DELETE`; the planner reads it directly. Point lookups over 20k rows fell
  from ~5 ms to ~7 µs each.
- **Index range scans.** The planner had only an equality `IndexSeek`, so `pk >`/`>=`/`<` predicates
  fell back to a full scan. A new `IndexRange` access path drives the existing B+-tree bounded scan
  (`lo` inclusive, `hi` exclusive), seeking straight to the start leaf and walking the sibling chain.
  The bounds are conservative supersets and the original predicate stays as the scan's residual
  filter, so `>` widens to an inclusive lower bound and `<=` (no exclusive-upper key exists) falls
  back to a sequential scan — exact semantics are preserved in every case. Range scans over 20k rows
  fell ~90×.

## 4. Representative results

One machine (Windows, `rustc` release build), ferrodb vs bundled SQLite, ratio = ferrodb ÷ sqlite:

| Workload | ferrodb | sqlite | ratio |
|----------|--------:|-------:|------:|
| bulk insert (20 000) | ~133 ms | ~7 ms | ~19× |
| point lookup (20 000) | ~132 ms | ~30 ms | ~4× |
| range scan (2 000 × 200) | ~159 ms | ~11 ms | ~15× |
| aggregate scan (50×) | ~398 ms | ~48 ms | ~8× |
| hash join (10×) | ~248 ms | ~4 ms | ~56× |

The **index-driven** workloads — the whole point of the M5 optimizer — are within a small constant
factor of a mature C database. The **full-scan and join** workloads honestly expose the cost of a
row-at-a-time interpreter that materializes each operator's output into a `RowSet`; a streaming
(iterator) executor is the obvious next step and the largest remaining lever.

## 5. The architecture book (`docs/book`)

An **mdBook** built from Markdown, one chapter per subsystem, in build order so a reader can follow
the engine from the raw file upward:

1. Introduction & the milestone map
2. Storage — pager, buffer pool, slotted pages
3. The B+-tree
4. WAL & crash recovery
5. MVCC transactions
6. SQL frontend — lexer, Pratt parser, executor
7. The cost-based optimizer
8. PostgreSQL wire protocol
9. WebAssembly playground
10. Benchmarks — the table above and how to reproduce it

`book.toml` + `SUMMARY.md` + `src/*.md`, buildable with `mdbook build`. The prose is a distilled,
navigable retelling of the seven design specs, not a duplicate of them.

## 6. Verification

- `cargo run -p ferrodb-bench --release --features sqlite` prints the comparison table; the default
  build (no feature) prints the ferrodb-only column and still compiles clean.
- A new engine test asserts the `IndexRange` plan shape and its exact boundary semantics
  (`>` excludes the endpoint, `<=` falls back to a seq scan, both return the correct rows).
- `mdbook build docs/book` renders without errors.

## 7. Success criteria

- [x] Benchmark harness runs both engines on five workloads; SQLite gated behind a feature.
- [x] The two optimizer bugs the benchmark exposed are fixed and covered by tests.
- [x] mdBook architecture book builds, with a Benchmarks chapter carrying real numbers.
- [x] `cargo test --workspace` green; fmt + clippy clean (default **and** `sqlite` feature); CI green.
