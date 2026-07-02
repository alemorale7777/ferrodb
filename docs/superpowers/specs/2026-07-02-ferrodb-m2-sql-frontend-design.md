# ferrodb Milestone 2 — SQL Frontend + Catalog + Executor (Design Spec)

> **Status:** Draft for review · **Date:** 2026-07-02 · Builds on the
> [master design](2026-07-02-ferrodb-design.md) §2.4–2.7 and the shipped M1 storage engine.

## 1. Goal

Turn the M1 key/value B+-tree into **a database you drive with SQL**. After M2, this works
end-to-end, persisted to disk through the M1 engine:

```sql
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
INSERT INTO users VALUES (1, 'alejandro', 30), (2, 'sam', 25);
SELECT name, age FROM users WHERE age > 26 ORDER BY name;
```

No transactions, WAL, joins, or optimizer yet — those are M3/M4/M5. M2 is the frontend
(lexer → parser → binder), the **catalog**, and a single-table **volcano executor**.

## 2. Scope

**In scope (M2):**
- `CREATE TABLE` (columns with types + `PRIMARY KEY`, `NOT NULL`), `DROP TABLE`.
- `INSERT INTO t [(cols)] VALUES (...), (...)`.
- `SELECT <cols|*> FROM t [WHERE <expr>] [ORDER BY <col> [ASC|DESC]] [LIMIT n [OFFSET m]]`.
- `DELETE FROM t [WHERE <expr>]`, `UPDATE t SET c = e [, ...] [WHERE <expr>]`.
- Types: `INTEGER` (i64), `REAL` (f64), `TEXT`, `BOOLEAN`, with SQL three-valued `NULL`.
- Expressions: literals, column refs, `AND/OR/NOT`, comparisons (`= <> < <= > >=`),
  arithmetic (`+ - * /`), `IS NULL`/`IS NOT NULL`.
- A REPL (`ferrodb`) that runs SQL and pretty-prints result tables.

**Out of scope (deferred):** joins, aggregates/`GROUP BY`, subqueries, secondary indexes in
the planner (M5), transactions (M4), WAL (M3), `ALTER TABLE`. Single table per query.

## 3. Architecture

```
SQL text
  │  crates/sql
  ▼
Lexer ──► Token stream ──► Pratt parser ──► AST (Statement)
                                              │  crates/sql::binder + crates/catalog
                                              ▼
                                       Binder / analyzer ──► bound plan (typed)
                                              │  crates/executor
                                              ▼
                              Volcano operators over the M1 B+-tree ──► ResultSet
                                              ▲
                                       crates/engine: Database / Session / execute(sql)
```

Row storage reuses M1: each table is a **B+-tree** keyed by its primary key (or an
auto-`RowId` when no PK), values are **encoded tuples**. The catalog is itself stored in the
same file via M1.

### 3.1 `crates/sql` — lexer, parser, AST

- **Lexer:** hand-written; emits `Token`s (keywords, identifiers, string/number/bool
  literals, punctuation, operators). Case-insensitive keywords; single-quoted string literals
  with `''` escaping.
- **Pratt parser:** precedence-climbing expression parser (handles `OR < AND < NOT <
  comparison < +/- < *//` and unary minus) plus recursive-descent statement parsing. No
  `sqlparser` crate.
- **AST:** `Statement` enum (`CreateTable`, `DropTable`, `Insert`, `Select`, `Update`,
  `Delete`) and `Expr` enum (`Literal`, `Column`, `BinaryOp`, `UnaryOp`, `IsNull`).

### 3.2 `crates/catalog` — schema as system tables

- System tables bootstrapped on first open, stored via M1 B+-trees:
  `ferro_tables(table_id, name, root_page, next_rowid)` and
  `ferro_columns(table_id, ordinal, name, type, not_null, is_pk)`.
- API: `create_table(name, columns) -> TableId`, `drop_table(name)`, `get_table(name) ->
  Option<TableSchema>`, `list_tables()`. `TableSchema` carries column names/types/flags and
  the table's B+-tree root page + rowid counter.
- The catalog root page id lives in the M1 `MetaPage` (extends M1's meta; backward-compatible
  since M1 files have no user tables).

### 3.3 Tuple encoding (`crates/catalog::tuple` or `crates/storage`)

- A row is encoded to bytes as: `[null-bitmap][value₀][value₁]…` in column order. Values:
  `INTEGER`→8-byte LE, `REAL`→8-byte LE bits, `BOOLEAN`→1 byte, `TEXT`→`[len:u32][utf8]`.
  `NULL` marked in the bitmap (value bytes omitted).
- The primary-key column's **order-preserving** encoding (M1 `encoding`) is the B+-tree key;
  the full tuple is the B+-tree value. Non-PK tables key by an auto-incrementing `RowId`.

### 3.4 `crates/sql::binder` — name/type resolution

- Resolves table + column names against the catalog, assigns column ordinals, type-checks
  every `Expr` (returns `TypeError` on mismatch, e.g. `TEXT > INTEGER`), desugars `SELECT *`,
  validates `INSERT` arity/types and `NOT NULL`. Output is a **bound** statement referencing
  columns by ordinal with known types — the executor never re-parses names.

### 3.5 `crates/executor` — volcano operators

- Iterator model: each operator implements `next() -> Result<Option<Tuple>>`.
- Operators: `SeqScan` (walk the table B+-tree via M1 `scan`), `Filter` (evaluate bound
  predicate, three-valued logic → keep only `TRUE`), `Project`, `Sort` (in-memory for M2,
  `ORDER BY`), `Limit`, `Insert`, `Update`, `Delete`.
- **Expression evaluator:** a typed interpreter over bound `Expr` producing a `Value`, with
  SQL `NULL` propagation.

### 3.6 `crates/engine` + REPL

- `Database::open(path)` wires catalog + storage; `Session::execute(sql) -> ResultSet`
  (parse → bind → plan → execute). `ResultSet` = column names + typed rows, or an affected-row
  count for DML.
- `ferrodb <file.db>` REPL: multiline SQL (`;` terminates), `.tables`, `.schema <t>`,
  ASCII result-table rendering, timing.

## 4. Data flow example

`SELECT name FROM users WHERE age > 26`:
lex → `[SELECT][name][FROM][users][WHERE][age][>][26]` → parse → `Select{ cols:[name],
from:"users", where: age > 26 }` → bind (users.age is col#2 INTEGER, 26 INTEGER → OK; name is
col#1 TEXT) → plan `Project([1], Filter(age>26, SeqScan(users)))` → execute: SeqScan pulls
tuples from the B+-tree, Filter evaluates `age > 26`, Project keeps `name` → ResultSet.

## 5. Testing strategy

- **Lexer/parser unit tests:** token streams + AST shapes for each statement; precedence
  (`a OR b AND c` ⇒ `a OR (b AND c)`); error cases (unterminated string, unexpected token).
- **Binder tests:** unknown table/column, type mismatch, `SELECT *` expansion, `NOT NULL`
  violation, `INSERT` arity.
- **Tuple codec:** round-trip every type incl. `NULL`, order-preservation of PK keys.
- **SQL logic tests (end-to-end):** `sqllogictest`-style `.test` files — run SQL, compare
  result rows. Covers CREATE/INSERT/SELECT/WHERE/ORDER BY/LIMIT/UPDATE/DELETE, persisted +
  reopened.
- **Property test:** random INSERT/DELETE workload vs. an in-memory `Vec<Row>` model, asserting
  `SELECT * ORDER BY pk` matches.

## 6. Milestone decomposition (tasks for the M2 plan)

Bottom-up, each an independently testable deliverable:

1. `crates/sql`: token types + **lexer**.
2. `crates/sql`: **AST** + **Pratt expression parser**.
3. `crates/sql`: **statement parser** (CREATE/INSERT/SELECT/UPDATE/DELETE/DROP).
4. Tuple **encode/decode** (all types + null bitmap).
5. `crates/catalog`: system tables + `create/drop/get/list` over M1.
6. `crates/sql::binder`: name/type resolution → bound statements.
7. `crates/executor`: `SeqScan` + `Insert` + expression evaluator (CREATE/INSERT/SELECT * work).
8. `crates/executor`: `Filter` + `Project` (`WHERE`, column lists).
9. `crates/executor`: `Sort` + `Limit` (`ORDER BY`, `LIMIT/OFFSET`).
10. `crates/executor`: `Update` + `Delete`.
11. `crates/engine`: `Database`/`Session`/`execute` + `ResultSet`.
12. `ferrodb` REPL + SQL-logic-test harness + CI update.

## 7. Success criteria

- [ ] The three statements in §1 run end-to-end, persisted and survive reopen.
- [ ] `cargo test --workspace` green incl. SQL-logic tests + binder + parser suites.
- [ ] Type errors and unknown names are rejected with clear messages, not panics.
- [ ] Catalog stored in-file; M1 `.db` files still open (backward-compatible meta).

## 8. Open questions for review

1. **PK requirement:** require every table to have a single-column PK in M2 (simpler keying),
   or support PK-less tables with a hidden `RowId` from the start? Default: **support both**
   (RowId fallback) — small effort, avoids a limitation.
2. **REPL binary name:** the master spec calls the SQL REPL `ferrodb` (vs. M1's `ferrodb-kv`).
   Keep both binaries, or have `ferrodb` supersede `ferrodb-kv`? Default: **keep both** —
   `ferrodb-kv` stays as the raw-engine demo, `ferrodb` is the SQL shell.
3. **Sort memory:** M2 `ORDER BY` sorts in memory (fine for the demo). External merge sort is
   deferred to M5 with the optimizer. OK?
