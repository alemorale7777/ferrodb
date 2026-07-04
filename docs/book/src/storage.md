# Storage: the pager

Every database is ultimately a program that reads and writes a file in fixed-size blocks. ferrodb's
storage layer is three components stacked on top of a single file: the **disk manager**, the
**buffer pool**, and **slotted pages**.

## The disk manager

The database is one file divided into fixed **4 KiB pages**, addressed by a `PageId` (a `u32` page
number). The disk manager does three things: allocate a new page (extend the file), read a page by
id, and write a page by id.

Every page carries a **CRC32C checksum** in its header. On every read the checksum is recomputed and
compared; a mismatch means the page is torn or corrupt and the read fails loudly rather than
returning garbage. This is the database's first line of defence against a half-written page.

Crucially, the disk manager does not talk to `std::fs::File` directly — it holds a
`Box<dyn Blob>`, a small trait (`read`/`write`/`seek`/`set_len`/`sync`/`len`). The native
implementation is a `File`; an in-memory `Vec<u8>` implementation (`MemBlob`) backs the WASM build.
Everything above this line is oblivious to which one is in use.

## The buffer pool

Reading a page from disk on every access would be ruinous, so the buffer pool is an in-memory cache
of pages held in a fixed set of **frames** (256 by default). Callers `fetch(page_id)` to pin a page
into a frame and get a mutable handle; they `unpin` it when done.

When every frame is occupied and a new page is needed, one must be evicted. The buffer pool uses
**clock-sweep** eviction: frames are arranged in a ring with a reference bit; the "clock hand"
sweeps forward, clearing set bits and evicting the first frame it finds with a clear bit and no
pins. This approximates least-recently-used without the bookkeeping of true LRU.

Two rules make the pool safe for durability (Chapter 4):

- A **pinned** page is never evicted — someone is using it.
- A **dirty** page (modified since it was read) must be written back before its frame is reused, and
  under the no-steal policy an *uncommitted* dirty page is never written back at all.

## Slotted pages

A 4 KiB page needs to hold a variable number of variable-length items — rows, or B+-tree entries.
The standard layout for this is a **slotted page**: a header at the front, a **slot directory**
growing downward from just after the header, and the **cells** (the actual bytes) growing upward
from the end. Each slot is a `(offset, length)` pointer into the cell area.

```
+--------+------+------+-----      -----+--------+--------+
| header | slot | slot | ...  free  ... |  cell  |  cell  |
+--------+------+------+-----      -----+--------+--------+
          \----- directory ----->       <---- cells -----/
```

This layout has two properties the layers above depend on. First, an item can be **found in O(1)**
by slot number without scanning the page. Second, an item can grow, shrink, or be deleted by
rewriting only its cell and slot — the other items do not move, so their slot numbers stay stable.
Free space is tracked so the page knows when it is full and must split.

With these three pieces — checksummed pages, a caching pinnable buffer pool, and a slotted page
layout — we have everything the B+-tree needs.
