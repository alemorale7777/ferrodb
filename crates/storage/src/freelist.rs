//! A LIFO free list of reclaimed pages, threaded through the pages themselves.
//!
//! Each free page stores the id of the next free page in its first 4 bytes;
//! `meta.free_list_head` points at the top of the stack.

use crate::disk::DiskManager;
use crate::meta::MetaPage;
use crate::page::{Page, PageId};
use crate::Result;

const NIL: u32 = u32::MAX;

/// Pop a reclaimed page if one is free, otherwise grow the file.
pub fn alloc(dm: &mut DiskManager, meta: &mut MetaPage) -> Result<PageId> {
    match meta.free_list_head {
        Some(head) => {
            let page = dm.read_page(head)?;
            let next = u32::from_le_bytes(page.data()[0..4].try_into().unwrap());
            meta.free_list_head = if next == NIL {
                None
            } else {
                Some(PageId(next))
            };
            Ok(head)
        }
        None => dm.allocate_page(),
    }
}

/// Push `id` onto the free list for later reuse.
pub fn free(dm: &mut DiskManager, meta: &mut MetaPage, id: PageId) -> Result<()> {
    let mut page = Page::new_zeroed();
    let old = meta.free_list_head.map(|p| p.0).unwrap_or(NIL);
    page.data_mut()[0..4].copy_from_slice(&old.to_le_bytes());
    dm.write_page(id, &mut page)?;
    meta.free_list_head = Some(id);
    Ok(())
}
