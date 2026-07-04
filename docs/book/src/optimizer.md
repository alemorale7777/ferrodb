# The cost-based optimizer

M5 turns the straight-line executor into a real query engine over a **physical plan tree**, with a
**cost-based optimizer** that chooses access paths and join order. `EXPLAIN` prints the plan it
picks.

## The plan tree

A query compiles to a tree of physical operators: `Scan` (with an **access path**), `Join` (hash or
nested-loop), `Filter`, `Aggregate`, `Sort`, `Project`, `Limit`. Each node carries an estimated
output cardinality (`est`) used to compare alternatives. `run_plan` walks this tree to produce rows.

## Access paths

How a base table is read is its **access path**:

- **`SeqScan`** — read every visible row.
- **`IndexSeek`** — a B+-tree point lookup for a `pk = const` predicate. One fetch chain instead of a
  full scan.
- **`IndexRange`** — a B+-tree bounded scan for `pk >`/`>=`/`<` predicates (added in M8). It seeks
  straight to the starting leaf and walks the sibling chain, with `lo` inclusive and `hi` exclusive.

Single-table predicates are **pushed down** onto the scan rather than run as a separate `Filter`
node, so the scan emits fewer rows. For the index paths the original predicate is *also* kept as a
residual filter, so a conservative index bound (for example, `>` widening to an inclusive lower
bound) is always narrowed back to exact SQL semantics.

## Cardinality from a statistic, not a scan

The optimizer needs to know roughly how many rows each table has. The obvious-but-wrong way is to
count them at plan time — which is what an early version did, walking the whole B+-tree on *every*
query and making a point lookup secretly O(n). The fix is the one every real database uses: keep a
**statistic**. `TableSchema.row_count` is maintained incrementally on `INSERT`/`DELETE` and persisted
in the catalog (analogous to PostgreSQL's `reltuples`), so the planner reads a cardinality estimate
in O(1). This single change took 20k-row point lookups from milliseconds to microseconds
(see [Benchmarks](./benchmarks.md)).

## Join algorithms and order

An equijoin (`a.x = b.y`) runs as a **hash join**: build a hash table on the smaller side, probe
with the larger. Without an equality condition it falls back to a **nested-loop join**.

Join *order* matters enormously: joining the two smallest relations first keeps intermediate results
small. For all-`INNER` queries of up to eight relations, ferrodb runs a **System-R-style dynamic
program** over subsets of relations, choosing the left-deep order that minimizes the summed
intermediate cardinality — so it never leads with the biggest table. A query containing a `LEFT`
join falls back to written order, because reordering an outer join is not generally valid.

## EXPLAIN

`EXPLAIN` renders the chosen plan as an indented tree, most-parent first, with each node's estimated
row count and the pushed-down filters. It is how you *see* the optimizer's decisions:

```
Project [u.name AS name]  (rows≈1)
  HashJoin [Inner] on u.id = o.user_id  (rows≈1)
    SeqScan orders o  (rows≈3)
    IndexSeek users u (pk = 1)  (rows≈1)
```

Here the optimizer recognized `u.id = 1` as an index seek, made that one-row relation the build side
of the hash join, and pushed the predicate down onto the `users` scan.
