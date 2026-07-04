//! Transaction manager: ids, status, and snapshots for MVCC.

use std::collections::{HashMap, HashSet};

pub type TxnId = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Active,
    Committed,
    Aborted,
}

/// A consistent view of the database taken at `BEGIN`.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// The owning transaction's id.
    pub me: TxnId,
    /// Transactions with id `>= xmax` are invisible (started after this snapshot).
    pub xmax: TxnId,
    /// Transactions in progress when this snapshot was taken.
    pub active: HashSet<TxnId>,
}

pub struct TxnManager {
    next: TxnId,
    status: HashMap<TxnId, Status>,
    active: HashSet<TxnId>,
    snapshots: HashMap<TxnId, Snapshot>,
}

impl TxnManager {
    pub fn new(next: TxnId) -> Self {
        TxnManager {
            next: next.max(1),
            status: HashMap::new(),
            active: HashSet::new(),
            snapshots: HashMap::new(),
        }
    }

    /// Start a transaction, capturing its snapshot of currently-active txns.
    pub fn begin(&mut self) -> TxnId {
        let id = self.next;
        self.next += 1;
        let snap = Snapshot {
            me: id,
            xmax: id,
            active: self.active.clone(),
        };
        self.status.insert(id, Status::Active);
        self.active.insert(id);
        self.snapshots.insert(id, snap);
        id
    }

    pub fn snapshot(&self, id: TxnId) -> &Snapshot {
        &self.snapshots[&id]
    }

    pub fn commit(&mut self, id: TxnId) {
        self.status.insert(id, Status::Committed);
        self.active.remove(&id);
        self.snapshots.remove(&id);
    }

    pub fn abort(&mut self, id: TxnId) {
        self.status.insert(id, Status::Aborted);
        self.active.remove(&id);
        self.snapshots.remove(&id);
    }

    /// Status of a txn this manager knows about; `None` for ids from before restart.
    pub fn known_status(&self, id: TxnId) -> Option<Status> {
        self.status.get(&id).copied()
    }

    pub fn is_active(&self, id: TxnId) -> bool {
        self.active.contains(&id)
    }

    /// The smallest active transaction id (the vacuum horizon), or `next` if none.
    pub fn oldest_active(&self) -> TxnId {
        self.active.iter().copied().min().unwrap_or(self.next)
    }
}
