//! The byte backing behind the disk manager and WAL.
//!
//! [`DiskManager`](crate::disk::DiskManager) and [`Wal`](crate::wal::Wal) treat
//! their storage as a seekable, growable byte blob. A real file provides that on
//! native targets; an in-memory `Vec<u8>` provides it where there is no
//! filesystem (WebAssembly), which is what makes the engine run in a browser.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// A seekable, growable, syncable byte store.
///
/// `Send` is required so a `Database` can move to a per-connection thread in the
/// wire server; both backing types (`File`, `MemBlob`) are `Send`.
#[allow(clippy::len_without_is_empty)] // `len` queries the backing store (&mut self); no empty concept
pub trait Blob: Read + Write + Seek + Send {
    /// Truncate or extend to exactly `len` bytes.
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    /// Flush to durable storage (a no-op for in-memory blobs).
    fn sync(&mut self) -> io::Result<()>;
    /// The current byte length.
    fn len(&mut self) -> io::Result<u64>;
}

impl Blob for File {
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        File::set_len(self, len)
    }
    fn sync(&mut self) -> io::Result<()> {
        self.sync_all()
    }
    fn len(&mut self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
}

/// An in-memory blob backed by a `Vec<u8>` — no filesystem required.
#[derive(Default)]
pub struct MemBlob {
    data: Vec<u8>,
    pos: u64,
}

impl MemBlob {
    pub fn new() -> Self {
        MemBlob::default()
    }
}

impl Read for MemBlob {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let start = self.pos as usize;
        if start >= self.data.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.data.len() - start);
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for MemBlob {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let start = self.pos as usize;
        let end = start + buf.len();
        if end > self.data.len() {
            self.data.resize(end, 0);
        }
        self.data[start..end].copy_from_slice(buf);
        self.pos = end as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for MemBlob {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let base = match from {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.data.len() as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if base < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = base as u64;
        Ok(self.pos)
    }
}

impl Blob for MemBlob {
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.data.resize(len as usize, 0);
        if self.pos > len {
            self.pos = len;
        }
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn len(&mut self) -> io::Result<u64> {
        Ok(self.data.len() as u64)
    }
}
