# MVCC transactions

Multiple transactions must run concurrently without corrupting each other's view of the data, and a
long read must not block a write. ferrodb achieves this with **multi-version concurrency control**
(MVCC) and **snapshot isolation**: writers create new versions instead of overwriting, and each
transaction reads from a consistent snapshot taken when it began.

## Version chains

Nothing is ever overwritten in place. Each key in the B+-tree maps not to a single row but to a
**version chain** — a list of versions, each stamped with:

- `xmin`: the transaction that created this version.
- `xmax`: the transaction that deleted it (or none).
- **hint bits**: whether `xmin` / `xmax` are known-committed.

The three write operations become:

- **INSERT** appends a new version with `xmin` = my transaction.
- **UPDATE** stamps the old version's `xmax` and appends a new version — delete-old + insert-new.
- **DELETE** stamps the visible version's `xmax` with a tombstone.

## Snapshots and visibility

At `BEGIN`, a transaction captures a **snapshot**: the set of transactions committed as of that
instant. To read a key, the engine walks its version chain and picks the one version **visible** to
the snapshot — created by a transaction the snapshot considers committed, and not yet deleted by one
it does. Because a reader only ever consults versions and its own fixed snapshot, **readers never
block writers and writers never block readers.**

## Commit truth without a separate log

How does a version know whether the transaction that wrote it committed? Many systems keep a
separate commit log (CLOG). ferrodb instead makes the **hint bits the persisted source of truth**:
when a transaction commits, its versions' hint bits are set to committed and written out. After a
restart, a committed version is simply visible; a version whose writer crashed or rolled back has no
committed hint bit and is invisible — there is no undo pass and no external log to consult.

## Write conflicts

If two concurrent transactions both try to update the same row, the second to arrive finds the
version already stamped with a live `xmax` from the other transaction and fails with a **write
conflict** — first-updater-wins. This is what keeps snapshot isolation from silently losing an
update.

## VACUUM

Version chains grow without bound as rows are updated. `VACUUM` walks the tables and reclaims
versions that are **dead to every live snapshot** — no current or future transaction could ever need
them — compacting the chains. It is the garbage collector that keeps MVCC from leaking space
forever.

Correctness here cannot be shown by a single-threaded test. ferrodb drives **interleaved
transactions** — begin A, begin B, write in A, read in B, commit, re-read — and asserts each sees
exactly the snapshot it should.
