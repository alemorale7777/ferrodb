//! A node in the HNSW graph: its layer, its per-layer adjacency, and the
//! opaque row key it stands for.

/// One element of the index.
///
/// The node does **not** own its vector — vectors live contiguously in the
/// index's arena (`Hnsw::vectors`), indexed by node id, so the distance
/// kernels stream over cache-friendly memory instead of chasing per-node
/// allocations. The node owns only the graph structure.
#[derive(Clone, Debug)]
pub struct Node {
    /// Highest layer this node appears on (assigned randomly at insert;
    /// see `Hnsw::random_layer` for the `floor(-ln(u) · mL)` draw).
    pub max_layer: u8,
    /// The row's primary-key bytes in the engine's order-preserving encoding.
    /// Opaque to the index: search returns it, the B+-tree resolves it —
    /// the same contract a Postgres index has with its heap (returns TIDs).
    pub key: Vec<u8>,
    /// Tombstone. HNSW has no true delete (removing links would tear holes
    /// the paper's construction never repairs); deleted nodes stay in the
    /// graph as routing waypoints but are excluded from results. Reclaimed
    /// by an index rebuild (the engine's `VACUUM` analog).
    pub deleted: bool,
    /// `neighbors[l]` = adjacency list at layer `l`, for `0..=max_layer`.
    /// Bounded by `m_max0` at layer 0 and `m` above (enforced on insert).
    pub neighbors: Vec<Vec<u32>>,
}

impl Node {
    pub fn new(max_layer: u8, key: Vec<u8>) -> Node {
        Node {
            max_layer,
            key,
            deleted: false,
            neighbors: vec![Vec::new(); max_layer as usize + 1],
        }
    }
}
