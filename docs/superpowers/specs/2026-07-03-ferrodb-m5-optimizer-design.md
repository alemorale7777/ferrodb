# ferrodb Milestone 5 — Joins, Aggregates & a Cost-Based Optimizer (Design Spec)

> **Status:** Draft for review · **Date:** 2026-07-03 · Builds on M1–M4.

## 1. Goal

Turn the single-table interpreter into a real **query engine**: multi-table joins, grouped
aggregation, and a **cost-based optimizer** that chooses join order and access paths from
cardinality estimates, with `EXPLAIN` to show its work.

Concretely:

- `INNER JOIN` and `LEFT [OUTER] JOIN … ON …`, with table aliases and qualified columns (`u.id`).
- Aggregates `COUNT(*) / COUNT / SUM / AVG / MIN / MAX` with `GROUP BY` and `HAVING`.
- A planner that: pushes single-table predicates down to scans, picks a **PK index seek** when a
  sargable equality/range on the primary key is available (vs. a full scan), and orders joins to
  minimise estimated intermediate cardinality.
- `EXPLAIN <query>` prints the physical plan tree with per-node estimated row counts.

Everything stays single-file, MVCC-correct, and green in CI.

## 2. Why a plan layer

Today `exec_select` is a hard-coded scan → filter → sort → project. Joins and aggregates need an
explicit **plan tree** so the optimizer can rewrite it. We introduce:

- `engine::plan` — physical plan nodes (`SeqScan`, `IndexSeek`, `NestedLoopJoin`, `HashJoin`,
  `Filter`, `HashAggregate`, `Sort`, `Limit`, `Project`) plus a `RowSet` (a schema of qualified
  columns + the materialised rows) that flows between them.
- `engine::planner` — builds a logical plan from the `Select` AST, applies rewrites (predicate
  pushdown, access-path selection, join ordering), and emits a physical plan.
- The executor materialises each node bottom-up (volcano-style but eager: every node returns a
  full `RowSet`). Eager is fine at this scale and keeps the code and tests simple.

## 3. Expression & schema model

`Expr::Column` gains an optional qualifier: `Column { table: Option<String>, name: String }`.
Resolution against a `RowSet` schema matches on `name` (and `table` when qualified); an ambiguous
unqualified name is an error. Aggregate calls become `Expr::Aggregate { func, arg }` (`arg = None`
is `COUNT(*)`).

`eval.rs` is refactored around a **leaf resolver**: `eval_with(expr, leaf)` where `leaf(&Expr)`
resolves `Column`/`Aggregate` nodes to a `Value` and returns `None` for everything else (which
`eval_with` recurses through, reusing the existing 3-valued logic / arithmetic / comparison). This
lets the same evaluator serve (a) single-table INSERT/UPDATE/DELETE, (b) join rows with qualified
columns, and (c) post-aggregation rows where an aggregate expression reads a precomputed column.

## 4. Joins

FROM becomes a left-deep chain: a base `TableRef { name, alias }` followed by zero or more
`Join { join_type, right, on }`. Execution per join:

- **Hash join** when the `ON` predicate is (or contains) an equijoin `left.a = right.b`: build a
  hash table on the smaller side's key, probe with the other. `LEFT` joins emit a NULL-padded row
  for any left row with no match.
- **Nested-loop join** otherwise (non-equi or no usable key), applying the `ON` predicate per pair.

Column schema of the result is the concatenation of the input schemas (qualified by alias/name).

## 5. Aggregation

If the query has any aggregate in its projection/HAVING, or a `GROUP BY`, run a **hash aggregate**:
partition input rows by the tuple of `GROUP BY` key values, and for each group fold the aggregates.
`COUNT(*)` counts rows; `COUNT/SUM/AVG/MIN/MAX(expr)` skip NULLs (SQL semantics); `AVG` is
`SUM/COUNT` as REAL; an empty input with no `GROUP BY` yields one row (`COUNT=0`, others NULL). The
output schema is the group columns followed by one synthetic column per distinct aggregate; `HAVING`
and the projection resolve aggregate expressions to those columns.

Non-aggregated queries skip this stage entirely (unchanged behaviour).

## 6. Cost-based optimizer

**Statistics.** Base-table cardinality is the count of MVCC-visible rows (obtained cheaply while the
scan runs, or by a counting pre-pass for ordering). Predicate **selectivity** uses standard
heuristics: `col = const` → `1/max(distinct,1)` (PK ⇒ 1 row); range (`<,<=,>,>=`) → `0.3`;
equality between columns (join) → `1/max(left_card,right_card)`; conjunction multiplies, `OR` adds
(capped at 1). These are estimates — the point is a *principled* ordering, not exact costs.

**Rewrites, in order:**

1. **Predicate pushdown** — split the `WHERE` (and `ON`) conjuncts; a conjunct referencing a single
   relation is attached to that relation's scan; multi-relation conjuncts stay as join/residual
   filters.
2. **Access-path selection** — per base relation, if a pushed conjunct is a sargable predicate on
   the primary key (`pk = const` ⇒ `IndexSeek`, est. 1 row; `pk <cmp> const` ⇒ index range), use the
   B+-tree; otherwise `SeqScan`. (Non-PK columns have no secondary index yet — always `SeqScan`.)
3. **Join ordering** — System-R-style DP over the relation set minimising the sum of intermediate
   result cardinalities, preferring predicate-connected pairs to avoid cross products; for > 8
   relations fall back to a greedy smallest-cardinality-first heuristic. Produces a left-deep tree.

`EXPLAIN <select>` returns the physical plan as text rows, each node indented under its parent with
its estimated row count and (for scans) the chosen access path and pushed predicate.

## 7. Grammar additions

```
select        := SELECT select_list FROM table_ref { join } [WHERE expr]
                 [GROUP BY expr {, expr}] [HAVING expr]
                 [ORDER BY order_key {, order_key}] [LIMIT n [OFFSET n]]
select_item   := '*' | ident '.' '*' | expr [AS ident | ident]
table_ref     := ident [AS ident | ident]
join          := [INNER | LEFT [OUTER]] JOIN table_ref ON expr
order_key     := expr [ASC | DESC]
primary (new) := ident '.' ident        -- qualified column
               | ident '(' (expr | '*') ')'   -- aggregate call
explain       := EXPLAIN select
```

New tokens: `.` (`Token::Dot`). New keywords: `AS, JOIN, INNER, LEFT, OUTER, ON, GROUP, HAVING,
EXPLAIN`. Aggregate names stay identifiers, recognised in the parser.

## 8. AST changes

- `Expr::Column(String)` → `Expr::Column { table: Option<String>, name: String }`;
  add `Expr::Aggregate { func: AggFunc, arg: Option<Box<Expr>> }` and `enum AggFunc`.
- `SelectItem` → `Wildcard | QualifiedWildcard(String) | Expr { expr, alias }`.
- `OrderBy` → `{ expr: Expr, descending: bool }`; `Select.order_by: Vec<OrderBy>`.
- `Select` gains `from: TableRef`, `joins: Vec<Join>`, `group_by: Vec<Expr>`, `having: Option<Expr>`.
- New `Statement::Explain(Box<Statement>)`.

## 9. Testing

- **Parser:** joins, aliases, qualified columns, aggregates, `GROUP BY`/`HAVING`, multi-key
  `ORDER BY`, `EXPLAIN`.
- **Joins:** inner join matches; left join null-pads; hash vs nested-loop produce identical results;
  3-table join.
- **Aggregates:** global `COUNT/SUM/AVG/MIN/MAX`; `GROUP BY` with `HAVING`; NULL handling; empty
  table global aggregate.
- **Optimizer:** `EXPLAIN` shows `IndexSeek` for `WHERE pk = k` and `SeqScan` otherwise; join order
  puts the smaller/most-selective relation first; results are identical with and without the chosen
  order (correctness invariant).
- **Regression:** all M1–M4 suites stay green; single-table SELECT semantics unchanged.

## 10. Success criteria

- [ ] Inner/left joins, grouped aggregation, `HAVING`, multi-key `ORDER BY` all work.
- [ ] Optimizer picks PK index seeks and a cost-ordered left-deep join tree; `EXPLAIN` shows it.
- [ ] `cargo test --workspace` green; fmt + clippy clean; CI green.
