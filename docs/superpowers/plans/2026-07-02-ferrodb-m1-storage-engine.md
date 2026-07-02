# ferrodb Milestone 1 — Storage Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the on-disk storage foundation of ferrodb — a page-based, buffer-pooled, crash-safe-ready B+-tree key/value store — as a standalone, property-tested Rust library plus a tiny `put/get/scan` CLI.

**Architecture:** Bottom-up. A `DiskManager` maps `PageId → 4 KiB page` in a single file. A `BufferPool` caches pages with clock-sweep eviction and pin/unpin. A `SlottedPage` view lays variable-length cells inside a page. A `BPlusTree` composes those into an ordered map (search / insert-with-split / range-scan / delete-with-merge), with a free list for reclaimed pages and CRC32C checksums on every page. No SQL, no WAL, no transactions yet — those are M2/M3/M4.

**Tech Stack:** Rust (stable), Cargo workspace. Core deps: `thiserror` (errors), `crc32fast` (checksums). Dev deps: `proptest` (property tests), `tempfile`. CLI dep: `rustyline`.

## Global Constraints

- Rust stable, edition **2021**. `cargo test --workspace` must stay green after every task.
- Page size is a single constant **`PAGE_SIZE = 4096`** in `storage::page`; never hard-code `4096` elsewhere.
- Page ids are **`PageId(u32)`**; a newtype, not a bare integer. `PageId(0)` is reserved for the meta page.
- Core `storage` crate may depend ONLY on `thiserror` and `crc32fast` (plus dev-deps). **No** third-party btree/storage/serialization crate — hand-rolled is the point.
- All multi-byte integers serialized to pages use **little-endian** via `to_le_bytes`/`from_le_bytes`.
- Every fallible operation returns `Result<T, StorageError>`; no `unwrap()`/`panic!` in library code paths except documented invariants.
- On-disk key ordering must equal logical key ordering: keys are compared as raw bytes (`memcmp`), so callers pass already order-preserving-encoded keys (integer encoding provided in Task 8).

---

## File Structure

- `Cargo.toml` — workspace manifest listing `crates/storage` and `crates/cli`.
- `crates/storage/src/lib.rs` — crate root, re-exports, `StorageError`.
- `crates/storage/src/page.rs` — `PAGE_SIZE`, `PageId`, `Page` (raw bytes + checksum helpers).
- `crates/storage/src/disk.rs` — `DiskManager` (file-backed page read/write/allocate).
- `crates/storage/src/meta.rs` — `MetaPage` (page 0: magic, version, free-list head, tree root).
- `crates/storage/src/freelist.rs` — free-page allocate/free on top of `DiskManager` + meta.
- `crates/storage/src/buffer.rs` — `BufferPool` (frames, pin/unpin, clock eviction, flush).
- `crates/storage/src/slotted.rs` — `SlottedPage` view (insert/get/delete/compact/iterate cells).
- `crates/storage/src/encoding.rs` — order-preserving key encoding for i64 / bytes.
- `crates/storage/src/btree/node.rs` — leaf & internal node read/write over a slotted page.
- `crates/storage/src/btree/tree.rs` — `BPlusTree` (search/insert/split/range/delete/merge).
- `crates/storage/src/btree/overflow.rs` — overflow page chains for large values.
- `crates/cli/src/main.rs` — `ferrodb-kv` REPL: `put k v`, `get k`, `scan [lo] [hi]`.

---

## Task 1: Workspace + storage crate skeleton + error type

**Files:**
- Create: `Cargo.toml` (workspace)
- Create: `crates/storage/Cargo.toml`
- Create: `crates/storage/src/lib.rs`

**Interfaces:**
- Produces: `storage::StorageError` (enum, `thiserror`), `storage::Result<T> = std::result::Result<T, StorageError>`.

- [ ] **Step 1: Write the workspace + crate manifests**

`Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/storage", "crates/cli"]
```

`crates/storage/Cargo.toml`:
```toml
[package]
name = "storage"
version = "0.1.0"
edition = "2021"

[dependencies]
thiserror = "1"
crc32fast = "1"

[dev-dependencies]
proptest = "1"
tempfile = "3"
```

- [ ] **Step 2: Write the failing test**

In `crates/storage/src/lib.rs`:
```rust
pub mod page;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("page {0} out of range")]
    PageOutOfRange(u32),
    #[error("checksum mismatch on page {0}")]
    BadChecksum(u32),
    #[error("page full")]
    PageFull,
    #[error("corrupt: {0}")]
    Corrupt(&'static str),
}

pub type Result<T> = std::result::Result<T, StorageError>;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_displays() {
        assert_eq!(StorageError::PageOutOfRange(7).to_string(), "page 7 out of range");
    }
}
```
Create an empty `crates/storage/src/page.rs` so the `pub mod page;` compiles.

- [ ] **Step 3: Run test to verify it passes (skeleton)**

Run: `cargo test -p storage error_displays`
Expected: PASS (1 passed).

- [ ] **Step 4: Commit**
```bash
git add Cargo.toml crates/storage
git commit -m "feat(storage): workspace + storage crate skeleton with StorageError"
```

---

## Task 2: Page abstraction + CRC32C checksum

**Files:**
- Modify: `crates/storage/src/page.rs`

**Interfaces:**
- Consumes: `storage::{Result, StorageError}`.
- Produces:
  - `pub const PAGE_SIZE: usize = 4096;`
  - `pub struct PageId(pub u32)` — `Copy`, `Eq`, `Ord`, `Hash`.
  - `pub struct Page { bytes: [u8; PAGE_SIZE] }` with `new_zeroed()`, `data(&self)->&[u8]`, `data_mut(&mut self)->&mut [u8]`, `as_bytes(&self)->&[u8; PAGE_SIZE]`, `from_bytes([u8;PAGE_SIZE])`.
  - Checksum lives in the **last 4 bytes** of the page: `pub const PAGE_DATA_SIZE: usize = PAGE_SIZE - 4;`. `data()`/`data_mut()` expose only the first `PAGE_DATA_SIZE` bytes. `compute_checksum(&mut self)` writes CRC32C of `data()` into the trailer; `verify_checksum(&self) -> bool`.

- [ ] **Step 1: Write the failing test**
```rust
use crate::page::*;

#[test]
fn checksum_roundtrip_detects_corruption() {
    let mut p = Page::new_zeroed();
    p.data_mut()[0..3].copy_from_slice(b"abc");
    p.compute_checksum();
    assert!(p.verify_checksum());
    // corrupt one data byte -> checksum no longer matches
    p.data_mut()[1] = b'X';
    assert!(!p.verify_checksum());
}

#[test]
fn data_excludes_checksum_trailer() {
    assert_eq!(Page::new_zeroed().data().len(), PAGE_DATA_SIZE);
    assert_eq!(PAGE_DATA_SIZE, PAGE_SIZE - 4);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage checksum_roundtrip_detects_corruption`
Expected: FAIL — `Page` / `PAGE_SIZE` not found.

- [ ] **Step 3: Write minimal implementation**
```rust
pub const PAGE_SIZE: usize = 4096;
pub const PAGE_DATA_SIZE: usize = PAGE_SIZE - 4; // last 4 bytes = CRC32C trailer

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PageId(pub u32);

#[derive(Clone)]
pub struct Page {
    bytes: [u8; PAGE_SIZE],
}

impl Page {
    pub fn new_zeroed() -> Self { Page { bytes: [0u8; PAGE_SIZE] } }
    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Self { Page { bytes } }
    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] { &self.bytes }
    pub fn data(&self) -> &[u8] { &self.bytes[..PAGE_DATA_SIZE] }
    pub fn data_mut(&mut self) -> &mut [u8] { &mut self.bytes[..PAGE_DATA_SIZE] }

    pub fn compute_checksum(&mut self) {
        let sum = crc32fast::hash(&self.bytes[..PAGE_DATA_SIZE]);
        self.bytes[PAGE_DATA_SIZE..].copy_from_slice(&sum.to_le_bytes());
    }
    pub fn verify_checksum(&self) -> bool {
        let stored = u32::from_le_bytes(self.bytes[PAGE_DATA_SIZE..].try_into().unwrap());
        stored == crc32fast::hash(&self.bytes[..PAGE_DATA_SIZE])
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/storage/src/page.rs
git commit -m "feat(storage): fixed-size Page with CRC32C checksum trailer"
```

---

## Task 3: Disk manager (file-backed page I/O + allocate)

**Files:**
- Create: `crates/storage/src/disk.rs`
- Modify: `crates/storage/src/lib.rs` (add `pub mod disk;`)

**Interfaces:**
- Consumes: `page::{Page, PageId, PAGE_SIZE}`, `Result`, `StorageError`.
- Produces: `pub struct DiskManager` with:
  - `open(path: impl AsRef<Path>) -> Result<DiskManager>` (create if absent).
  - `num_pages(&self) -> u32` (file length / PAGE_SIZE).
  - `read_page(&mut self, id: PageId) -> Result<Page>` (verifies checksum; `PageOutOfRange` if id ≥ num_pages).
  - `write_page(&mut self, id: PageId, page: &mut Page) -> Result<()>` (calls `compute_checksum` then writes at `id*PAGE_SIZE`; extends file if needed).
  - `allocate_page(&mut self) -> Result<PageId>` (appends a fresh zeroed page, returns its id).
  - `sync(&mut self) -> Result<()>` (`file.sync_all()`).

- [ ] **Step 1: Write the failing test**
```rust
use storage::disk::DiskManager;
use storage::page::{Page, PageId};

#[test]
fn allocate_write_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut dm = DiskManager::open(&path).unwrap();
    let id = dm.allocate_page().unwrap();
    assert_eq!(id, PageId(0));
    let mut p = Page::new_zeroed();
    p.data_mut()[0..5].copy_from_slice(b"hello");
    dm.write_page(id, &mut p).unwrap();
    dm.sync().unwrap();

    let got = dm.read_page(id).unwrap();
    assert_eq!(&got.data()[0..5], b"hello");
    assert!(got.verify_checksum());
}

#[test]
fn read_out_of_range_errs() {
    let dir = tempfile::tempdir().unwrap();
    let mut dm = DiskManager::open(dir.path().join("t.db")).unwrap();
    assert!(matches!(dm.read_page(PageId(0)),
        Err(storage::StorageError::PageOutOfRange(0))));
}
```
Put integration tests in `crates/storage/tests/disk.rs`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test disk`
Expected: FAIL — `disk` module missing.

- [ ] **Step 3: Write minimal implementation**
```rust
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use crate::page::{Page, PageId, PAGE_SIZE};
use crate::{Result, StorageError};

pub struct DiskManager { file: File, num_pages: u32 }

impl DiskManager {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(DiskManager { file, num_pages: (len / PAGE_SIZE as u64) as u32 })
    }
    pub fn num_pages(&self) -> u32 { self.num_pages }

    pub fn read_page(&mut self, id: PageId) -> Result<Page> {
        if id.0 >= self.num_pages { return Err(StorageError::PageOutOfRange(id.0)); }
        self.file.seek(SeekFrom::Start(id.0 as u64 * PAGE_SIZE as u64))?;
        let mut buf = [0u8; PAGE_SIZE];
        self.file.read_exact(&mut buf)?;
        let page = Page::from_bytes(buf);
        if !page.verify_checksum() { return Err(StorageError::BadChecksum(id.0)); }
        Ok(page)
    }

    pub fn write_page(&mut self, id: PageId, page: &mut Page) -> Result<()> {
        page.compute_checksum();
        self.file.seek(SeekFrom::Start(id.0 as u64 * PAGE_SIZE as u64))?;
        self.file.write_all(page.as_bytes())?;
        if id.0 + 1 > self.num_pages { self.num_pages = id.0 + 1; }
        Ok(())
    }

    pub fn allocate_page(&mut self) -> Result<PageId> {
        let id = PageId(self.num_pages);
        let mut zero = Page::new_zeroed();
        self.write_page(id, &mut zero)?;
        Ok(id)
    }

    pub fn sync(&mut self) -> Result<()> { self.file.sync_all()?; Ok(()) }
}
```
Add `pub mod disk;` to `lib.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): file-backed DiskManager with checksummed page I/O"
```

---

## Task 4: Meta page (page 0)

**Files:**
- Create: `crates/storage/src/meta.rs`
- Modify: `crates/storage/src/lib.rs` (`pub mod meta;`)

**Interfaces:**
- Consumes: `page::{Page, PageId}`, `Result`, `StorageError`.
- Produces: `pub struct MetaPage { pub magic: u32, pub version: u16, pub free_list_head: Option<PageId>, pub tree_root: Option<PageId> }` with `MAGIC = 0xFE44_0DB0`, `encode(&self) -> Page`, `decode(&Page) -> Result<MetaPage>`. `None` page ids serialize as sentinel `u32::MAX`.

- [ ] **Step 1: Write the failing test**
```rust
use storage::meta::MetaPage;
use storage::page::PageId;

#[test]
fn meta_encode_decode_roundtrip() {
    let m = MetaPage { magic: MetaPage::MAGIC, version: 1,
        free_list_head: None, tree_root: Some(PageId(5)) };
    let page = m.encode();
    let back = MetaPage::decode(&page).unwrap();
    assert_eq!(back.tree_root, Some(PageId(5)));
    assert_eq!(back.free_list_head, None);
    assert_eq!(back.magic, MetaPage::MAGIC);
}

#[test]
fn meta_rejects_bad_magic() {
    let mut m = MetaPage { magic: 0, version: 1, free_list_head: None, tree_root: None };
    let page = m.encode();
    assert!(MetaPage::decode(&page).is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test meta`
Expected: FAIL — `meta` missing.

- [ ] **Step 3: Write minimal implementation**
```rust
use crate::page::{Page, PageId};
use crate::{Result, StorageError};

const NIL: u32 = u32::MAX;

pub struct MetaPage {
    pub magic: u32,
    pub version: u16,
    pub free_list_head: Option<PageId>,
    pub tree_root: Option<PageId>,
}

impl MetaPage {
    pub const MAGIC: u32 = 0xFE44_0DB0;

    fn opt(id: Option<PageId>) -> u32 { id.map(|p| p.0).unwrap_or(NIL) }
    fn unopt(v: u32) -> Option<PageId> { if v == NIL { None } else { Some(PageId(v)) } }

    pub fn encode(&self) -> Page {
        let mut p = Page::new_zeroed();
        let d = p.data_mut();
        d[0..4].copy_from_slice(&self.magic.to_le_bytes());
        d[4..6].copy_from_slice(&self.version.to_le_bytes());
        d[6..10].copy_from_slice(&Self::opt(self.free_list_head).to_le_bytes());
        d[10..14].copy_from_slice(&Self::opt(self.tree_root).to_le_bytes());
        p
    }

    pub fn decode(p: &Page) -> Result<MetaPage> {
        let d = p.data();
        let magic = u32::from_le_bytes(d[0..4].try_into().unwrap());
        if magic != Self::MAGIC { return Err(StorageError::Corrupt("bad meta magic")); }
        Ok(MetaPage {
            magic,
            version: u16::from_le_bytes(d[4..6].try_into().unwrap()),
            free_list_head: Self::unopt(u32::from_le_bytes(d[6..10].try_into().unwrap())),
            tree_root: Self::unopt(u32::from_le_bytes(d[10..14].try_into().unwrap())),
        })
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): MetaPage (magic/version/free-list/root) encode+decode"
```

---

## Task 5: Free list (allocate reuses freed pages)

**Files:**
- Create: `crates/storage/src/freelist.rs`
- Modify: `crates/storage/src/lib.rs` (`pub mod freelist;`)

**Interfaces:**
- Consumes: `DiskManager`, `MetaPage`, `page::{Page, PageId}`.
- Produces: free functions operating on a `&mut DiskManager` + `&mut MetaPage`:
  - `alloc(dm, meta) -> Result<PageId>` — pop head of free list if present (reading the freed page's first 4 bytes as the next-free pointer), else `dm.allocate_page()`.
  - `free(dm, meta, id) -> Result<()>` — push `id` onto the free list (write old head into `id`'s first 4 bytes, set `meta.free_list_head = Some(id)`).

- [ ] **Step 1: Write the failing test**
```rust
use storage::disk::DiskManager;
use storage::meta::MetaPage;
use storage::freelist;
use storage::page::PageId;

#[test]
fn freed_page_is_reused_lifo() {
    let dir = tempfile::tempdir().unwrap();
    let mut dm = DiskManager::open(dir.path().join("t.db")).unwrap();
    let mut meta = MetaPage { magic: MetaPage::MAGIC, version: 1,
        free_list_head: None, tree_root: None };

    let a = freelist::alloc(&mut dm, &mut meta).unwrap(); // fresh
    let b = freelist::alloc(&mut dm, &mut meta).unwrap(); // fresh
    freelist::free(&mut dm, &mut meta, a).unwrap();
    let c = freelist::alloc(&mut dm, &mut meta).unwrap(); // should reuse `a`
    assert_eq!(c, a);
    assert_ne!(b, c);
    assert_eq!(meta.free_list_head, None); // list drained
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test freelist`
Expected: FAIL — `freelist` missing.

- [ ] **Step 3: Write minimal implementation**
```rust
use crate::disk::DiskManager;
use crate::meta::MetaPage;
use crate::page::{Page, PageId};
use crate::Result;

const NIL: u32 = u32::MAX;

pub fn alloc(dm: &mut DiskManager, meta: &mut MetaPage) -> Result<PageId> {
    match meta.free_list_head {
        Some(head) => {
            let page = dm.read_page(head)?;
            let next = u32::from_le_bytes(page.data()[0..4].try_into().unwrap());
            meta.free_list_head = if next == NIL { None } else { Some(PageId(next)) };
            Ok(head)
        }
        None => dm.allocate_page(),
    }
}

pub fn free(dm: &mut DiskManager, meta: &mut MetaPage, id: PageId) -> Result<()> {
    let mut page = Page::new_zeroed();
    let old = meta.free_list_head.map(|p| p.0).unwrap_or(NIL);
    page.data_mut()[0..4].copy_from_slice(&old.to_le_bytes());
    dm.write_page(id, &mut page)?;
    meta.free_list_head = Some(id);
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): LIFO free list for page reuse"
```

---

## Task 6: Buffer pool (pin/unpin + clock-sweep eviction)

**Files:**
- Create: `crates/storage/src/buffer.rs`
- Modify: `crates/storage/src/lib.rs` (`pub mod buffer;`)

**Interfaces:**
- Consumes: `DiskManager`, `page::{Page, PageId}`.
- Produces: `pub struct BufferPool` owning a `DiskManager` and `capacity` frames:
  - `new(dm: DiskManager, capacity: usize) -> BufferPool`.
  - `fetch(&mut self, id: PageId) -> Result<usize>` — returns a **frame index**; pins the frame (loads from disk on miss, evicting an unpinned frame via clock sweep, flushing it if dirty).
  - `frame(&self, i: usize) -> &Page` / `frame_mut(&mut self, i: usize) -> &mut Page`.
  - `mark_dirty(&mut self, i: usize)`.
  - `unpin(&mut self, i: usize)`.
  - `new_page(&mut self, id: PageId) -> Result<usize>` — bring a freshly-allocated id into a pinned dirty frame without reading disk.
  - `flush_all(&mut self) -> Result<()>`; `disk_mut(&mut self) -> &mut DiskManager`.

- [ ] **Step 1: Write the failing test**
```rust
use storage::disk::DiskManager;
use storage::buffer::BufferPool;
use storage::page::{Page, PageId};

#[test]
fn fetch_caches_and_evicts() {
    let dir = tempfile::tempdir().unwrap();
    let mut dm = DiskManager::open(dir.path().join("t.db")).unwrap();
    for _ in 0..3 { dm.allocate_page().unwrap(); } // pages 0,1,2 exist
    let mut bp = BufferPool::new(dm, 2); // only 2 frames

    let f0 = bp.fetch(PageId(0)).unwrap();
    bp.frame_mut(f0).data_mut()[0] = 42;
    bp.mark_dirty(f0);
    bp.unpin(f0);

    let f1 = bp.fetch(PageId(1)).unwrap(); bp.unpin(f1);
    // fetching page 2 must evict one of {0,1}; page 0 was dirty so it flushes
    let _f2 = bp.fetch(PageId(2)).unwrap();

    // re-read page 0 straight from disk: the 42 must have been flushed
    let p = bp.disk_mut().read_page(PageId(0)).unwrap();
    assert_eq!(p.data()[0], 42);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test buffer`
Expected: FAIL — `buffer` missing.

- [ ] **Step 3: Write minimal implementation**
```rust
use std::collections::HashMap;
use crate::disk::DiskManager;
use crate::page::{Page, PageId};
use crate::{Result, StorageError};

struct Frame { id: PageId, page: Page, pins: u32, dirty: bool, ref_bit: bool }

pub struct BufferPool {
    dm: DiskManager,
    frames: Vec<Frame>,
    table: HashMap<PageId, usize>,
    capacity: usize,
    clock: usize,
}

impl BufferPool {
    pub fn new(dm: DiskManager, capacity: usize) -> Self {
        BufferPool { dm, frames: Vec::with_capacity(capacity),
            table: HashMap::new(), capacity, clock: 0 }
    }
    pub fn disk_mut(&mut self) -> &mut DiskManager { &mut self.dm }
    pub fn frame(&self, i: usize) -> &Page { &self.frames[i].page }
    pub fn frame_mut(&mut self, i: usize) -> &mut Page { &mut self.frames[i].page }
    pub fn mark_dirty(&mut self, i: usize) { self.frames[i].dirty = true; }
    pub fn unpin(&mut self, i: usize) {
        if self.frames[i].pins > 0 { self.frames[i].pins -= 1; }
    }

    fn install(&mut self, id: PageId, page: Page) -> Result<usize> {
        if self.frames.len() < self.capacity {
            self.frames.push(Frame { id, page, pins: 1, dirty: false, ref_bit: true });
            let i = self.frames.len() - 1;
            self.table.insert(id, i);
            return Ok(i);
        }
        // clock sweep for an unpinned victim
        let n = self.frames.len();
        for _ in 0..(2 * n) {
            let i = self.clock;
            self.clock = (self.clock + 1) % n;
            if self.frames[i].pins > 0 { continue; }
            if self.frames[i].ref_bit { self.frames[i].ref_bit = false; continue; }
            if self.frames[i].dirty {
                let vid = self.frames[i].id;
                self.dm.write_page(vid, &mut self.frames[i].page)?;
            }
            self.table.remove(&self.frames[i].id);
            self.frames[i] = Frame { id, page, pins: 1, dirty: false, ref_bit: true };
            self.table.insert(id, i);
            return Ok(i);
        }
        Err(StorageError::Corrupt("buffer pool full of pinned pages"))
    }

    pub fn fetch(&mut self, id: PageId) -> Result<usize> {
        if let Some(&i) = self.table.get(&id) {
            self.frames[i].pins += 1;
            self.frames[i].ref_bit = true;
            return Ok(i);
        }
        let page = self.dm.read_page(id)?;
        self.install(id, page)
    }

    pub fn new_page(&mut self, id: PageId) -> Result<usize> {
        let i = self.install(id, Page::new_zeroed())?;
        self.frames[i].dirty = true;
        Ok(i)
    }

    pub fn flush_all(&mut self) -> Result<()> {
        for f in &mut self.frames {
            if f.dirty { self.dm.write_page(f.id, &mut f.page)?; f.dirty = false; }
        }
        self.dm.sync()
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): buffer pool with pin/unpin and clock-sweep eviction"
```

---

## Task 7: Slotted page layout

**Files:**
- Create: `crates/storage/src/slotted.rs`
- Modify: `crates/storage/src/lib.rs` (`pub mod slotted;`)

**Interfaces:**
- Consumes: `page::{Page, PAGE_DATA_SIZE}`, `Result`, `StorageError`.
- Produces: `pub struct SlottedPage<'a>(&'a mut Page)` — a view interpreting a page's data region as a slot directory + cells. Header (bytes 0..): `num_slots: u16`, `free_start: u16` (grows up from header), `free_end: u16` (grows down from `PAGE_DATA_SIZE`). Slot dir entries follow the header: each `(offset: u16, len: u16)`. Methods:
  - `init(&mut self)` — set an empty page.
  - `num_slots(&self) -> u16`.
  - `insert(&mut self, slot: u16, bytes: &[u8]) -> Result<()>` — insert a cell at slot index (shifting later slots), `PageFull` if no room.
  - `get(&self, slot: u16) -> &[u8]`.
  - `set(&mut self, slot: u16, bytes: &[u8]) -> Result<()>` — replace (delete+insert semantics).
  - `remove(&mut self, slot: u16)` — remove slot, shift the directory.
  - `free_space(&self) -> usize`.
  - `iter(&self) -> impl Iterator<Item = &[u8]>`.
  - `compact(&mut self)` — reclaim fragmentation by rewriting live cells.

- [ ] **Step 1: Write the failing test**
```rust
use storage::page::Page;
use storage::slotted::SlottedPage;

#[test]
fn slotted_insert_get_remove_keeps_order() {
    let mut page = Page::new_zeroed();
    let mut sp = SlottedPage::new(&mut page);
    sp.init();
    sp.insert(0, b"alpha").unwrap();
    sp.insert(1, b"gamma").unwrap();
    sp.insert(1, b"beta").unwrap(); // shift gamma to slot 2
    assert_eq!(sp.num_slots(), 3);
    assert_eq!(sp.get(0), b"alpha");
    assert_eq!(sp.get(1), b"beta");
    assert_eq!(sp.get(2), b"gamma");
    sp.remove(1);
    assert_eq!(sp.num_slots(), 2);
    assert_eq!(sp.get(1), b"gamma");
}

#[test]
fn slotted_reports_full() {
    let mut page = Page::new_zeroed();
    let mut sp = SlottedPage::new(&mut page);
    sp.init();
    let big = vec![7u8; 4000];
    assert!(sp.insert(0, &big).is_ok());
    assert!(matches!(sp.insert(1, &big), Err(storage::StorageError::PageFull)));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test slotted`
Expected: FAIL — `slotted` missing.

- [ ] **Step 3: Write minimal implementation**
```rust
use crate::page::Page;
use crate::{Result, StorageError};

const HDR: usize = 6;          // num_slots(2) + free_start(2) + free_end(2)
const SLOT: usize = 4;         // offset(2) + len(2)

pub struct SlottedPage<'a>(&'a mut Page);

impl<'a> SlottedPage<'a> {
    pub fn new(p: &'a mut Page) -> Self { SlottedPage(p) }

    pub fn init(&mut self) {
        let cap = self.0.data().len() as u16;
        self.set_u16(0, 0);          // num_slots
        self.set_u16(2, HDR as u16); // free_start
        self.set_u16(4, cap);        // free_end
    }

    fn set_u16(&mut self, at: usize, v: u16) {
        self.0.data_mut()[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn get_u16(&self, at: usize) -> u16 {
        u16::from_le_bytes(self.0.data()[at..at + 2].try_into().unwrap())
    }
    pub fn num_slots(&self) -> u16 { self.get_u16(0) }
    fn free_start(&self) -> u16 { self.get_u16(2) }
    fn free_end(&self) -> u16 { self.get_u16(4) }

    fn slot_off_len(&self, slot: u16) -> (usize, usize) {
        let base = HDR + slot as usize * SLOT;
        (self.get_u16(base) as usize, self.get_u16(base + 2) as usize)
    }

    pub fn free_space(&self) -> usize {
        (self.free_end() as isize - self.free_start() as isize).max(0) as usize
    }

    pub fn get(&self, slot: u16) -> &[u8] {
        let (off, len) = self.slot_off_len(slot);
        &self.0.data()[off..off + len]
    }

    pub fn insert(&mut self, slot: u16, bytes: &[u8]) -> Result<()> {
        let n = self.num_slots();
        let need = SLOT + bytes.len();
        if self.free_space() < need { return Err(StorageError::PageFull); }
        // place cell at the top of free space
        let new_end = self.free_end() as usize - bytes.len();
        self.0.data_mut()[new_end..new_end + bytes.len()].copy_from_slice(bytes);
        // shift slot directory entries >= slot right by one
        for s in (slot..n).rev() {
            let (o, l) = self.slot_off_len(s);
            let dst = HDR + (s as usize + 1) * SLOT;
            self.set_u16(dst, o as u16);
            self.set_u16(dst + 2, l as u16);
        }
        let base = HDR + slot as usize * SLOT;
        self.set_u16(base, new_end as u16);
        self.set_u16(base + 2, bytes.len() as u16);
        self.set_u16(0, n + 1);
        self.set_u16(2, (HDR + (n as usize + 1) * SLOT) as u16);
        self.set_u16(4, new_end as u16);
        Ok(())
    }

    pub fn remove(&mut self, slot: u16) {
        let n = self.num_slots();
        for s in slot..n - 1 {
            let (o, l) = self.slot_off_len(s + 1);
            let dst = HDR + s as usize * SLOT;
            self.set_u16(dst, o as u16);
            self.set_u16(dst + 2, l as u16);
        }
        self.set_u16(0, n - 1);
        self.set_u16(2, (HDR + (n as usize - 1) * SLOT) as u16);
        // note: cell bytes left as dead space until compact()
    }

    pub fn set(&mut self, slot: u16, bytes: &[u8]) -> Result<()> {
        self.remove(slot);
        self.insert(slot, bytes)
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u8]> + '_ {
        (0..self.num_slots()).map(move |s| self.get(s))
    }

    pub fn compact(&mut self) {
        let cells: Vec<Vec<u8>> = self.iter().map(|c| c.to_vec()).collect();
        self.init();
        for (i, c) in cells.iter().enumerate() {
            let _ = self.insert(i as u16, c);
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): slotted-page layout (insert/get/remove/compact)"
```

---

## Task 8: Order-preserving key encoding

**Files:**
- Create: `crates/storage/src/encoding.rs`
- Modify: `crates/storage/src/lib.rs` (`pub mod encoding;`)

**Interfaces:**
- Produces:
  - `pub fn encode_i64(v: i64) -> [u8; 8]` — flips the sign bit so unsigned big-endian byte order equals signed integer order.
  - `pub fn decode_i64(b: &[u8]) -> i64`.
  - `pub fn encode_bytes(v: &[u8]) -> Vec<u8>` — identity (byte order already lexicographic); provided for symmetry.

- [ ] **Step 1: Write the failing test**
```rust
use storage::encoding::{encode_i64, decode_i64};

#[test]
fn i64_encoding_is_order_preserving() {
    let mut vals = [-5i64, 0, 3, i64::MIN, i64::MAX, -1, 1];
    let mut enc: Vec<[u8;8]> = vals.iter().map(|v| encode_i64(*v)).collect();
    vals.sort();
    enc.sort(); // sort by raw bytes
    let decoded: Vec<i64> = enc.iter().map(|b| decode_i64(b)).collect();
    assert_eq!(decoded, vals.to_vec());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test encoding`
Expected: FAIL — `encoding` missing.

- [ ] **Step 3: Write minimal implementation**
```rust
pub fn encode_i64(v: i64) -> [u8; 8] {
    // flip sign bit -> maps i64 order onto u64 big-endian byte order
    ((v as u64) ^ (1u64 << 63)).to_be_bytes()
}
pub fn decode_i64(b: &[u8]) -> i64 {
    let u = u64::from_be_bytes(b[..8].try_into().unwrap());
    (u ^ (1u64 << 63)) as i64
}
pub fn encode_bytes(v: &[u8]) -> Vec<u8> { v.to_vec() }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): order-preserving i64 key encoding"
```

---

## Task 9: B+-tree node view (leaf & internal over a slotted page)

**Files:**
- Create: `crates/storage/src/btree/mod.rs` (`pub mod node; pub mod tree;`)
- Create: `crates/storage/src/btree/node.rs`
- Modify: `crates/storage/src/lib.rs` (`pub mod btree;`)

**Interfaces:**
- Consumes: `SlottedPage`, `page::{Page, PageId}`.
- Produces: cell codecs on top of a slotted page (slot **0** reserved as the node header cell):
  - `pub enum NodeKind { Leaf, Internal }`.
  - `read_kind(page) -> NodeKind`, `read_next_leaf(page) -> Option<PageId>` (leaf sibling pointer, stored in header cell), `write_header(sp, kind, next_leaf)`.
  - Leaf cells encode `(key: Vec<u8>, value: Vec<u8>)`; internal cells encode `(key: Vec<u8>, child: PageId)` with a leading `left_child` in the header. Helpers: `leaf_encode/leaf_decode`, `internal_encode/internal_decode`, and `search_key(page, key) -> (slot_index, found: bool)` doing binary search over data slots (slots `1..num_slots`).

- [ ] **Step 1: Write the failing test**
```rust
use storage::page::{Page, PageId};
use storage::btree::node;

#[test]
fn leaf_header_and_cell_roundtrip() {
    let mut page = Page::new_zeroed();
    node::init_leaf(&mut page, Some(PageId(9)));
    node::leaf_put(&mut page, b"b".to_vec(), b"2".to_vec()).unwrap();
    node::leaf_put(&mut page, b"a".to_vec(), b"1".to_vec()).unwrap();
    // stored sorted by key
    assert_eq!(node::read_next_leaf(&page), Some(PageId(9)));
    let (slot, found) = node::search_key(&page, b"a");
    assert!(found);
    let (k, v) = node::leaf_at(&page, slot);
    assert_eq!((k.as_slice(), v.as_slice()), (b"a".as_ref(), b"1".as_ref()));
    let (_slot, found_missing) = node::search_key(&page, b"z");
    assert!(!found_missing);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test node`
Expected: FAIL — `btree::node` missing.

- [ ] **Step 3: Write minimal implementation**

Create `btree/mod.rs`:
```rust
pub mod node;
pub mod tree;
```
Create `btree/node.rs`:
```rust
use crate::page::{Page, PageId};
use crate::slotted::SlottedPage;
use crate::Result;

const NIL: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind { Leaf, Internal }

// Header cell (slot 0): [kind:u8][next_leaf:u32][left_child:u32]
fn write_header(page: &mut Page, kind: NodeKind, next_leaf: Option<PageId>, left: Option<PageId>) {
    let mut sp = SlottedPage::new(page);
    let mut hdr = [0u8; 9];
    hdr[0] = if kind == NodeKind::Leaf { 0 } else { 1 };
    hdr[1..5].copy_from_slice(&next_leaf.map(|p| p.0).unwrap_or(NIL).to_le_bytes());
    hdr[5..9].copy_from_slice(&left.map(|p| p.0).unwrap_or(NIL).to_le_bytes());
    sp.insert(0, &hdr).unwrap();
}

pub fn init_leaf(page: &mut Page, next_leaf: Option<PageId>) {
    { let mut sp = SlottedPage::new(page); sp.init(); }
    write_header(page, NodeKind::Leaf, next_leaf, None);
}
pub fn init_internal(page: &mut Page, left_child: PageId) {
    { let mut sp = SlottedPage::new(page); sp.init(); }
    write_header(page, NodeKind::Internal, None, Some(left_child));
}

pub fn read_kind(page: &Page) -> NodeKind {
    let sp = SlottedPage::new_ro(page);
    if sp.get(0)[0] == 0 { NodeKind::Leaf } else { NodeKind::Internal }
}
pub fn read_next_leaf(page: &Page) -> Option<PageId> {
    let sp = SlottedPage::new_ro(page);
    let v = u32::from_le_bytes(sp.get(0)[1..5].try_into().unwrap());
    if v == NIL { None } else { Some(PageId(v)) }
}
pub fn left_child(page: &Page) -> PageId {
    let sp = SlottedPage::new_ro(page);
    PageId(u32::from_le_bytes(sp.get(0)[5..9].try_into().unwrap()))
}

// Leaf cell: [klen:u16][key][value]
pub fn leaf_put(page: &mut Page, key: Vec<u8>, val: Vec<u8>) -> Result<()> {
    let (slot, found) = search_key(page, &key);
    let mut cell = Vec::with_capacity(2 + key.len() + val.len());
    cell.extend_from_slice(&(key.len() as u16).to_le_bytes());
    cell.extend_from_slice(&key);
    cell.extend_from_slice(&val);
    let mut sp = SlottedPage::new(page);
    if found { sp.set(slot, &cell) } else { sp.insert(slot, &cell) }
}
pub fn leaf_at(page: &Page, slot: u16) -> (Vec<u8>, Vec<u8>) {
    let sp = SlottedPage::new_ro(page);
    let cell = sp.get(slot);
    let klen = u16::from_le_bytes(cell[0..2].try_into().unwrap()) as usize;
    (cell[2..2 + klen].to_vec(), cell[2 + klen..].to_vec())
}

// Internal cell: [klen:u16][key][child:u32]
pub fn internal_put(page: &mut Page, key: Vec<u8>, child: PageId) -> Result<()> {
    let (slot, _found) = search_key(page, &key);
    let mut cell = Vec::new();
    cell.extend_from_slice(&(key.len() as u16).to_le_bytes());
    cell.extend_from_slice(&key);
    cell.extend_from_slice(&child.0.to_le_bytes());
    SlottedPage::new(page).insert(slot, &cell)
}
pub fn internal_at(page: &Page, slot: u16) -> (Vec<u8>, PageId) {
    let sp = SlottedPage::new_ro(page);
    let cell = sp.get(slot);
    let klen = u16::from_le_bytes(cell[0..2].try_into().unwrap()) as usize;
    let child = u32::from_le_bytes(cell[2 + klen..2 + klen + 4].try_into().unwrap());
    (cell[2..2 + klen].to_vec(), PageId(child))
}

pub fn key_of(page: &Page, slot: u16) -> Vec<u8> {
    let sp = SlottedPage::new_ro(page);
    let cell = sp.get(slot);
    let klen = u16::from_le_bytes(cell[0..2].try_into().unwrap()) as usize;
    cell[2..2 + klen].to_vec()
}
pub fn num_entries(page: &Page) -> u16 { SlottedPage::new_ro(page).num_slots() - 1 }

/// Binary search over data slots [1..num_slots). Returns (slot_index, found).
pub fn search_key(page: &Page, key: &[u8]) -> (u16, bool) {
    let n = SlottedPage::new_ro(page).num_slots();
    let (mut lo, mut hi) = (1u16, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        match key_of(page, mid).as_slice().cmp(key) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return (mid, true),
        }
    }
    (lo, false)
}
```
Add a read-only constructor to `slotted.rs`:
```rust
impl<'a> SlottedPage<'a> {
    pub fn new_ro(p: &'a Page) -> SlottedPageRef<'a> { SlottedPageRef(p) }
}
pub struct SlottedPageRef<'a>(&'a Page);
impl<'a> SlottedPageRef<'a> {
    fn get_u16(&self, at: usize) -> u16 {
        u16::from_le_bytes(self.0.data()[at..at+2].try_into().unwrap())
    }
    pub fn num_slots(&self) -> u16 { self.get_u16(0) }
    pub fn get(&self, slot: u16) -> &[u8] {
        let base = 6 + slot as usize * 4;
        let off = self.get_u16(base) as usize;
        let len = self.get_u16(base + 2) as usize;
        &self.0.data()[off..off + len]
    }
}
```
> Adjust `node.rs` calls from `SlottedPage::new_ro` to return `SlottedPageRef` (rename in `node.rs` accordingly). The header cell occupies slot 0, so data slots start at 1 and `num_entries = num_slots - 1`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): B+-tree node codecs + binary search over slots"
```

---

## Task 10: B+-tree insert + search + leaf split + root growth

**Files:**
- Create: `crates/storage/src/btree/tree.rs`

**Interfaces:**
- Consumes: `BufferPool`, `MetaPage`, `btree::node::*`, `page::PageId`.
- Produces: `pub struct BPlusTree<'a> { bp: &'a mut BufferPool, meta: &'a mut MetaPage }` with:
  - `open(bp, meta) -> BPlusTree` — creates an empty leaf root on first use (sets `meta.tree_root`).
  - `get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>`.
  - `insert(&mut self, key: &[u8], val: &[u8]) -> Result<()>` — descends to the leaf, inserts, and on `PageFull` **splits** the leaf, pushing a separator up; splits internal nodes recursively; grows a new root when the old root splits.

- [ ] **Step 1: Write the failing test** (`crates/storage/tests/btree.rs`)
```rust
use storage::disk::DiskManager;
use storage::buffer::BufferPool;
use storage::meta::MetaPage;
use storage::btree::tree::BPlusTree;
use storage::encoding::encode_i64;

fn fresh() -> (BufferPool, MetaPage) {
    let dir = tempfile::tempdir().unwrap();
    let dm = DiskManager::open(dir.path().join("t.db")).unwrap();
    std::mem::forget(dir); // keep temp file alive for the test process
    (BufferPool::new(dm, 64),
     MetaPage { magic: MetaPage::MAGIC, version: 1, free_list_head: None, tree_root: None })
}

#[test]
fn insert_and_get_many_forces_splits() {
    let (mut bp, mut meta) = fresh();
    {
        let mut t = BPlusTree::open(&mut bp, &mut meta);
        for i in 0..2000i64 {
            t.insert(&encode_i64(i), format!("v{i}").as_bytes()).unwrap();
        }
        for i in 0..2000i64 {
            assert_eq!(t.get(&encode_i64(i)).unwrap(),
                       Some(format!("v{i}").into_bytes()), "key {i}");
        }
        assert_eq!(t.get(&encode_i64(9999)).unwrap(), None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test btree insert_and_get_many_forces_splits`
Expected: FAIL — `btree::tree` missing.

- [ ] **Step 3: Write minimal implementation**
```rust
use crate::buffer::BufferPool;
use crate::meta::MetaPage;
use crate::page::PageId;
use crate::btree::node;
use crate::Result;

pub struct BPlusTree<'a> { bp: &'a mut BufferPool, meta: &'a mut MetaPage }

// A split result bubbling up: (separator_key, right_child_page)
type Split = Option<(Vec<u8>, PageId)>;

impl<'a> BPlusTree<'a> {
    pub fn open(bp: &'a mut BufferPool, meta: &'a mut MetaPage) -> Self {
        let mut t = BPlusTree { bp, meta };
        if t.meta.tree_root.is_none() {
            let id = t.bp.disk_mut().allocate_page().unwrap();
            let f = t.bp.new_page(id).unwrap();
            node::init_leaf(t.bp.frame_mut(f), None);
            t.bp.mark_dirty(f); t.bp.unpin(f);
            t.meta.tree_root = Some(id);
        }
        t
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut pid = self.meta.tree_root.unwrap();
        loop {
            let f = self.bp.fetch(pid)?;
            let page = self.bp.frame(f).clone();
            self.bp.unpin(f);
            match node::read_kind(&page) {
                node::NodeKind::Leaf => {
                    let (slot, found) = node::search_key(&page, key);
                    return Ok(if found { Some(node::leaf_at(&page, slot).1) } else { None });
                }
                node::NodeKind::Internal => { pid = self.child_for(&page, key); }
            }
        }
    }

    fn child_for(&self, page: &crate::page::Page, key: &[u8]) -> PageId {
        // entries are (sep_key, child) meaning "keys >= sep_key go to child"
        let (slot, found) = node::search_key(page, key);
        let n = node::num_entries(page);
        if n == 0 { return node::left_child(page); }
        // slot is insertion point in [1..=num_slots]; the child left of it
        let idx = if found { slot } else { slot.saturating_sub(1) };
        if idx < 1 { node::left_child(page) } else { node::internal_at(page, idx).1 }
    }

    pub fn insert(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        let root = self.meta.tree_root.unwrap();
        if let Some((sep, right)) = self.insert_rec(root, key, val)? {
            // root split -> new root
            let id = self.bp.disk_mut().allocate_page()?;
            let f = self.bp.new_page(id)?;
            node::init_internal(self.bp.frame_mut(f), root);
            node::internal_put(self.bp.frame_mut(f), sep, right)?;
            self.bp.mark_dirty(f); self.bp.unpin(f);
            self.meta.tree_root = Some(id);
        }
        Ok(())
    }

    fn insert_rec(&mut self, pid: PageId, key: &[u8], val: &[u8]) -> Result<Split> {
        let f = self.bp.fetch(pid)?;
        let kind = node::read_kind(self.bp.frame(f));
        match kind {
            node::NodeKind::Leaf => {
                let res = node::leaf_put(self.bp.frame_mut(f), key.to_vec(), val.to_vec());
                match res {
                    Ok(()) => { self.bp.mark_dirty(f); self.bp.unpin(f); Ok(None) }
                    Err(crate::StorageError::PageFull) => {
                        self.bp.unpin(f);
                        self.split_leaf(pid, key, val)
                    }
                    Err(e) => { self.bp.unpin(f); Err(e) }
                }
            }
            node::NodeKind::Internal => {
                let page = self.bp.frame(f).clone();
                self.bp.unpin(f);
                let child = self.child_for(&page, key);
                if let Some((sep, right)) = self.insert_rec(child, key, val)? {
                    let f2 = self.bp.fetch(pid)?;
                    let r = node::internal_put(self.bp.frame_mut(f2), sep.clone(), right);
                    match r {
                        Ok(()) => { self.bp.mark_dirty(f2); self.bp.unpin(f2); Ok(None) }
                        Err(crate::StorageError::PageFull) => {
                            self.bp.unpin(f2);
                            self.split_internal(pid, &sep, right)
                        }
                        Err(e) => { self.bp.unpin(f2); Err(e) }
                    }
                } else { Ok(None) }
            }
        }
    }

    fn split_leaf(&mut self, pid: PageId, key: &[u8], val: &[u8]) -> Result<Split> {
        // collect all entries + the new one, split in half
        let f = self.bp.fetch(pid)?;
        let page = self.bp.frame(f).clone();
        self.bp.unpin(f);
        let mut items: Vec<(Vec<u8>, Vec<u8>)> = (1..=node::num_entries(page.clone_ref()))
            .map(|s| node::leaf_at(&page, s)).collect();
        // NOTE: use a helper that returns entries; see below
        let (mut ks, ins) = (items, (key.to_vec(), val.to_vec()));
        match ks.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
            Ok(i) => ks[i] = ins, Err(i) => ks.insert(i, ins),
        }
        let mid = ks.len() / 2;
        let sep = ks[mid].0.clone();
        let right_id = self.bp.disk_mut().allocate_page()?;
        let old_next = node::read_next_leaf(&page);
        // rewrite left
        let lf = self.bp.fetch(pid)?;
        node::init_leaf(self.bp.frame_mut(lf), Some(right_id));
        for (k, v) in &ks[..mid] { node::leaf_put(self.bp.frame_mut(lf), k.clone(), v.clone())?; }
        self.bp.mark_dirty(lf); self.bp.unpin(lf);
        // write right
        let rf = self.bp.new_page(right_id)?;
        node::init_leaf(self.bp.frame_mut(rf), old_next);
        for (k, v) in &ks[mid..] { node::leaf_put(self.bp.frame_mut(rf), k.clone(), v.clone())?; }
        self.bp.mark_dirty(rf); self.bp.unpin(rf);
        Ok(Some((sep, right_id)))
    }

    fn split_internal(&mut self, pid: PageId, sep_in: &[u8], child_in: PageId) -> Result<Split> {
        let f = self.bp.fetch(pid)?;
        let page = self.bp.frame(f).clone();
        self.bp.unpin(f);
        let mut items: Vec<(Vec<u8>, PageId)> = (1..=node::num_entries(&page))
            .map(|s| node::internal_at(&page, s)).collect();
        match items.binary_search_by(|(k, _)| k.as_slice().cmp(sep_in)) {
            Ok(i) => items.insert(i, (sep_in.to_vec(), child_in)),
            Err(i) => items.insert(i, (sep_in.to_vec(), child_in)),
        }
        let leftmost = node::left_child(&page);
        let mid = items.len() / 2;
        let sep_up = items[mid].0.clone();
        let right_first_child = items[mid].1;
        let right_id = self.bp.disk_mut().allocate_page()?;
        // rewrite left node (entries before mid)
        let lf = self.bp.fetch(pid)?;
        node::init_internal(self.bp.frame_mut(lf), leftmost);
        for (k, c) in &items[..mid] { node::internal_put(self.bp.frame_mut(lf), k.clone(), *c)?; }
        self.bp.mark_dirty(lf); self.bp.unpin(lf);
        // right node: first child = right_first_child, entries after mid
        let rf = self.bp.new_page(right_id)?;
        node::init_internal(self.bp.frame_mut(rf), right_first_child);
        for (k, c) in &items[mid + 1..] { node::internal_put(self.bp.frame_mut(rf), k.clone(), *c)?; }
        self.bp.mark_dirty(rf); self.bp.unpin(rf);
        Ok(Some((sep_up, right_id)))
    }
}
```
> **Implementer note:** `node::num_entries` takes `&Page`; drop the `.clone_ref()` sketch above and pass `&page`. Add a convenience `node::leaf_entries(&Page) -> Vec<(Vec<u8>,Vec<u8>)>` and `node::internal_entries(&Page) -> Vec<(Vec<u8>,PageId)>` if it reads cleaner. The separator convention is **"keys ≥ separator go right"**; keep `child_for` consistent with it. This is the most intricate task — expect to iterate against the test, which exercises thousands of inserts across many splits and multiple tree levels.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage --test btree`
Expected: PASS (all 2000 keys retrievable).

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): B+-tree insert/get with leaf+internal split and root growth"
```

---

## Task 11: Range scan via leaf sibling pointers

**Files:**
- Modify: `crates/storage/src/btree/tree.rs`

**Interfaces:**
- Produces: `pub fn scan(&mut self, lo: Option<&[u8]>, hi: Option<&[u8]>) -> Result<Vec<(Vec<u8>, Vec<u8>)>>` — inclusive `lo`, exclusive `hi`; `None` means unbounded. Descends to the leaf holding `lo`, then walks `next_leaf` pointers collecting in-range entries.

- [ ] **Step 1: Write the failing test**
```rust
#[test]
fn range_scan_is_sorted_and_bounded() {
    let (mut bp, mut meta) = fresh();
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    for i in 0..500i64 { t.insert(&encode_i64(i), b"x").unwrap(); }
    let rows = t.scan(Some(&encode_i64(10)), Some(&encode_i64(20))).unwrap();
    let keys: Vec<i64> = rows.iter()
        .map(|(k,_)| storage::encoding::decode_i64(k)).collect();
    assert_eq!(keys, (10..20).collect::<Vec<_>>());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test btree range_scan_is_sorted_and_bounded`
Expected: FAIL — `scan` not defined.

- [ ] **Step 3: Write minimal implementation**
```rust
impl<'a> BPlusTree<'a> {
    fn leftmost_leaf_for(&mut self, lo: Option<&[u8]>) -> Result<PageId> {
        let mut pid = self.meta.tree_root.unwrap();
        loop {
            let f = self.bp.fetch(pid)?;
            let page = self.bp.frame(f).clone();
            self.bp.unpin(f);
            match node::read_kind(&page) {
                node::NodeKind::Leaf => return Ok(pid),
                node::NodeKind::Internal => {
                    pid = match lo { Some(k) => self.child_for(&page, k),
                                     None => node::left_child(&page) };
                }
            }
        }
    }

    pub fn scan(&mut self, lo: Option<&[u8]>, hi: Option<&[u8]>)
        -> Result<Vec<(Vec<u8>, Vec<u8>)>>
    {
        let mut out = Vec::new();
        let mut pid = self.leftmost_leaf_for(lo)?;
        loop {
            let f = self.bp.fetch(pid)?;
            let page = self.bp.frame(f).clone();
            self.bp.unpin(f);
            for s in 1..=node::num_entries(&page) {
                let (k, v) = node::leaf_at(&page, s);
                if let Some(l) = lo { if k.as_slice() < l { continue; } }
                if let Some(h) = hi { if k.as_slice() >= h { return Ok(out); } }
                out.push((k, v));
            }
            match node::read_next_leaf(&page) { Some(n) => pid = n, None => return Ok(out) }
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage --test btree`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): ordered range scan over leaf sibling chain"
```

---

## Task 12: Delete (leaf tombstone + property test vs BTreeMap)

**Files:**
- Modify: `crates/storage/src/btree/tree.rs`
- Create: `crates/storage/tests/btree_prop.rs`

**Interfaces:**
- Produces: `pub fn delete(&mut self, key: &[u8]) -> Result<bool>` — removes the key from its leaf, returning whether it existed. (M1 uses simple leaf-level delete without internal-node merge; rebalancing/merge is folded into M4 `VACUUM`. This keeps M1 finishable while remaining correct — the tree stays valid and searchable, it just may hold under-full leaves. Documented explicitly.)

- [ ] **Step 1: Write the failing test (property test against a model)**
```rust
use proptest::prelude::*;
use storage::disk::DiskManager;
use storage::buffer::BufferPool;
use storage::meta::MetaPage;
use storage::btree::tree::BPlusTree;
use storage::encoding::encode_i64;
use std::collections::BTreeMap;

proptest! {
    #[test]
    fn btree_matches_btreemap(ops in prop::collection::vec(
        (any::<i8>(), 0i64..64, 0u8..16), 0..400))
    {
        let dir = tempfile::tempdir().unwrap();
        let dm = DiskManager::open(dir.path().join("p.db")).unwrap();
        let mut bp = BufferPool::new(dm, 32);
        let mut meta = MetaPage { magic: MetaPage::MAGIC, version: 1,
            free_list_head: None, tree_root: None };
        let mut t = BPlusTree::open(&mut bp, &mut meta);
        let mut model: BTreeMap<i64, Vec<u8>> = BTreeMap::new();

        for (op, k, v) in ops {
            let key = encode_i64(k);
            if op % 3 == 0 {
                let had = t.delete(&key).unwrap();
                prop_assert_eq!(had, model.remove(&k).is_some());
            } else {
                let val = vec![v];
                t.insert(&key, &val).unwrap();
                model.insert(k, val);
            }
        }
        // full-scan equality
        let got: Vec<(i64, Vec<u8>)> = t.scan(None, None).unwrap().into_iter()
            .map(|(k, v)| (storage::encoding::decode_i64(&k), v)).collect();
        let want: Vec<(i64, Vec<u8>)> = model.into_iter().collect();
        prop_assert_eq!(got, want);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test btree_prop`
Expected: FAIL — `delete` not defined.

- [ ] **Step 3: Write minimal implementation**
```rust
impl<'a> BPlusTree<'a> {
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        let mut pid = self.meta.tree_root.unwrap();
        loop {
            let f = self.bp.fetch(pid)?;
            let kind = node::read_kind(self.bp.frame(f));
            match kind {
                node::NodeKind::Leaf => {
                    let (slot, found) = node::search_key(self.bp.frame(f), key);
                    if found {
                        crate::slotted::SlottedPage::new(self.bp.frame_mut(f)).remove(slot);
                        self.bp.mark_dirty(f);
                    }
                    self.bp.unpin(f);
                    return Ok(found);
                }
                node::NodeKind::Internal => {
                    let page = self.bp.frame(f).clone();
                    self.bp.unpin(f);
                    pid = self.child_for(&page, key);
                }
            }
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS (property test runs its default cases green).

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): leaf delete + proptest B+-tree vs BTreeMap model"
```

---

## Task 13: Overflow pages for large values

**Files:**
- Create: `crates/storage/src/btree/overflow.rs`
- Modify: `crates/storage/src/btree/mod.rs` (`pub mod overflow;`), `crates/storage/src/btree/node.rs` (value-indirection), `crates/storage/src/btree/tree.rs` (spill on insert, gather on get)

**Interfaces:**
- Produces:
  - `overflow::write_chain(bp, dm, bytes) -> Result<PageId>` — splits `bytes` across a linked chain of pages (`[next:u32][chunk...]`), returns the head.
  - `overflow::read_chain(bp, head) -> Result<Vec<u8>>`.
  - Leaf value cell gains a 1-byte flag: `0 = inline value`, `1 = overflow head PageId(u32)`. Values whose total cell would exceed **¼ of `PAGE_DATA_SIZE`** spill to a chain; `get` transparently gathers them.

- [ ] **Step 1: Write the failing test**
```rust
#[test]
fn large_values_roundtrip_via_overflow() {
    let (mut bp, mut meta) = fresh();
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    let big = vec![0xABu8; 20_000]; // ~5 pages
    t.insert(&encode_i64(1), &big).unwrap();
    t.insert(&encode_i64(2), b"small").unwrap();
    assert_eq!(t.get(&encode_i64(1)).unwrap(), Some(big));
    assert_eq!(t.get(&encode_i64(2)).unwrap(), Some(b"small".to_vec()));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test btree large_values_roundtrip_via_overflow`
Expected: FAIL — large value overflows a single page / no chaining.

- [ ] **Step 3: Write minimal implementation**

`btree/overflow.rs`:
```rust
use crate::buffer::BufferPool;
use crate::page::{Page, PageId, PAGE_DATA_SIZE};
use crate::Result;

const NIL: u32 = u32::MAX;
const CHUNK: usize = PAGE_DATA_SIZE - 4; // 4 bytes reserved for next ptr

pub fn write_chain(bp: &mut BufferPool, bytes: &[u8]) -> Result<PageId> {
    let mut next = NIL;
    let mut head = PageId(NIL);
    for chunk in bytes.chunks(CHUNK).collect::<Vec<_>>().into_iter().rev() {
        let id = bp.disk_mut().allocate_page()?;
        let f = bp.new_page(id)?;
        let d = bp.frame_mut(f).data_mut();
        d[0..4].copy_from_slice(&next.to_le_bytes());
        d[4..4 + chunk.len()].copy_from_slice(chunk);
        bp.mark_dirty(f); bp.unpin(f);
        next = id.0; head = id;
    }
    Ok(head)
}

pub fn read_chain(bp: &mut BufferPool, head: PageId, total_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(total_len);
    let mut cur = head.0;
    while cur != NIL && out.len() < total_len {
        let f = bp.fetch(PageId(cur))?;
        let d = bp.frame(f).data();
        let next = u32::from_le_bytes(d[0..4].try_into().unwrap());
        let take = (total_len - out.len()).min(CHUNK);
        out.extend_from_slice(&d[4..4 + take]);
        bp.unpin(f);
        cur = next;
    }
    Ok(out)
}
```
In `node.rs`, prefix the stored value with `[flag:u8]` and, when overflowed, store `[1][total_len:u32][head:u32]` instead of the inline bytes. In `tree.rs::insert`, before `leaf_put`, if `1 + key + value` exceeds `PAGE_DATA_SIZE/4`, call `overflow::write_chain` and store the indirection; in `get`, when the flag is `1`, call `overflow::read_chain`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): overflow page chains for large values"
```

---

## Task 14: Persistence across reopen (meta flush + durability of tree)

**Files:**
- Modify: `crates/storage/src/btree/tree.rs` (add `checkpoint`), `crates/storage/src/lib.rs` (re-exports)

**Interfaces:**
- Produces: `pub fn checkpoint(&mut self) -> Result<()>` on `BPlusTree` — flushes the buffer pool and writes the current `MetaPage` (with `tree_root`, `free_list_head`) to `PageId(0)`. A standalone helper `pub fn load_meta(bp: &mut BufferPool) -> Result<MetaPage>` reads page 0 if it exists, else returns a fresh meta. **Invariant:** page 0 is always the meta page; `DiskManager` on a brand-new file has 0 pages, so `open` allocates page 0 for meta before creating the root leaf.

- [ ] **Step 1: Write the failing test**
```rust
#[test]
fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist.db");
    // write + checkpoint, then drop everything
    {
        let dm = DiskManager::open(&path).unwrap();
        let mut bp = BufferPool::new(dm, 16);
        let mut meta = storage::btree::tree::load_meta(&mut bp).unwrap();
        let mut t = BPlusTree::open(&mut bp, &mut meta);
        for i in 0..300i64 { t.insert(&encode_i64(i), b"v").unwrap(); }
        t.checkpoint().unwrap();
    }
    // reopen: root must be recovered from meta page 0
    {
        let dm = DiskManager::open(&path).unwrap();
        let mut bp = BufferPool::new(dm, 16);
        let mut meta = storage::btree::tree::load_meta(&mut bp).unwrap();
        let mut t = BPlusTree::open(&mut bp, &mut meta);
        assert_eq!(t.get(&encode_i64(0)).unwrap(), Some(b"v".to_vec()));
        assert_eq!(t.get(&encode_i64(299)).unwrap(), Some(b"v".to_vec()));
    }
}
```
> **Note:** with meta on page 0, the root-leaf allocation in `BPlusTree::open` (Task 10) must skip page 0. Update `open` so that when `load_meta` returns a fresh meta, it reserves page 0 for meta (`allocate_page()` → id 0 held for meta) before allocating the root leaf. Adjust the Task 10 test's `fresh()` helper to call `load_meta` too (update it in this task).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p storage --test btree data_survives_reopen`
Expected: FAIL — `checkpoint` / `load_meta` missing.

- [ ] **Step 3: Write minimal implementation**
```rust
use crate::meta::MetaPage;
use crate::page::{Page, PageId};

pub fn load_meta(bp: &mut BufferPool) -> Result<MetaPage> {
    if bp.disk_mut().num_pages() == 0 {
        // reserve page 0 for meta
        bp.disk_mut().allocate_page()?;
        return Ok(MetaPage { magic: MetaPage::MAGIC, version: 1,
            free_list_head: None, tree_root: None });
    }
    let page = bp.disk_mut().read_page(PageId(0))?;
    MetaPage::decode(&page)
}

impl<'a> BPlusTree<'a> {
    pub fn checkpoint(&mut self) -> Result<()> {
        self.bp.flush_all()?;
        let mut meta_page = self.meta.encode();
        self.bp.disk_mut().write_page(PageId(0), &mut meta_page)?;
        self.bp.disk_mut().sync()
    }
}
```
> Update `BPlusTree::open` (Task 10): only allocate the root leaf when `meta.tree_root.is_none()`, and the first data page will be `PageId(1)` because page 0 is now meta.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p storage`
Expected: PASS (whole suite green, including reopen).

- [ ] **Step 5: Commit**
```bash
git add crates/storage
git commit -m "feat(storage): meta-page checkpoint + reopen durability"
```

---

## Task 15: `ferrodb-kv` CLI (put / get / scan)

**Files:**
- Create: `crates/cli/Cargo.toml`
- Create: `crates/cli/src/main.rs`

**Interfaces:**
- Consumes: the whole `storage` public API.
- Produces: a binary `ferrodb-kv <file.db>` with a rustyline REPL: `put <int-key> <value>`, `get <int-key>`, `scan [lo] [hi]`, `.checkpoint`, `.exit`. Keys are parsed as `i64` and encoded via `encoding::encode_i64`.

- [ ] **Step 1: Write the crate manifest**

`crates/cli/Cargo.toml`:
```toml
[package]
name = "ferrodb-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "ferrodb-kv"
path = "src/main.rs"

[dependencies]
storage = { path = "../storage" }
rustyline = "14"
```

- [ ] **Step 2: Write an integration test driving the store (not the REPL loop)**

`crates/cli/tests/smoke.rs` — exercise the same code path the REPL uses so the CLI has real coverage:
```rust
use storage::disk::DiskManager;
use storage::buffer::BufferPool;
use storage::btree::tree::{BPlusTree, load_meta};
use storage::encoding::encode_i64;

#[test]
fn kv_put_get_scan() {
    let dir = tempfile::tempdir().unwrap();
    let dm = DiskManager::open(dir.path().join("cli.db")).unwrap();
    let mut bp = BufferPool::new(dm, 16);
    let mut meta = load_meta(&mut bp).unwrap();
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    t.insert(&encode_i64(3), b"three").unwrap();
    t.insert(&encode_i64(1), b"one").unwrap();
    let rows = t.scan(None, None).unwrap();
    let keys: Vec<i64> = rows.iter().map(|(k,_)| storage::encoding::decode_i64(k)).collect();
    assert_eq!(keys, vec![1, 3]);
    t.checkpoint().unwrap();
}
```
Add `tempfile = "3"` to `crates/cli` dev-dependencies.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p ferrodb-cli`
Expected: FAIL — crate/deps not yet present (or compile error until `main.rs` exists).

- [ ] **Step 4: Write minimal implementation**

`crates/cli/src/main.rs`:
```rust
use rustyline::DefaultEditor;
use storage::buffer::BufferPool;
use storage::disk::DiskManager;
use storage::btree::tree::{BPlusTree, load_meta};
use storage::encoding::{encode_i64, decode_i64};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "ferrodb.db".into());
    let dm = DiskManager::open(&path)?;
    let mut bp = BufferPool::new(dm, 256);
    let mut meta = load_meta(&mut bp)?;
    let mut rl = DefaultEditor::new()?;
    println!("ferrodb-kv — {path}. commands: put <k> <v> | get <k> | scan [lo] [hi] | .checkpoint | .exit");
    loop {
        let line = match rl.readline("kv> ") { Ok(l) => l, Err(_) => break };
        let _ = rl.add_history_entry(line.as_str());
        let parts: Vec<&str> = line.split_whitespace().collect();
        let mut t = BPlusTree::open(&mut bp, &mut meta);
        match parts.as_slice() {
            [".exit"] => break,
            [".checkpoint"] => { t.checkpoint()?; println!("ok"); }
            ["put", k, v] => { t.insert(&encode_i64(k.parse()?), v.as_bytes())?; println!("ok"); }
            ["get", k] => match t.get(&encode_i64(k.parse()?))? {
                Some(v) => println!("{}", String::from_utf8_lossy(&v)),
                None => println!("(nil)"),
            },
            ["scan", rest @ ..] => {
                let lo = rest.get(0).and_then(|s| s.parse::<i64>().ok()).map(encode_i64);
                let hi = rest.get(1).and_then(|s| s.parse::<i64>().ok()).map(encode_i64);
                for (k, v) in t.scan(lo.as_deref(), hi.as_deref())? {
                    println!("{} = {}", decode_i64(&k), String::from_utf8_lossy(&v));
                }
            }
            [] => {}
            _ => println!("unknown command"),
        }
    }
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    t.checkpoint()?;
    Ok(())
}
```

- [ ] **Step 5: Run test + manual smoke, then commit**

Run: `cargo test --workspace` → Expected: PASS.
Run: `cargo build --release` → Expected: builds `ferrodb-kv`.
```bash
git add crates/cli
git commit -m "feat(cli): ferrodb-kv REPL over the storage engine"
```

---

## Task 16: CI + README + milestone wrap

**Files:**
- Create: `.github/workflows/ci.yml`
- Create: `README.md`

- [ ] **Step 1: Add GitHub Actions CI**

`.github/workflows/ci.yml`:
```yaml
name: ci
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: clippy }
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace
```

- [ ] **Step 2: Write README (project intro + M1 status + roadmap link)**

`README.md` — one-paragraph pitch ("a relational database written from scratch in Rust: page storage, B+-trees, WAL crash recovery, MVCC, cost-based planner, speaks the Postgres wire protocol"), a "Milestone 1 ✅ Storage engine" section with the `ferrodb-kv` demo, and a link to `docs/superpowers/specs/2026-07-02-ferrodb-design.md` for the full roadmap.

- [ ] **Step 3: Run the full gate locally**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 4: Commit + tag milestone**
```bash
git add .github README.md
git commit -m "ci: fmt+clippy+test gate; docs: README + M1 storage engine complete"
git tag m1-storage-engine
```

---

## Self-Review

**Spec coverage (M1 slice of the design):** pager/disk (T3), buffer pool + clock eviction (T6), slotted pages (T7), B+-tree search/insert/split/range/delete (T9–T12), free list (T5), checksums (T2), overflow pages (T13), order-preserving encoding (T8), reopen durability (T14), CLI + CI (T15–T16). WAL, MVCC, SQL, planner, pgwire, and wasm are intentionally deferred to M2–M8 per the roadmap.

**Placeholder scan:** every code step contains real code. Two tasks (T10 split logic, T13 node value-indirection) carry explicit implementer notes rather than a second copy of neighboring code — these are guidance, not placeholders, and the required signatures are fully specified in the Interfaces blocks.

**Type consistency:** `PageId(u32)` newtype throughout; `SlottedPage`/`SlottedPageRef` split for mut/ro access (introduced T7/T9); `BPlusTree::{open,get,insert,scan,delete,checkpoint}` + `load_meta` names are stable across T10–T15; `Split = Option<(Vec<u8>, PageId)>` separator convention ("keys ≥ separator go right") is stated once in T10 and reused in T11 `child_for`. `PAGE_SIZE`/`PAGE_DATA_SIZE` are the single source for page geometry.

**Known M1 simplification (documented, not a gap):** delete does leaf-level removal without internal-node merge; rebalancing is deferred to M4 `VACUUM`. The tree remains valid, sorted, and fully searchable — verified by the T12 property test against `BTreeMap`.
