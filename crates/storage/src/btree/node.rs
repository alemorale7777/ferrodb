//! B+-tree node encoding over a slotted page.
//!
//! Slot 0 is a header cell: `[kind:u8][next_leaf:u32][left_child:u32]`.
//! Data slots `1..num_slots` are sorted by key:
//!
//! - leaf cell:     `[klen:u16][key][value-opaque]`
//! - internal cell: `[klen:u16][key][child:u32]`
//!
//! The value is opaque to this module — the tree layer decides inline vs. overflow.

use crate::page::{Page, PageId};
use crate::slotted::{SlottedPage, SlottedPageRef};
use crate::Result;

const NIL: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Leaf,
    Internal,
}

fn write_header(page: &mut Page, kind: NodeKind, next_leaf: Option<PageId>, left: Option<PageId>) {
    let mut hdr = [0u8; 9];
    hdr[0] = if kind == NodeKind::Leaf { 0 } else { 1 };
    hdr[1..5].copy_from_slice(&next_leaf.map(|p| p.0).unwrap_or(NIL).to_le_bytes());
    hdr[5..9].copy_from_slice(&left.map(|p| p.0).unwrap_or(NIL).to_le_bytes());
    let mut sp = SlottedPage::new(page);
    sp.insert(0, &hdr).unwrap();
}

pub fn init_leaf(page: &mut Page, next_leaf: Option<PageId>) {
    {
        let mut sp = SlottedPage::new(page);
        sp.init();
    }
    write_header(page, NodeKind::Leaf, next_leaf, None);
}

pub fn init_internal(page: &mut Page, left_child: PageId) {
    {
        let mut sp = SlottedPage::new(page);
        sp.init();
    }
    write_header(page, NodeKind::Internal, None, Some(left_child));
}

pub fn read_kind(page: &Page) -> NodeKind {
    if SlottedPageRef::new(page).get(0)[0] == 0 {
        NodeKind::Leaf
    } else {
        NodeKind::Internal
    }
}

pub fn read_next_leaf(page: &Page) -> Option<PageId> {
    let v = u32::from_le_bytes(SlottedPageRef::new(page).get(0)[1..5].try_into().unwrap());
    if v == NIL {
        None
    } else {
        Some(PageId(v))
    }
}

pub fn left_child(page: &Page) -> PageId {
    PageId(u32::from_le_bytes(
        SlottedPageRef::new(page).get(0)[5..9].try_into().unwrap(),
    ))
}

/// Number of key entries (data slots), i.e. slots minus the header slot.
pub fn num_entries(page: &Page) -> u16 {
    SlottedPageRef::new(page).num_slots() - 1
}

pub fn key_of(page: &Page, slot: u16) -> Vec<u8> {
    let cell = SlottedPageRef::new(page).get(slot);
    let klen = u16::from_le_bytes(cell[0..2].try_into().unwrap()) as usize;
    cell[2..2 + klen].to_vec()
}

/// Binary search over data slots `[1, num_slots)`. Returns `(slot, found)`,
/// where on a miss `slot` is the insertion point.
pub fn search_key(page: &Page, key: &[u8]) -> (u16, bool) {
    let n = SlottedPageRef::new(page).num_slots();
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

pub fn leaf_put(page: &mut Page, key: &[u8], val: &[u8]) -> Result<()> {
    let (slot, found) = search_key(page, key);
    let mut cell = Vec::with_capacity(2 + key.len() + val.len());
    cell.extend_from_slice(&(key.len() as u16).to_le_bytes());
    cell.extend_from_slice(key);
    cell.extend_from_slice(val);
    let mut sp = SlottedPage::new(page);
    if found {
        sp.set(slot, &cell)
    } else {
        sp.insert(slot, &cell)
    }
}

pub fn leaf_at(page: &Page, slot: u16) -> (Vec<u8>, Vec<u8>) {
    let cell = SlottedPageRef::new(page).get(slot);
    let klen = u16::from_le_bytes(cell[0..2].try_into().unwrap()) as usize;
    (cell[2..2 + klen].to_vec(), cell[2 + klen..].to_vec())
}

pub fn leaf_entries(page: &Page) -> Vec<(Vec<u8>, Vec<u8>)> {
    (1..=num_entries(page)).map(|s| leaf_at(page, s)).collect()
}

pub fn internal_put(page: &mut Page, key: &[u8], child: PageId) -> Result<()> {
    let (slot, found) = search_key(page, key);
    let mut cell = Vec::with_capacity(2 + key.len() + 4);
    cell.extend_from_slice(&(key.len() as u16).to_le_bytes());
    cell.extend_from_slice(key);
    cell.extend_from_slice(&child.0.to_le_bytes());
    let mut sp = SlottedPage::new(page);
    if found {
        sp.set(slot, &cell)
    } else {
        sp.insert(slot, &cell)
    }
}

pub fn internal_at(page: &Page, slot: u16) -> (Vec<u8>, PageId) {
    let cell = SlottedPageRef::new(page).get(slot);
    let klen = u16::from_le_bytes(cell[0..2].try_into().unwrap()) as usize;
    let child = u32::from_le_bytes(cell[2 + klen..2 + klen + 4].try_into().unwrap());
    (cell[2..2 + klen].to_vec(), PageId(child))
}

pub fn internal_entries(page: &Page) -> Vec<(Vec<u8>, PageId)> {
    (1..=num_entries(page))
        .map(|s| internal_at(page, s))
        .collect()
}
