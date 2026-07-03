# ferrodb Milestone 3 — WAL + Crash Recovery (Design Spec)

> **Status:** Draft for review · **Date:** 2026-07-03 · Builds on M1 storage + M2 SQL engine.

## 1. Goal

Make committed data survive a crash. The headline demo: kill the process mid-write (or between
the WAL commit and the data flush), restart, and every committed statement is intact and atomic —
no half-applied B+-tree split, no torn multi-page write.

## 2. Model — redo logging, no-steal, force-at-commit

We choose the simplest buffer policy that is provably recoverable, and document its trade-off:

- **No-steal:** a dirty page is never written to the data file before its transaction commits.
  Enforced by a buffer-pool mode that refuses to evict dirty pages.
- **Force-at-commit:** at commit we flush all of the transaction's dirty pages to the data file.
- **Redo WAL:** because uncommitted changes never reach disk (no-steal), recovery needs **no undo** —
  it only redoes committed transactions. We log **full-page after-images**.

A transaction = **one SQL statement** (autocommit). The WAL makes the multi-page commit-flush
**atomic**: a crash in the middle is repaired by replaying the committed page images.

**Trade-off (documented):** no-steal means a statement's dirty-page set must fit in the buffer pool
(256 pages). Real systems steal + undo (ARIES) to lift this; that's a later refinement. Force-at-
commit fsyncs every statement (slower than LSN-deferred flushing) but is simple and correct.

## 3. Mechanism

**WAL file** `<db>.wal`, records appended sequentially:
- `Update { txn: u64, page_id: u32, image: [u8; 4096] }` — full page after-image.
- `Commit { txn: u64 }`.

**Commit protocol** (per statement):
1. Run the statement (mutates pages in the buffer pool; `self.meta` in memory). No-steal keeps every
   dirty page in memory.
2. Append an `Update` for each dirty page **and** for the meta page (page 0), then a `Commit`.
3. **`fsync` the WAL** (durability point — WAL-before-data).
4. Flush the dirty data pages + meta page 0 to the data file; `fsync` the data file.
5. Truncate the WAL (its work is now durable in the data file).

**Abort** (statement error): discard the dirty buffer pages (reload committed images from disk) and
restore `self.meta` from a snapshot taken at statement start. Nothing was flushed, so this fully
rolls back.

**Recovery** (on `Database::open`, before reading the catalog):
1. Parse the WAL. A torn/partial tail record ends parsing (the crash point).
2. Collect the set of committed txn ids (those with a `Commit` record).
3. Replay every `Update` belonging to a committed txn into the data file, in log order; `fsync`.
4. Truncate the WAL.
   - No `Commit` present → replay nothing (the in-flight statement is aborted). Data stays at the
     last committed state (guaranteed consistent by no-steal).

## 4. Integration

- **`storage::wal::Wal`** — the log file + `append_update` / `append_commit` / `sync` / `reset` /
  `recover(&mut DiskManager)`.
- **`storage::buffer::BufferPool`** — add `set_no_steal(bool)` (eviction skips dirty pages),
  `dirty_frames() -> Vec<(PageId, Page)>`, `discard_dirty()` (reload committed images).
- **`storage::meta::MetaPage`** — derive `Clone`/`Copy` (statement-start snapshot for abort).
- **`engine::Database`** — owns a `Wal` + a txn counter; `open` recovers first; `execute` wraps each
  statement in begin → dispatch → (commit | rollback). `ferrodb-kv` (raw KV) is unchanged and
  non-transactional.

## 5. Testing

- **Redo after crash:** commit a statement to the WAL (fsync) but *skip the data flush* (simulated
  crash), drop the DB, reopen → recovery replays the WAL → the row is present.
- **Atomicity:** mutate a statement but never write its `Commit` (crash before WAL commit), drop,
  reopen → the statement's effect is absent; earlier committed data intact.
- **Torn tail:** a WAL ending mid-record is parsed up to the last whole record.
- Full M1/M2 suites stay green (no-steal + WAL are additive; `ferrodb-kv` path untouched).

## 6. Success criteria

- [ ] Simulated crash between WAL-commit and data-flush recovers to the committed state.
- [ ] Uncommitted statement leaves no trace after a crash.
- [ ] `cargo test --workspace` green; fmt + clippy clean; CI green.
