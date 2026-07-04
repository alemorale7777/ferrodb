# SQL frontend & executor

Above the storage and transaction layers sits the part users actually touch: SQL. ferrodb parses and
executes SQL with **no `sqlparser` crate** — a hand-written lexer and a Pratt parser feed an AST that
a volcano-style executor runs over the B+-trees.

## Lexer

The lexer turns the raw SQL string into a stream of tokens — keywords, identifiers, string and
number literals, operators, punctuation. It is a straightforward character scanner; the only
subtleties are case-insensitive keywords and distinguishing `-` the operator from a negative number.

## Pratt parser

Expressions are parsed with a **Pratt parser** (top-down operator precedence). Each operator has a
binding power; the parser reads a prefix (a literal, a column, a parenthesized group, a function
call) and then, while the next operator binds tighter than the current context, folds it in. This
handles `a + b * c`, `NOT x AND y`, comparison chains, and qualified column references (`t.col`)
cleanly, without a precedence-climbing table of special cases. Statements (`SELECT`, `INSERT`,
`CREATE`, `UPDATE`, `DELETE`) are parsed by recursive descent around the expression parser.

## The catalog

The database is **self-describing**: table definitions live in a catalog stored *in the same file*,
in its own B+-tree keyed by table name. A `TableSchema` records the columns and their types, the
root page of the table's data tree, the next row id, and (as of M8) a `row_count` statistic for the
optimizer. `CREATE TABLE` writes a catalog entry; every query reads the schema back out of it.

## Types and three-valued logic

ferrodb has four types — `INTEGER`, `REAL`, `TEXT`, `BOOLEAN` — and, like SQL, a first-class `NULL`.
Comparisons and boolean operators follow **three-valued logic**: `NULL = NULL` is not true but
`NULL`, `NULL AND false` is `false`, `NULL AND true` is `NULL`. The evaluator implements these
tables so `WHERE` filters and expressions behave as SQL requires rather than treating `NULL` as a
sentinel value.

## The volcano executor

Execution is a tree of operators, each of which produces rows. In M2 this is a simple pipeline:
scan a table, filter rows by `WHERE`, sort by `ORDER BY`, project the selected columns, apply
`LIMIT`/`OFFSET`. Each operator pulls from its child — the classic **volcano** model. `INSERT`
encodes a tuple and puts it into the table's B+-tree; `UPDATE` and `DELETE` scan, match, and rewrite
version chains (Chapter 5).

This clean operator model is what M5 builds on: the optimizer's job is to choose *which* operators
to assemble and in *what order*, without changing how any single operator works.
