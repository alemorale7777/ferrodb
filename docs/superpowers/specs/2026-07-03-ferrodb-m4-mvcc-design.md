# ferrodb Milestone 4 — MVCC Transactions (Design Spec)

> **Status:** Draft for review · **Date:** 2026-07-03 · Builds on M1–M3.

## 1. Goal

Multi-statement transactions with **snapshot isolation**: `BEGIN` / `COMMIT` / `ROLLBACK`,
concurrent transactions that each see a consistent snapshot, readers that never block writers,
first-updater-wins write-conflict detection, and `VACUUM` to reclaim dead versions.

Concurrency is **logical**: the engine stays single-threaded, but multiple transactions are open
at once and interleaved by the caller (`begin` → `execute_in` → `commit`/`rollback`). That is
enough to demonstrate — and test deterministically — every MVCC property.

## 2. Model — multi-version rows, no in-place updates

Each row key (primary key, or a hidden row id) maps to a **version chain**: a list of versions,
each carrying a header `(xmin, xmax, flags)`:

- `xmin` — the transaction that created the version.
- `xmax` — the transaction that deleted/superseded it (`0` = live).
- `flags` — `xmin_committed`, `xmax_committed` **hint bits** (see §4).

`INSERT` appends a version with `xmin = me, xmax = 0`. `DELETE` sets the visible version's
`xmax = me`. `UPDATE` = delete-old + append-new. **Nothing is ever overwritten in place**, which
is what makes both rollback and crash recovery trivial.

## 3. Snapshots & visibility

A transaction captures a **snapshot** at `BEGIN`: `{ xmax: next_txn, active: {in-progress txn ids} }`.
A version `v` is **visible** to snapshot `S` iff:

- **created-visible:** `v.xmin == S.self` (own write), or `v.xmin` is *committed as of S* —
  `xmin_committed && v.xmin ∉ S.active && v.xmin < S.xmax`; **and**
- **not deleted-visible:** it is *not* the case that `v.xmax == S.self`, or that `v.xmax` is a
  committed deletion as of S (`xmax_committed && v.xmax ∉ S.active && v.xmax < S.xmax`).

At most one version per key is visible to any snapshot. Readers evaluate this purely from the
stored versions + their own snapshot — they never block writers.

## 4. Commit status: in-memory manager + persisted hint bits

A `TxnManager` tracks the status (in-progress / committed / aborted) of live transactions in
memory. For **durability across restart**, the source of truth is the per-version **hint bits**:
on `COMMIT`, for every version the transaction touched (its write-set) we set `xmin_committed` /
`xmax_committed` and persist the pages. So after reopen, a version is committed iff its hint bit
is set — no separate commit log needed. A version whose creator never set `xmin_committed`
(crashed/aborted) is simply invisible.

- **COMMIT(T):** set hint bits for T's write-set, mark T committed, then WAL-commit + force (M3).
- **ROLLBACK(T):** mark T aborted. T's created versions keep `xmin_committed = false` → invisible;
  T's `xmax = T` stamps stay `xmax_committed = false` → ignored (target stays live). No undo.
- **Write conflict:** updating/deleting a version whose `xmax` is set by a *committed* or
  *still-active* other transaction → abort with a serialization error (first-updater-wins). An
  `xmax` from an *aborted* transaction is ignored.

`next_txn` is persisted under a reserved catalog key (`\x00next_txn`); table-name keys never start
with `NUL`, and `list_tables` skips reserved keys.

## 5. VACUUM

`VACUUM` scans every table and drops versions that are dead to **all** live snapshots: versions
created by an aborted transaction, and versions whose `xmax` is a committed deletion older than the
oldest active snapshot. Reclaims chain space (page reclamation itself still deferred).

## 6. API & integration

- `engine::txn::{TxnId, TxnManager, Snapshot}`.
- `engine::mvcc` — version-chain encode/decode + the visibility predicate.
- `Database`: `begin() -> TxnId`, `execute_in(txn, sql) -> Output`, `commit_txn(txn)`,
  `rollback_txn(txn)`, `vacuum()`. `execute(sql)` becomes autocommit (`begin`+`execute_in`+commit)
  so M2/M3 behaviour and the crash tests are preserved.
- Storage keys stay PK/row-id-based; the value is now a version chain. (A PK point-lookup is still
  an O(n) scan — a PK index is an M5 optimizer concern.)

## 7. Testing

- **Snapshot isolation:** T1 inserts/updates uncommitted → invisible to a concurrent T2's snapshot;
  visible to a snapshot taken *after* T1 commits; T2's pre-existing snapshot still doesn't see it.
- **Rollback:** T1's changes vanish on `ROLLBACK`; a rolled-back delete leaves the row live.
- **Write conflict:** two transactions update the same row → the second to write aborts.
- **VACUUM:** dead versions removed; live data unaffected.
- **Regression:** all M1–M3 suites stay green; autocommit + crash-recovery tests unchanged.

## 8. Success criteria

- [ ] `BEGIN`/`COMMIT`/`ROLLBACK` with snapshot isolation, demonstrated by interleaved transactions.
- [ ] Write-conflict aborts; `VACUUM` reclaims dead versions.
- [ ] `cargo test --workspace` green; fmt + clippy clean; CI green.
