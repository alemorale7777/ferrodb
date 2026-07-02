//! The disk manager: maps `PageId` ↔ a fixed-size slot in a single data file.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::page::{Page, PageId, PAGE_SIZE};
use crate::{Result, StorageError};

/// Owns the data file and translates page ids into file offsets.
pub struct DiskManager {
    file: File,
    num_pages: u32,
}

impl DiskManager {
    /// Open (creating if absent) the data file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let len = file.metadata()?.len();
        Ok(DiskManager {
            file,
            num_pages: (len / PAGE_SIZE as u64) as u32,
        })
    }

    pub fn num_pages(&self) -> u32 {
        self.num_pages
    }

    /// Read the page at `id`, verifying its checksum.
    pub fn read_page(&mut self, id: PageId) -> Result<Page> {
        if id.0 >= self.num_pages {
            return Err(StorageError::PageOutOfRange(id.0));
        }
        self.file
            .seek(SeekFrom::Start(id.0 as u64 * PAGE_SIZE as u64))?;
        let mut buf = [0u8; PAGE_SIZE];
        self.file.read_exact(&mut buf)?;
        let page = Page::from_bytes(buf);
        if !page.verify_checksum() {
            return Err(StorageError::BadChecksum(id.0));
        }
        Ok(page)
    }

    /// Write `page` to `id` after refreshing its checksum, extending the file if needed.
    pub fn write_page(&mut self, id: PageId, page: &mut Page) -> Result<()> {
        page.compute_checksum();
        self.file
            .seek(SeekFrom::Start(id.0 as u64 * PAGE_SIZE as u64))?;
        self.file.write_all(page.as_bytes())?;
        if id.0 + 1 > self.num_pages {
            self.num_pages = id.0 + 1;
        }
        Ok(())
    }

    /// Append a fresh zeroed page and return its id.
    pub fn allocate_page(&mut self) -> Result<PageId> {
        let id = PageId(self.num_pages);
        let mut zero = Page::new_zeroed();
        self.write_page(id, &mut zero)?;
        Ok(id)
    }

    /// Flush all buffered writes to durable storage.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}
