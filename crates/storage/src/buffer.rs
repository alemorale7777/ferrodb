//! A buffer pool caching pages in memory with clock-sweep eviction.
//!
//! Callers `fetch` a page (getting a pinned frame index), read/write it via
//! `frame`/`frame_mut`, `mark_dirty` if they changed it, and `unpin` when done.

use std::collections::HashMap;

use crate::disk::DiskManager;
use crate::page::{Page, PageId};
use crate::{Result, StorageError};

struct Frame {
    id: PageId,
    page: Page,
    pins: u32,
    dirty: bool,
    ref_bit: bool,
}

pub struct BufferPool {
    dm: DiskManager,
    frames: Vec<Frame>,
    table: HashMap<PageId, usize>,
    capacity: usize,
    clock: usize,
    no_steal: bool,
}

impl BufferPool {
    pub fn new(dm: DiskManager, capacity: usize) -> Self {
        BufferPool {
            dm,
            frames: Vec::with_capacity(capacity),
            table: HashMap::new(),
            capacity,
            clock: 0,
            no_steal: false,
        }
    }

    /// In no-steal mode, dirty pages are never evicted (written to disk) — the
    /// transaction layer flushes them explicitly at commit. Enables WAL recovery.
    pub fn set_no_steal(&mut self, v: bool) {
        self.no_steal = v;
    }

    /// Whether any frame currently holds unflushed changes.
    pub fn has_dirty(&self) -> bool {
        self.frames.iter().any(|f| f.dirty)
    }

    /// Clone the current image + id of every dirty frame (for WAL logging).
    pub fn dirty_frames(&self) -> Vec<(PageId, Page)> {
        self.frames
            .iter()
            .filter(|f| f.dirty)
            .map(|f| (f.id, f.page.clone()))
            .collect()
    }

    /// Drop all uncommitted changes: reload each dirty frame's committed image
    /// from disk and clear its dirty flag. Used to roll back an aborted statement.
    pub fn discard_dirty(&mut self) -> Result<()> {
        let dm = &mut self.dm;
        for f in &mut self.frames {
            if f.dirty {
                if f.id.0 < dm.num_pages() {
                    f.page = dm.read_page(f.id)?;
                }
                f.dirty = false;
            }
        }
        Ok(())
    }

    pub fn disk_mut(&mut self) -> &mut DiskManager {
        &mut self.dm
    }
    pub fn frame(&self, i: usize) -> &Page {
        &self.frames[i].page
    }
    pub fn frame_mut(&mut self, i: usize) -> &mut Page {
        &mut self.frames[i].page
    }
    pub fn mark_dirty(&mut self, i: usize) {
        self.frames[i].dirty = true;
    }
    pub fn unpin(&mut self, i: usize) {
        if self.frames[i].pins > 0 {
            self.frames[i].pins -= 1;
        }
    }

    fn install(&mut self, id: PageId, page: Page) -> Result<usize> {
        if self.frames.len() < self.capacity {
            self.frames.push(Frame {
                id,
                page,
                pins: 1,
                dirty: false,
                ref_bit: true,
            });
            let i = self.frames.len() - 1;
            self.table.insert(id, i);
            return Ok(i);
        }
        let n = self.frames.len();
        for _ in 0..(2 * n) {
            let i = self.clock;
            self.clock = (self.clock + 1) % n;
            if self.frames[i].pins > 0 {
                continue;
            }
            if self.no_steal && self.frames[i].dirty {
                continue; // never steal an uncommitted dirty page
            }
            if self.frames[i].ref_bit {
                self.frames[i].ref_bit = false;
                continue;
            }
            if self.frames[i].dirty {
                let vid = self.frames[i].id;
                self.dm.write_page(vid, &mut self.frames[i].page)?;
            }
            self.table.remove(&self.frames[i].id);
            self.frames[i] = Frame {
                id,
                page,
                pins: 1,
                dirty: false,
                ref_bit: true,
            };
            self.table.insert(id, i);
            return Ok(i);
        }
        Err(StorageError::Corrupt("buffer pool full of pinned pages"))
    }

    /// Bring page `id` into a pinned frame, loading from disk on a miss.
    pub fn fetch(&mut self, id: PageId) -> Result<usize> {
        if let Some(&i) = self.table.get(&id) {
            self.frames[i].pins += 1;
            self.frames[i].ref_bit = true;
            return Ok(i);
        }
        let page = self.dm.read_page(id)?;
        self.install(id, page)
    }

    /// Bring a freshly-allocated `id` into a pinned dirty frame without reading disk.
    pub fn new_page(&mut self, id: PageId) -> Result<usize> {
        let i = self.install(id, Page::new_zeroed())?;
        self.frames[i].dirty = true;
        Ok(i)
    }

    /// Flush every dirty frame and sync the underlying file.
    pub fn flush_all(&mut self) -> Result<()> {
        let dm = &mut self.dm;
        for f in &mut self.frames {
            if f.dirty {
                dm.write_page(f.id, &mut f.page)?;
                f.dirty = false;
            }
        }
        dm.sync()
    }
}
