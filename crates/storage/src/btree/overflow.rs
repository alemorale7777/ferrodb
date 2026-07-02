//! Overflow page chains for values too large to store inline in a leaf cell.
//!
//! Each overflow page is `[next:u32][chunk...]`; `next == u32::MAX` ends the chain.

use crate::buffer::BufferPool;
use crate::page::{PageId, PAGE_DATA_SIZE};
use crate::Result;

const NIL: u32 = u32::MAX;
const CHUNK: usize = PAGE_DATA_SIZE - 4;

/// Write `bytes` across a linked chain of pages; return the head page id.
pub fn write_chain(bp: &mut BufferPool, bytes: &[u8]) -> Result<PageId> {
    let mut next = NIL;
    let mut head = PageId(NIL);
    let chunks: Vec<&[u8]> = bytes.chunks(CHUNK).collect();
    for chunk in chunks.into_iter().rev() {
        let id = bp.disk_mut().allocate_page()?;
        let f = bp.new_page(id)?;
        {
            let d = bp.frame_mut(f).data_mut();
            d[0..4].copy_from_slice(&next.to_le_bytes());
            d[4..4 + chunk.len()].copy_from_slice(chunk);
        }
        bp.mark_dirty(f);
        bp.unpin(f);
        next = id.0;
        head = id;
    }
    Ok(head)
}

/// Reassemble `total_len` bytes starting from the chain head.
pub fn read_chain(bp: &mut BufferPool, head: PageId, total_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(total_len);
    let mut cur = head.0;
    while cur != NIL && out.len() < total_len {
        let f = bp.fetch(PageId(cur))?;
        let (next, chunk) = {
            let d = bp.frame(f).data();
            let next = u32::from_le_bytes(d[0..4].try_into().unwrap());
            let take = (total_len - out.len()).min(CHUNK);
            (next, d[4..4 + take].to_vec())
        };
        out.extend_from_slice(&chunk);
        bp.unpin(f);
        cur = next;
    }
    Ok(out)
}
