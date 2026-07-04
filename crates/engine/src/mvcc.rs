//! Multi-version storage: a row key maps to a **version chain**, and visibility
//! is decided per snapshot. Nothing is ever overwritten in place.
//!
//! Chain bytes: `[nver:u16]` then per version
//! `[xmin:u64][xmax:u64][flags:u8][datalen:u32][data]`. `xmax == 0` means live;
//! `flags` bit0 = `xmin_committed`, bit1 = `xmax_committed` (persisted hint bits).

use crate::txn::{Snapshot, Status, TxnId, TxnManager};

#[derive(Clone, Debug)]
pub struct Version {
    pub xmin: TxnId,
    pub xmax: TxnId,
    pub xmin_committed: bool,
    pub xmax_committed: bool,
    pub data: Vec<u8>,
}

impl Version {
    pub fn new(xmin: TxnId, data: Vec<u8>) -> Self {
        Version {
            xmin,
            xmax: 0,
            xmin_committed: false,
            xmax_committed: false,
            data,
        }
    }
}

pub fn encode_chain(chain: &[Version]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(chain.len() as u16).to_le_bytes());
    for v in chain {
        out.extend_from_slice(&v.xmin.to_le_bytes());
        out.extend_from_slice(&v.xmax.to_le_bytes());
        out.push((v.xmin_committed as u8) | ((v.xmax_committed as u8) << 1));
        out.extend_from_slice(&(v.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&v.data);
    }
    out
}

pub fn decode_chain(bytes: &[u8]) -> Vec<Version> {
    let n = u16::from_le_bytes(bytes[0..2].try_into().unwrap()) as usize;
    let mut pos = 2;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let xmin = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        let xmax = u64::from_le_bytes(bytes[pos + 8..pos + 16].try_into().unwrap());
        let flags = bytes[pos + 16];
        let dlen = u32::from_le_bytes(bytes[pos + 17..pos + 21].try_into().unwrap()) as usize;
        pos += 21;
        let data = bytes[pos..pos + dlen].to_vec();
        pos += dlen;
        out.push(Version {
            xmin,
            xmax,
            xmin_committed: flags & 1 == 1,
            xmax_committed: (flags >> 1) & 1 == 1,
            data,
        });
    }
    out
}

/// Did txn `t` commit? Live txns consult the manager; pre-restart ids fall back
/// to the persisted hint bit (aborted txns never set it).
fn committed(mgr: &TxnManager, t: TxnId, hint: bool) -> bool {
    match mgr.known_status(t) {
        Some(Status::Committed) => true,
        Some(_) => false,
        None => hint,
    }
}

/// Is version `v` visible to snapshot `s`?
pub fn visible(v: &Version, s: &Snapshot, mgr: &TxnManager) -> bool {
    let created = v.xmin == s.me
        || (committed(mgr, v.xmin, v.xmin_committed)
            && !s.active.contains(&v.xmin)
            && v.xmin < s.xmax);
    if !created {
        return false;
    }
    if v.xmax == 0 {
        return true;
    }
    let deleted = v.xmax == s.me
        || (committed(mgr, v.xmax, v.xmax_committed)
            && !s.active.contains(&v.xmax)
            && v.xmax < s.xmax);
    !deleted
}

/// Index of the version visible to `s`, if any (at most one under snapshot isolation).
pub fn visible_index(chain: &[Version], s: &Snapshot, mgr: &TxnManager) -> Option<usize> {
    chain.iter().position(|v| visible(v, s, mgr))
}

/// Is `xmax` an effective deletion (committed or by a still-live txn)? An `xmax`
/// left by an aborted transaction is not — it can be overwritten.
pub fn is_live_delete(mgr: &TxnManager, xmax: TxnId, xmax_committed: bool) -> bool {
    if xmax == 0 {
        return false;
    }
    match mgr.known_status(xmax) {
        Some(Status::Aborted) => false,
        Some(_) => true,
        None => xmax_committed,
    }
}
