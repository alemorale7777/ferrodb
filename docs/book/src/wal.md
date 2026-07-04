# WAL & crash recovery

A database must survive a crash. If the machine loses power midway through a multi-page B+-tree
split, the file must not be left in a state where the tree is half-updated and unreadable. ferrodb
guarantees this with a **write-ahead log** (WAL) and a **no-steal** buffer policy.

## The two policies

Recovery strategy is defined by two questions about when dirty pages reach the data file:

- **Steal?** May an *uncommitted* transaction's dirty pages be written to the data file? ferrodb
  says **no** (no-steal). Uncommitted changes never touch the data file, so recovery never has to
  *undo* anything.
- **Force?** Must a committing transaction's pages be flushed to the data file *before* commit
  returns? ferrodb says **no** (no-force) for the data file, but it *does* force the **log**. The
  pages are logged and the log is fsynced at commit; the data file catches up lazily.

No-steal + log-force gives the simplest correct recovery: **redo-only**. There is nothing to undo,
and the only thing to redo is committed work that had not yet reached the data file.

## The commit protocol

Every statement is an autocommit transaction (until M4 adds explicit `BEGIN`). On commit:

1. Write the **after-images** of every page the transaction modified into the WAL.
2. Append a commit record.
3. **fsync the WAL** — now the transaction is durable, even though the data file is stale.
4. Only then may the dirty pages be written back to the data file (and they may be deferred).

The critical window is between step 3 and the data-file write. A crash there has the change durably
in the log but not in the data file — exactly the case recovery must repair.

## Recovery

On startup, ferrodb scans the WAL. For every transaction that has a commit record, it **replays**
the logged page after-images into the data file; transactions without a commit record are ignored,
so their partial effects vanish. Because the after-image is the *whole page*, replay is idempotent —
applying it twice is the same as once — which is what makes redo safe to run after any crash.

The upshot: a crash between the WAL commit and the data flush — the exact window that would otherwise
corrupt a multi-page split — is repaired on restart, and a statement that never committed leaves no
trace.

## Proving it without racing the OS

You cannot reliably test crash recovery by launching a process and `kill`-ing it at the right
microsecond. ferrodb instead uses **deterministic crash injection**: the test drives the engine to
the precise point between log-commit and data-flush, drops the in-memory state, reopens from the
file, and asserts the committed data is present and the uncommitted data is gone. The dangerous
window is reproduced on purpose, every run.
