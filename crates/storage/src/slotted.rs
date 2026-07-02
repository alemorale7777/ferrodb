//! Slotted-page layout: a slot directory growing up from the header and
//! variable-length cells growing down from the end of the data region.
//!
//! Header (6 bytes): `num_slots: u16`, `free_start: u16`, `free_end: u16`.
//! Each directory slot is `(offset: u16, len: u16)`, pointing at a cell.

use crate::page::Page;
use crate::{Result, StorageError};

const HDR: usize = 6;
const SLOT: usize = 4;

/// Mutable view over a page's slot directory + cells.
pub struct SlottedPage<'a>(&'a mut Page);

/// Read-only view over a page's slot directory + cells.
pub struct SlottedPageRef<'a>(&'a Page);

impl<'a> SlottedPage<'a> {
    pub fn new(p: &'a mut Page) -> Self {
        SlottedPage(p)
    }

    /// Reset to an empty page (no slots, all data region free).
    pub fn init(&mut self) {
        let cap = self.0.data().len() as u16;
        self.set_u16(0, 0);
        self.set_u16(2, HDR as u16);
        self.set_u16(4, cap);
    }

    fn set_u16(&mut self, at: usize, v: u16) {
        self.0.data_mut()[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn get_u16(&self, at: usize) -> u16 {
        u16::from_le_bytes(self.0.data()[at..at + 2].try_into().unwrap())
    }

    pub fn num_slots(&self) -> u16 {
        self.get_u16(0)
    }
    fn free_start(&self) -> u16 {
        self.get_u16(2)
    }
    fn free_end(&self) -> u16 {
        self.get_u16(4)
    }

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

    /// Insert `bytes` as a new cell at directory index `slot`, shifting later slots right.
    pub fn insert(&mut self, slot: u16, bytes: &[u8]) -> Result<()> {
        let n = self.num_slots();
        let need = SLOT + bytes.len();
        if self.free_space() < need {
            return Err(StorageError::PageFull);
        }
        let new_end = self.free_end() as usize - bytes.len();
        self.0.data_mut()[new_end..new_end + bytes.len()].copy_from_slice(bytes);
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

    /// Remove the cell at `slot`, shifting later directory entries left.
    /// The cell's bytes become dead space until [`SlottedPage::compact`].
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
    }

    /// Replace the cell at `slot` (remove + re-insert).
    pub fn set(&mut self, slot: u16, bytes: &[u8]) -> Result<()> {
        self.remove(slot);
        self.insert(slot, bytes)
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u8]> + '_ {
        (0..self.num_slots()).map(move |s| self.get(s))
    }

    /// Rewrite live cells contiguously, reclaiming dead space from removals.
    pub fn compact(&mut self) {
        let cells: Vec<Vec<u8>> = self.iter().map(|c| c.to_vec()).collect();
        self.init();
        for (i, c) in cells.iter().enumerate() {
            let _ = self.insert(i as u16, c);
        }
    }
}

impl<'a> SlottedPageRef<'a> {
    pub fn new(p: &'a Page) -> Self {
        SlottedPageRef(p)
    }
    fn get_u16(&self, at: usize) -> u16 {
        u16::from_le_bytes(self.0.data()[at..at + 2].try_into().unwrap())
    }
    pub fn num_slots(&self) -> u16 {
        self.get_u16(0)
    }
    /// The returned slice borrows the underlying page (`'a`), not this view,
    /// so callers may construct the view inline: `SlottedPageRef::new(p).get(s)`.
    pub fn get(&self, slot: u16) -> &'a [u8] {
        let base = HDR + slot as usize * SLOT;
        let off = self.get_u16(base) as usize;
        let len = self.get_u16(base + 2) as usize;
        &self.0.data()[off..off + len]
    }
}
