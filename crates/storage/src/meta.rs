//! The meta page (`PageId(0)`): the database's root record.

use crate::page::{Page, PageId};
use crate::{Result, StorageError};

const NIL: u32 = u32::MAX;

/// Contents of page 0: identifies the file and points at the free list and B+-tree root.
#[derive(Clone, Copy, Debug)]
pub struct MetaPage {
    pub magic: u32,
    pub version: u16,
    pub free_list_head: Option<PageId>,
    pub tree_root: Option<PageId>,
}

impl MetaPage {
    pub const MAGIC: u32 = 0xFE44_0DB0;

    fn opt(id: Option<PageId>) -> u32 {
        id.map(|p| p.0).unwrap_or(NIL)
    }
    fn unopt(v: u32) -> Option<PageId> {
        if v == NIL {
            None
        } else {
            Some(PageId(v))
        }
    }

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
        if magic != Self::MAGIC {
            return Err(StorageError::Corrupt("bad meta magic"));
        }
        Ok(MetaPage {
            magic,
            version: u16::from_le_bytes(d[4..6].try_into().unwrap()),
            free_list_head: Self::unopt(u32::from_le_bytes(d[6..10].try_into().unwrap())),
            tree_root: Self::unopt(u32::from_le_bytes(d[10..14].try_into().unwrap())),
        })
    }
}
