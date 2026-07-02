//! The B+-tree itself: search, insert (with leaf/internal splits and root
//! growth), ordered range scan, delete, and checkpoint-to-meta persistence.
//!
//! Separator convention: an internal entry `(sep, child)` means "keys ≥ sep go
//! to `child`"; keys below the first separator go to the node's `left_child`.

use crate::btree::{node, overflow};
use crate::buffer::BufferPool;
use crate::meta::MetaPage;
use crate::page::{Page, PageId, PAGE_DATA_SIZE};
use crate::{Result, StorageError};

pub struct BPlusTree<'a> {
    bp: &'a mut BufferPool,
    meta: &'a mut MetaPage,
}

/// A split bubbling up to the parent: `(separator_key, new_right_page)`.
type Split = Option<(Vec<u8>, PageId)>;

/// Load the meta page (page 0), reserving it on a brand-new file.
pub fn load_meta(bp: &mut BufferPool) -> Result<MetaPage> {
    if bp.disk_mut().num_pages() == 0 {
        bp.disk_mut().allocate_page()?; // reserve page 0 for meta
        return Ok(MetaPage {
            magic: MetaPage::MAGIC,
            version: 1,
            free_list_head: None,
            tree_root: None,
        });
    }
    let page = bp.disk_mut().read_page(PageId(0))?;
    MetaPage::decode(&page)
}

impl<'a> BPlusTree<'a> {
    pub fn open(bp: &'a mut BufferPool, meta: &'a mut MetaPage) -> Self {
        let t = BPlusTree { bp, meta };
        if t.meta.tree_root.is_none() {
            if t.bp.disk_mut().num_pages() == 0 {
                t.bp.disk_mut().allocate_page().unwrap(); // reserve page 0 for meta
            }
            let id = t.bp.disk_mut().allocate_page().unwrap();
            let f = t.bp.new_page(id).unwrap();
            node::init_leaf(t.bp.frame_mut(f), None);
            t.bp.mark_dirty(f);
            t.bp.unpin(f);
            t.meta.tree_root = Some(id);
        }
        t
    }

    // ---- value inline/overflow codec -------------------------------------

    fn encode_value(&mut self, val: &[u8]) -> Result<Vec<u8>> {
        let threshold = PAGE_DATA_SIZE / 4;
        if val.len() < threshold {
            let mut e = Vec::with_capacity(val.len() + 1);
            e.push(0);
            e.extend_from_slice(val);
            Ok(e)
        } else {
            let head = overflow::write_chain(self.bp, val)?;
            let mut e = Vec::with_capacity(9);
            e.push(1);
            e.extend_from_slice(&(val.len() as u32).to_le_bytes());
            e.extend_from_slice(&head.0.to_le_bytes());
            Ok(e)
        }
    }

    fn decode_value(&mut self, stored: &[u8]) -> Result<Vec<u8>> {
        if stored.is_empty() {
            return Ok(Vec::new());
        }
        if stored[0] == 0 {
            Ok(stored[1..].to_vec())
        } else {
            let len = u32::from_le_bytes(stored[1..5].try_into().unwrap()) as usize;
            let head = PageId(u32::from_le_bytes(stored[5..9].try_into().unwrap()));
            overflow::read_chain(self.bp, head, len)
        }
    }

    // ---- point lookup -----------------------------------------------------

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut pid = self.meta.tree_root.unwrap();
        loop {
            let f = self.bp.fetch(pid)?;
            let page = self.bp.frame(f).clone();
            self.bp.unpin(f);
            match node::read_kind(&page) {
                node::NodeKind::Leaf => {
                    let (slot, found) = node::search_key(&page, key);
                    if found {
                        let stored = node::leaf_at(&page, slot).1;
                        return Ok(Some(self.decode_value(&stored)?));
                    }
                    return Ok(None);
                }
                node::NodeKind::Internal => pid = child_for(&page, key),
            }
        }
    }

    // ---- insert -----------------------------------------------------------

    pub fn insert(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        let encoded = self.encode_value(val)?;
        let root = self.meta.tree_root.unwrap();
        if let Some((sep, right)) = self.insert_rec(root, key, &encoded)? {
            let id = self.bp.disk_mut().allocate_page()?;
            let f = self.bp.new_page(id)?;
            node::init_internal(self.bp.frame_mut(f), root);
            node::internal_put(self.bp.frame_mut(f), &sep, right)?;
            self.bp.mark_dirty(f);
            self.bp.unpin(f);
            self.meta.tree_root = Some(id);
        }
        Ok(())
    }

    fn insert_rec(&mut self, pid: PageId, key: &[u8], enc_val: &[u8]) -> Result<Split> {
        let f = self.bp.fetch(pid)?;
        let kind = node::read_kind(self.bp.frame(f));
        match kind {
            node::NodeKind::Leaf => {
                let res = node::leaf_put(self.bp.frame_mut(f), key, enc_val);
                match res {
                    Ok(()) => {
                        self.bp.mark_dirty(f);
                        self.bp.unpin(f);
                        Ok(None)
                    }
                    Err(StorageError::PageFull) => {
                        self.bp.unpin(f);
                        self.split_leaf(pid, key, enc_val)
                    }
                    Err(e) => {
                        self.bp.unpin(f);
                        Err(e)
                    }
                }
            }
            node::NodeKind::Internal => {
                let page = self.bp.frame(f).clone();
                self.bp.unpin(f);
                let child = child_for(&page, key);
                if let Some((sep, right)) = self.insert_rec(child, key, enc_val)? {
                    let f2 = self.bp.fetch(pid)?;
                    let r = node::internal_put(self.bp.frame_mut(f2), &sep, right);
                    match r {
                        Ok(()) => {
                            self.bp.mark_dirty(f2);
                            self.bp.unpin(f2);
                            Ok(None)
                        }
                        Err(StorageError::PageFull) => {
                            self.bp.unpin(f2);
                            self.split_internal(pid, &sep, right)
                        }
                        Err(e) => {
                            self.bp.unpin(f2);
                            Err(e)
                        }
                    }
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn split_leaf(&mut self, pid: PageId, key: &[u8], enc_val: &[u8]) -> Result<Split> {
        let f = self.bp.fetch(pid)?;
        let page = self.bp.frame(f).clone();
        let old_next = node::read_next_leaf(&page);
        self.bp.unpin(f);

        let mut entries = node::leaf_entries(&page);
        match entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
            Ok(i) => entries[i].1 = enc_val.to_vec(),
            Err(i) => entries.insert(i, (key.to_vec(), enc_val.to_vec())),
        }
        let mid = entries.len() / 2;
        let sep = entries[mid].0.clone();
        let right_id = self.bp.disk_mut().allocate_page()?;

        let lf = self.bp.fetch(pid)?;
        node::init_leaf(self.bp.frame_mut(lf), Some(right_id));
        for (k, v) in &entries[..mid] {
            node::leaf_put(self.bp.frame_mut(lf), k, v)?;
        }
        self.bp.mark_dirty(lf);
        self.bp.unpin(lf);

        let rf = self.bp.new_page(right_id)?;
        node::init_leaf(self.bp.frame_mut(rf), old_next);
        for (k, v) in &entries[mid..] {
            node::leaf_put(self.bp.frame_mut(rf), k, v)?;
        }
        self.bp.mark_dirty(rf);
        self.bp.unpin(rf);

        Ok(Some((sep, right_id)))
    }

    fn split_internal(&mut self, pid: PageId, sep_in: &[u8], child_in: PageId) -> Result<Split> {
        let f = self.bp.fetch(pid)?;
        let page = self.bp.frame(f).clone();
        let leftmost = node::left_child(&page);
        self.bp.unpin(f);

        let mut items = node::internal_entries(&page);
        let pos = items
            .binary_search_by(|(k, _)| k.as_slice().cmp(sep_in))
            .unwrap_or_else(|i| i);
        items.insert(pos, (sep_in.to_vec(), child_in));

        let mid = items.len() / 2;
        let sep_up = items[mid].0.clone();
        let right_first_child = items[mid].1;
        let right_id = self.bp.disk_mut().allocate_page()?;

        let lf = self.bp.fetch(pid)?;
        node::init_internal(self.bp.frame_mut(lf), leftmost);
        for (k, c) in &items[..mid] {
            node::internal_put(self.bp.frame_mut(lf), k, *c)?;
        }
        self.bp.mark_dirty(lf);
        self.bp.unpin(lf);

        let rf = self.bp.new_page(right_id)?;
        node::init_internal(self.bp.frame_mut(rf), right_first_child);
        for (k, c) in &items[mid + 1..] {
            node::internal_put(self.bp.frame_mut(rf), k, *c)?;
        }
        self.bp.mark_dirty(rf);
        self.bp.unpin(rf);

        Ok(Some((sep_up, right_id)))
    }

    // ---- range scan -------------------------------------------------------

    fn leftmost_leaf_for(&mut self, lo: Option<&[u8]>) -> Result<PageId> {
        let mut pid = self.meta.tree_root.unwrap();
        loop {
            let f = self.bp.fetch(pid)?;
            let page = self.bp.frame(f).clone();
            self.bp.unpin(f);
            match node::read_kind(&page) {
                node::NodeKind::Leaf => return Ok(pid),
                node::NodeKind::Internal => {
                    pid = match lo {
                        Some(k) => child_for(&page, k),
                        None => node::left_child(&page),
                    };
                }
            }
        }
    }

    /// Inclusive `lo`, exclusive `hi`; `None` bounds are unbounded.
    pub fn scan(
        &mut self,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut pid = self.leftmost_leaf_for(lo)?;
        loop {
            let f = self.bp.fetch(pid)?;
            let page = self.bp.frame(f).clone();
            self.bp.unpin(f);
            for s in 1..=node::num_entries(&page) {
                let (k, stored) = node::leaf_at(&page, s);
                if let Some(l) = lo {
                    if k.as_slice() < l {
                        continue;
                    }
                }
                if let Some(h) = hi {
                    if k.as_slice() >= h {
                        return Ok(out);
                    }
                }
                let v = self.decode_value(&stored)?;
                out.push((k, v));
            }
            match node::read_next_leaf(&page) {
                Some(n) => pid = n,
                None => return Ok(out),
            }
        }
    }

    // ---- delete -----------------------------------------------------------

    /// Remove `key` from its leaf. Returns whether it existed. M1 does not merge
    /// under-full nodes (that lands with M4 `VACUUM`); the tree stays valid.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        let mut pid = self.meta.tree_root.unwrap();
        loop {
            let f = self.bp.fetch(pid)?;
            let kind = node::read_kind(self.bp.frame(f));
            match kind {
                node::NodeKind::Leaf => {
                    let (slot, found) = node::search_key(self.bp.frame(f), key);
                    if found {
                        crate::slotted::SlottedPage::new(self.bp.frame_mut(f)).remove(slot);
                        self.bp.mark_dirty(f);
                    }
                    self.bp.unpin(f);
                    return Ok(found);
                }
                node::NodeKind::Internal => {
                    let page = self.bp.frame(f).clone();
                    self.bp.unpin(f);
                    pid = child_for(&page, key);
                }
            }
        }
    }

    // ---- persistence ------------------------------------------------------

    /// Flush all dirty pages and write the current meta record to page 0.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.bp.flush_all()?;
        let mut meta_page = self.meta.encode();
        self.bp.disk_mut().write_page(PageId(0), &mut meta_page)?;
        self.bp.disk_mut().sync()
    }
}

/// Choose the child of an internal `page` that owns `key`.
fn child_for(page: &Page, key: &[u8]) -> PageId {
    if node::num_entries(page) == 0 {
        return node::left_child(page);
    }
    let (slot, found) = node::search_key(page, key);
    if found {
        return node::internal_at(page, slot).1;
    }
    if slot <= 1 {
        node::left_child(page)
    } else {
        node::internal_at(page, slot - 1).1
    }
}
