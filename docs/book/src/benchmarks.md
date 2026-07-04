# Benchmarks

The final milestone measures ferrodb against a mature C database — **bundled SQLite** — on the same
in-memory workloads. The goal is not to win (it does not) but to be **honest and reproducible**, and
to use scale as a forcing function that exposes performance bugs the correctness tests never would.

## The harness

`crates/bench` drives five workloads over a 20 000-row dataset, fully in memory, sending **the same
SQL strings** to both engines with no statement caching — so every number covers the whole
parse → plan → execute path. SQLite sits behind an optional `sqlite` Cargo feature (via `rusqlite`
with the bundled build), so the default workspace build and CI stay dependency-light.

```console
$ cargo run -p ferrodb-bench --release --features sqlite
```

Two fairness rules matter. Inserts are **batched multi-row statements on both sides** — ferrodb's
no-steal buffer pool bounds a single transaction's dirty set, so a 20k-row load is committed in
batches rather than one giant transaction. The aggregate uses `WHERE v >= 0` (which matches every
row) to force a genuine full scan on both engines, rather than measuring SQLite's special-cased
bare-`COUNT(*)` shortcut.

## Representative results

One machine, release build, ferrodb vs bundled SQLite. `ratio = ferrodb ÷ sqlite` (1.0× = parity,
higher = ferrodb slower):

| Workload | ferrodb | sqlite | ratio |
|----------|--------:|-------:|------:|
| bulk insert (20 000) | ~133 ms | ~7 ms | ~19× |
| point lookup (20 000) | ~132 ms | ~30 ms | ~4× |
| range scan (2 000 × 200) | ~159 ms | ~11 ms | ~15× |
| aggregate scan (50×) | ~398 ms | ~48 ms | ~8× |
| hash join (10×) | ~248 ms | ~4 ms | ~56× |

Numbers vary run to run and machine to machine; treat them as orders of magnitude, not lab results.

## What the numbers say

The **index-driven** workloads — point lookup and range scan, the entire reason the M5 optimizer
exists — land within a small constant factor of SQLite. That is the headline result: a from-scratch
B+-tree with a cost-based optimizer is genuinely competitive on the operations indexes are for.

The **full-scan and join** workloads are slower, and honestly so. They expose the cost of a
**row-at-a-time interpreter that materializes each operator's output into a `RowSet`** before the
next operator runs. A streaming (iterator/volcano-pull) executor that never materializes
intermediate results is the single largest remaining performance lever, and the obvious next step
beyond this milestone.

## The bugs the benchmark caught

Running the harness at scale immediately exposed two optimizer defects that every correctness test
had passed straight over:

1. **Cardinality estimation scanned the table.** The planner counted rows by walking the whole
   B+-tree on every query, so a point lookup was secretly O(n). Replacing the live count with a
   persisted `row_count` statistic (see [The optimizer](./optimizer.md)) took 20k-row point lookups
   from ~5 ms to ~7 µs each — a ~700× improvement.
2. **Range predicates fell back to full scans.** There was no index range access path, so
   `WHERE id >= a AND id < b` scanned all 20k rows. Adding `IndexRange` over the existing B+-tree
   bounded scan cut range queries ~90×.

This is the real argument for benchmarking: not the final table, but the two bugs that only appeared
under load.
