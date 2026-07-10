//! Index persistence: a sidecar file with an mmap-able vector arena.
//!
//! # Why a sidecar and not the pager (a deliberate, defensible deviation)
//!
//! ferrodb's storage engine is a 4 KiB pager with WAL'd full-page redo. The
//! B+-tree pages beautifully — it was born paged. An HNSW graph does not:
//! traversal hops between arbitrary nodes, so paging it means either
//! WAL-logging every adjacency mutation (pgvector's approach — a milestone of
//! engineering by itself) or thrashing. Instead the index is treated as
//! **derived data**: the base table (WAL-protected) is the source of truth,
//! and the index file is a checkpoint that can always be rebuilt — exactly
//! what `REINDEX` is for. Crash cost: rebuild time, not corruption.
//!
//! # File layout (little-endian, hand-rolled — repo ethos, no serde)
//!
//! ```text
//! [header: 68 bytes]
//!   magic "FDBHNSW\x01" | metric u8 | pad 3 | dim u32 | m u32 | m_max0 u32
//!   | ef_construction u32 | count u32 | entry u32 (MAX = none) | rng u64
//!   | tombstones u64 | graph_len u64 | fnv1a64 of graph section u64
//!   (8+1+3+4+4+4+4+4+4+8+8+8+8 = 68 bytes)
//! [graph section: graph_len bytes]
//!   per node: max_layer u8 | deleted u8 | key_len u16 | key bytes
//!             per layer 0..=max_layer: cnt u32 | cnt × id u32
//! [zero padding to the next 4096 boundary]
//! [vector arena: count × dim × 4 bytes of raw f32]
//! ```
//!
//! The arena starts on a 4096 boundary so it can be mapped and cast to
//! `&[f32]` directly: `mmap` returns page-aligned addresses, and page
//! alignment implies `f32` alignment.
//!
//! # Cold-start tradeoff (interview material)
//!
//! The checksum covers header + graph only. Checksumming the arena would
//! fault in every page at open — the exact cost mmap exists to avoid. So a
//! load is fast and lazy: graph resident and verified, vectors paged in on
//! first touch. The price: arena corruption isn't caught at open (it *is*
//! caught in ferrodb proper, where the table's page checksums cover the
//! source data and the index can be rebuilt from it).

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::distance::{kernels, Metric};
use crate::hnsw::{Hnsw, HnswParams};
use crate::node::Node;

const MAGIC: &[u8; 8] = b"FDBHNSW\x01";
const HEADER_LEN: usize = 68;
const ARENA_ALIGN: usize = 4096;

// ---- the vector arena --------------------------------------------------------

/// Storage behind the vector arena: an owned `Vec<f32>` while building, or a
/// read-only file mapping straight after a load. Reads see `&[f32]` either
/// way; the first mutation upgrades a mapping to owned (copy-on-write, whole
/// arena — inserts after a cold load pay one copy, then proceed normally).
pub enum VectorStore {
    Owned(Vec<f32>),
    #[cfg(unix)]
    Mapped(MappedArena),
}

impl Default for VectorStore {
    fn default() -> Self {
        VectorStore::new()
    }
}

impl VectorStore {
    pub fn new() -> VectorStore {
        VectorStore::Owned(Vec::new())
    }

    pub fn as_slice(&self) -> &[f32] {
        match self {
            VectorStore::Owned(v) => v,
            #[cfg(unix)]
            VectorStore::Mapped(m) => m.as_slice(),
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Append floats, upgrading a mapping to owned memory first.
    pub fn extend(&mut self, v: &[f32]) {
        self.make_owned();
        match self {
            VectorStore::Owned(vec) => vec.extend_from_slice(v),
            #[cfg(unix)]
            VectorStore::Mapped(_) => unreachable!("make_owned upgraded"),
        }
    }

    fn make_owned(&mut self) {
        #[cfg(unix)]
        if let VectorStore::Mapped(m) = self {
            *self = VectorStore::Owned(m.as_slice().to_vec());
        }
    }
}

/// A read-only, page-aligned `mmap` of the vector arena.
///
/// Raw syscalls declared by hand — the repo takes no dependency for what is
/// two `extern "C"` lines (`libc`/`memmap2` would be the crates). Confined
/// `unsafe`, obligations documented at each site.
#[cfg(unix)]
pub struct MappedArena {
    map: *mut core::ffi::c_void,
    map_len: usize,
    nfloats: usize,
}

// SAFETY: the mapping is PROT_READ + MAP_PRIVATE — immutable shared state,
// never written after construction, unmapped once in Drop. Immutable data is
// safe to share and send across threads.
#[cfg(unix)]
unsafe impl Send for MappedArena {}
#[cfg(unix)]
unsafe impl Sync for MappedArena {}

#[cfg(unix)]
mod sys {
    use core::ffi::{c_int, c_void};
    pub const PROT_READ: c_int = 1;
    pub const MAP_PRIVATE: c_int = 2;
    pub const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;
    extern "C" {
        pub fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }
}

#[cfg(unix)]
impl MappedArena {
    /// Map `nfloats` f32s starting at byte `offset` of `file`.
    /// `offset` must be a multiple of the page size (the file format
    /// guarantees a 4096-aligned arena; 4096 divides every real page size
    /// we run on).
    fn map(file: &File, offset: u64, nfloats: usize) -> io::Result<MappedArena> {
        use std::os::unix::io::AsRawFd;
        let map_len = nfloats * 4;
        if map_len == 0 {
            return Ok(MappedArena {
                map: std::ptr::null_mut(),
                map_len: 0,
                nfloats: 0,
            });
        }
        // SAFETY: fd is a live, readable file; offset is page-aligned by the
        // file format (enforced by the writer, validated by the loader);
        // len > 0. A failed map returns MAP_FAILED, checked below.
        let map = unsafe {
            sys::mmap(
                std::ptr::null_mut(),
                map_len,
                sys::PROT_READ,
                sys::MAP_PRIVATE,
                file.as_raw_fd(),
                offset as i64,
            )
        };
        if map == sys::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(MappedArena {
            map,
            map_len,
            nfloats,
        })
    }

    fn as_slice(&self) -> &[f32] {
        if self.nfloats == 0 {
            return &[];
        }
        // SAFETY: `map` is a live PROT_READ mapping of `map_len` bytes
        // (unmapped only in Drop, and `&self` proves no Drop yet); the base
        // address is page-aligned, satisfying f32's 4-byte alignment; every
        // bit pattern is a valid f32.
        unsafe { std::slice::from_raw_parts(self.map as *const f32, self.nfloats) }
    }
}

#[cfg(unix)]
impl Drop for MappedArena {
    fn drop(&mut self) {
        if !self.map.is_null() {
            // SAFETY: exactly the region mmap returned, unmapped once.
            unsafe {
                sys::munmap(self.map, self.map_len);
            }
        }
    }
}

// ---- checksum ------------------------------------------------------------------

/// FNV-1a, 64-bit. Hand-rolled (four lines) rather than pulling a crate; the
/// storage layer's CRC32C guards 4 KiB pages, this guards a one-shot graph
/// blob — collision resistance needs are modest and documented.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---- save / load ----------------------------------------------------------------

fn corrupt(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

impl Hnsw {
    /// Serialize the index to `path` (atomically: write `path.tmp`, fsync,
    /// rename — the same discipline the engine's WAL applies to durability).
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let tmp = path.with_extension("hnsw.tmp");

        // Graph section.
        let mut graph = Vec::new();
        for n in &self.nodes {
            graph.push(n.max_layer);
            graph.push(n.deleted as u8);
            let key_len =
                u16::try_from(n.key.len()).map_err(|_| corrupt("row key longer than u16::MAX"))?;
            graph.extend_from_slice(&key_len.to_le_bytes());
            graph.extend_from_slice(&n.key);
            for layer in &n.neighbors {
                graph.extend_from_slice(&(layer.len() as u32).to_le_bytes());
                for &id in layer {
                    graph.extend_from_slice(&id.to_le_bytes());
                }
            }
        }

        // Header.
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.push(match self.metric {
            Metric::L2 => 0,
            Metric::Cosine => 1,
            Metric::Dot => 2,
        });
        header.extend_from_slice(&[0u8; 3]); // pad
        header.extend_from_slice(&(self.dim as u32).to_le_bytes());
        header.extend_from_slice(&(self.params.m as u32).to_le_bytes());
        header.extend_from_slice(&(self.params.m_max0 as u32).to_le_bytes());
        header.extend_from_slice(&(self.params.ef_construction as u32).to_le_bytes());
        header.extend_from_slice(&(self.nodes.len() as u32).to_le_bytes());
        header.extend_from_slice(&self.entry.unwrap_or(u32::MAX).to_le_bytes());
        header.extend_from_slice(&self.rng.to_le_bytes());
        header.extend_from_slice(&(self.tombstones as u64).to_le_bytes());
        header.extend_from_slice(&(graph.len() as u64).to_le_bytes());
        header.extend_from_slice(&fnv1a(&graph).to_le_bytes());
        debug_assert_eq!(header.len(), HEADER_LEN);

        let mut f = File::create(&tmp)?;
        f.write_all(&header)?;
        f.write_all(&graph)?;
        // Pad so the arena lands on a page boundary (mmap offset must be
        // page-aligned, and page alignment gives f32 alignment for free).
        let pos = HEADER_LEN + graph.len();
        let pad = (ARENA_ALIGN - pos % ARENA_ALIGN) % ARENA_ALIGN;
        f.write_all(&vec![0u8; pad])?;
        let arena = self.vectors.as_slice();
        // f32 -> LE bytes without unsafe: to_le_bytes per element into a
        // buffered chunk (the write is one-shot; clarity beats cleverness).
        let mut buf = Vec::with_capacity(arena.len() * 4);
        for x in arena {
            buf.extend_from_slice(&x.to_le_bytes());
        }
        f.write_all(&buf)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load an index. On unix the vector arena is `mmap`'d (lazy, fast cold
    /// start); elsewhere it is read into memory. The graph section is always
    /// read fully and checksum-verified — a torn or corrupt file is an error,
    /// and the engine's answer to that error is a rebuild from the table.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Hnsw> {
        Self::load_inner(path.as_ref(), true)
    }

    /// Load with the arena read into owned memory even on unix (tests use
    /// this to compare the two paths; callers who intend heavy inserts can
    /// use it to skip the copy-on-write upgrade).
    pub fn load_owned(path: impl AsRef<Path>) -> io::Result<Hnsw> {
        Self::load_inner(path.as_ref(), false)
    }

    fn load_inner(path: &Path, use_mmap: bool) -> io::Result<Hnsw> {
        let mut f = File::open(path)?;
        let mut header = [0u8; HEADER_LEN];
        f.read_exact(&mut header)
            .map_err(|_| corrupt("short header"))?;
        if &header[0..8] != MAGIC {
            return Err(corrupt("bad magic: not a ferrodb hnsw file"));
        }
        let u32_at = |o: usize| u32::from_le_bytes(header[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(header[o..o + 8].try_into().unwrap());
        let metric = match header[8] {
            0 => Metric::L2,
            1 => Metric::Cosine,
            2 => Metric::Dot,
            _ => return Err(corrupt("bad metric tag")),
        };
        let dim = u32_at(12) as usize;
        let params = HnswParams {
            m: u32_at(16) as usize,
            m_max0: u32_at(20) as usize,
            ef_construction: u32_at(24) as usize,
        };
        let count = u32_at(28) as usize;
        let entry = match u32_at(32) {
            u32::MAX => None,
            e => Some(e),
        };
        let rng = u64_at(36);
        let tombstones = u64_at(44) as usize;
        let graph_len = u64_at(52) as usize;
        let want_sum = u64_at(60);
        if dim == 0 || params.m < 2 {
            return Err(corrupt("bad header parameters"));
        }

        let mut graph = vec![0u8; graph_len];
        f.read_exact(&mut graph)
            .map_err(|_| corrupt("torn graph section"))?;
        if fnv1a(&graph) != want_sum {
            return Err(corrupt("graph checksum mismatch"));
        }

        // Decode nodes.
        let mut nodes = Vec::with_capacity(count);
        let mut pos = 0usize;
        let take = |pos: &mut usize, n: usize, g: &[u8]| -> io::Result<()> {
            if *pos + n > g.len() {
                return Err(corrupt("graph section underrun"));
            }
            Ok(())
        };
        for _ in 0..count {
            take(&mut pos, 4, &graph)?;
            let max_layer = graph[pos];
            let deleted = graph[pos + 1] != 0;
            let key_len = u16::from_le_bytes(graph[pos + 2..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            take(&mut pos, key_len, &graph)?;
            let key = graph[pos..pos + key_len].to_vec();
            pos += key_len;
            let mut neighbors = Vec::with_capacity(max_layer as usize + 1);
            for _ in 0..=max_layer {
                take(&mut pos, 4, &graph)?;
                let cnt = u32::from_le_bytes(graph[pos..pos + 4].try_into().unwrap()) as usize;
                pos += 4;
                take(&mut pos, cnt * 4, &graph)?;
                let mut adj = Vec::with_capacity(cnt);
                for i in 0..cnt {
                    let id =
                        u32::from_le_bytes(graph[pos + i * 4..pos + i * 4 + 4].try_into().unwrap());
                    if id as usize >= count {
                        return Err(corrupt("neighbor id out of range"));
                    }
                    adj.push(id);
                }
                pos += cnt * 4;
                neighbors.push(adj);
            }
            nodes.push(Node {
                max_layer,
                key,
                deleted,
                neighbors,
            });
        }
        if pos != graph_len {
            return Err(corrupt("graph section overrun"));
        }
        if let Some(e) = entry {
            if e as usize >= count {
                return Err(corrupt("entry point out of range"));
            }
        }

        // The arena.
        let arena_off = {
            let p = HEADER_LEN + graph_len;
            p.div_ceil(ARENA_ALIGN) * ARENA_ALIGN
        };
        let nfloats = count * dim;
        let file_len = f.metadata()?.len() as usize;
        if file_len < arena_off + nfloats * 4 {
            return Err(corrupt("vector arena truncated"));
        }

        let vectors = Self::open_arena(&f, arena_off as u64, nfloats, use_mmap)?;

        Ok(Hnsw {
            dim,
            metric,
            params,
            ml: 1.0 / (params.m as f64).ln(),
            kern: kernels(),
            vectors,
            nodes,
            entry,
            rng,
            tombstones,
        })
    }

    #[cfg(unix)]
    fn open_arena(f: &File, off: u64, nfloats: usize, use_mmap: bool) -> io::Result<VectorStore> {
        if use_mmap {
            return Ok(VectorStore::Mapped(MappedArena::map(f, off, nfloats)?));
        }
        Self::read_arena(f, off, nfloats)
    }

    #[cfg(not(unix))]
    fn open_arena(f: &File, off: u64, nfloats: usize, _use_mmap: bool) -> io::Result<VectorStore> {
        // No mmap here (e.g. Windows dev machines): read the arena eagerly.
        // Same behavior, higher cold-start cost — documented tradeoff.
        Self::read_arena(f, off, nfloats)
    }

    fn read_arena(mut f: &File, off: u64, nfloats: usize) -> io::Result<VectorStore> {
        use std::io::Seek;
        f.seek(io::SeekFrom::Start(off))?;
        let mut buf = vec![0u8; nfloats * 4];
        f.read_exact(&mut buf)?;
        let mut floats = Vec::with_capacity(nfloats);
        for c in buf.chunks_exact(4) {
            floats.push(f32::from_le_bytes(c.try_into().unwrap()));
        }
        Ok(VectorStore::Owned(floats))
    }
}
