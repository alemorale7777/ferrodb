//! Write-ahead log: full-page redo records for crash recovery.
//!
//! Records are appended sequentially:
//!   - `Update`: `[0][txn:u64][page_id:u32][PAGE_SIZE bytes]`
//!   - `Commit`: `[1][txn:u64]`
//!
//! With a no-steal buffer pool, uncommitted changes never reach the data file,
//! so recovery is **redo-only**: replay the after-images of committed txns.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::disk::DiskManager;
use crate::page::{Page, PageId, PAGE_SIZE};
use crate::Result;

const REC_UPDATE: u8 = 0;
const REC_COMMIT: u8 = 1;

const UPDATE_LEN: usize = 1 + 8 + 4 + PAGE_SIZE;
const COMMIT_LEN: usize = 1 + 8;

pub struct Wal {
    file: File,
}

impl Wal {
    pub fn open(path: impl AsRef<Path>) -> Result<Wal> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Wal { file })
    }

    /// Append a full-page after-image for `pid` under transaction `txn`.
    pub fn append_update(&mut self, txn: u64, pid: PageId, page: &Page) -> Result<()> {
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&[REC_UPDATE])?;
        self.file.write_all(&txn.to_le_bytes())?;
        self.file.write_all(&pid.0.to_le_bytes())?;
        self.file.write_all(page.as_bytes())?;
        Ok(())
    }

    /// Append a commit marker for `txn`.
    pub fn append_commit(&mut self, txn: u64) -> Result<()> {
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&[REC_COMMIT])?;
        self.file.write_all(&txn.to_le_bytes())?;
        Ok(())
    }

    /// Force the log to durable storage. Call before flushing data pages.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Truncate the log to empty (its records are now durable in the data file).
    pub fn reset(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Replay committed page images into the data file, then clear the log.
    /// A truncated tail record (the crash point) ends parsing.
    pub fn recover(&mut self, dm: &mut DiskManager) -> Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut buf = Vec::new();
        self.file.read_to_end(&mut buf)?;

        let mut updates: Vec<(u64, PageId, [u8; PAGE_SIZE])> = Vec::new();
        let mut committed: HashSet<u64> = HashSet::new();
        let mut pos = 0usize;
        while pos < buf.len() {
            match buf[pos] {
                REC_UPDATE => {
                    if pos + UPDATE_LEN > buf.len() {
                        break; // torn tail
                    }
                    let txn = u64::from_le_bytes(buf[pos + 1..pos + 9].try_into().unwrap());
                    let pid = u32::from_le_bytes(buf[pos + 9..pos + 13].try_into().unwrap());
                    let mut img = [0u8; PAGE_SIZE];
                    img.copy_from_slice(&buf[pos + 13..pos + 13 + PAGE_SIZE]);
                    updates.push((txn, PageId(pid), img));
                    pos += UPDATE_LEN;
                }
                REC_COMMIT => {
                    if pos + COMMIT_LEN > buf.len() {
                        break;
                    }
                    let txn = u64::from_le_bytes(buf[pos + 1..pos + 9].try_into().unwrap());
                    committed.insert(txn);
                    pos += COMMIT_LEN;
                }
                _ => break, // garbage / torn
            }
        }

        let mut replayed = false;
        for (txn, pid, img) in updates {
            if committed.contains(&txn) {
                let mut page = Page::from_bytes(img);
                dm.write_page(pid, &mut page)?;
                replayed = true;
            }
        }
        if replayed {
            dm.sync()?;
        }
        self.reset()?;
        Ok(())
    }
}
