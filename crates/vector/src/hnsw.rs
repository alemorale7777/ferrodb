//! HNSW: Hierarchical Navigable Small World graphs.
//!
//! Implements Malkov & Yashunin, *"Efficient and robust approximate nearest
//! neighbor search using Hierarchical Navigable Small World graphs"* (2016,
//! TPAMI 2018) — the algorithm behind pgvector, Qdrant, Weaviate, Milvus and
//! FAISS's `IndexHNSWFlat`. Algorithm numbers in comments refer to the paper.
//!
//! # Shape of the thing
//!
//! A stack of proximity graphs. Layer 0 contains every element; each higher
//! layer keeps roughly `1/M` of the layer below (each element draws its top
//! layer from `floor(-ln(unif(0,1)) · mL)` with `mL = 1/ln(M)`). Search
//! descends greedily through the sparse top layers (`ef = 1` — just "step to
//! the closest neighbor until stuck"), then runs a beam search of width
//! `ef_search` on layer 0. The layers are a skip list's express lanes,
//! generalized from a sorted line to a metric space; expected search cost is
//! logarithmic-ish in element count for the descent plus an `ef`-bounded
//! local exploration.
//!
//! # Where we follow the paper and where we deviate
//!
//! - Insert is Algorithm 1, search-layer is Algorithm 2, neighbor selection
//!   is the **heuristic** Algorithm 4 (with `keepPrunedConnections`), search
//!   is Algorithm 5. Defaults `M = 16`, `ef_construction = 200`,
//!   `Mmax0 = 2M` follow the paper's recommendations.
//! - `extendCandidates` (Alg. 4's optional candidate-set expansion) is off,
//!   as the paper itself recommends for anything but extremely clustered
//!   data; the hook is where filtered search will plug in instead.
//! - The paper has no delete; we add tombstones (nodes keep routing, stop
//!   appearing in results) because a database needs `DELETE` to mean
//!   something before `VACUUM`/rebuild reclaims the node.
//!
//! # Why the neighbor-selection heuristic (the interview question)
//!
//! Naive "link to the M nearest" fails on clustered data: inside a cluster
//! all M links point at cluster-mates, so the graph between clusters is
//! sparse or disconnected, and greedy search that starts in the wrong
//! cluster can never cross. Algorithm 4 keeps candidate `e` only if it is
//! closer to the query than to every neighbor already kept — a cheap
//! "does this link cover a *new direction*?" test (it approximates the
//! relative-neighborhood graph). The result is fewer redundant same-cluster
//! links and reliable long-range bridges, which is what keeps recall high.

use crate::distance::{kernels, Kernels, Metric};
use crate::node::Node;
use crate::persistence::VectorStore;

use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::{BinaryHeap, HashMap};

/// `f32` distance with a total order, usable inside a `BinaryHeap`.
/// (`f32` itself is not `Ord` because of NaN; the kernels never produce NaN —
/// see the zero-vector guard in `distance.rs` — and `total_cmp` makes that a
/// type-level fact rather than a hope.)
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Dist(pub f32);

impl Eq for Dist {}
impl PartialOrd for Dist {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Dist {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Build/search parameters. `m` and `ef_construction` are fixed at build
/// time (they shape the graph); `ef` is per-query (the recall/latency knob).
#[derive(Clone, Copy, Debug)]
pub struct HnswParams {
    /// Max links per node on layers ≥ 1; also drives `mL = 1/ln(m)`.
    pub m: usize,
    /// Max links per node on layer 0 (paper default: `2 * m`).
    pub m_max0: usize,
    /// Beam width while building. Higher = better graph, slower inserts.
    pub ef_construction: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        HnswParams {
            m: 16,
            m_max0: 32,
            ef_construction: 200,
        }
    }
}

/// An in-memory HNSW index over fixed-dimension `f32` vectors.
///
/// Not internally synchronized — see [`ConcurrentHnsw`] for the
/// reader-parallel wrapper. Node ids are dense `u32`s in insertion order.
pub struct Hnsw {
    pub(crate) dim: usize,
    pub(crate) metric: Metric,
    pub(crate) params: HnswParams,
    /// `1 / ln(m)` — the layer-assignment scale factor from the paper.
    pub(crate) ml: f64,
    pub(crate) kern: Kernels,
    /// All vectors, dim-strided: node `i` occupies `[i*dim .. (i+1)*dim]`.
    /// Owned in memory after inserts; possibly mmap-backed straight off disk
    /// after a load (see `persistence`) — reads don't care which.
    pub(crate) vectors: VectorStore,
    pub(crate) nodes: Vec<Node>,
    /// The global entry point: a node on the highest occupied layer.
    pub(crate) entry: Option<u32>,
    /// xorshift64* state for layer draws (deterministic per seed — tests and
    /// the recall harness rely on reproducible builds).
    pub(crate) rng: u64,
    /// Count of tombstoned nodes (for stats / rebuild heuristics).
    pub(crate) tombstones: usize,
}

impl Hnsw {
    pub fn new(dim: usize, metric: Metric, params: HnswParams, seed: u64) -> Hnsw {
        assert!(dim > 0, "dimension must be positive");
        assert!(params.m >= 2, "m must be at least 2");
        Hnsw {
            dim,
            metric,
            params,
            ml: 1.0 / (params.m as f64).ln(),
            kern: kernels(),
            vectors: VectorStore::new(),
            nodes: Vec::new(),
            entry: None,
            rng: seed | 1, // xorshift state must be nonzero
            tombstones: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn dim(&self) -> usize {
        self.dim
    }
    pub fn metric(&self) -> Metric {
        self.metric
    }
    pub fn params(&self) -> HnswParams {
        self.params
    }
    pub fn tombstones(&self) -> usize {
        self.tombstones
    }

    /// The vector stored for node `id`.
    pub fn vector(&self, id: u32) -> &[f32] {
        let i = id as usize * self.dim;
        &self.vectors.as_slice()[i..i + self.dim]
    }

    /// The row key stored for node `id`.
    pub fn key(&self, id: u32) -> &[u8] {
        &self.nodes[id as usize].key
    }

    fn dist_to(&self, q: &[f32], id: u32) -> f32 {
        self.kern.distance(self.metric, q, self.vector(id))
    }

    fn dist_between(&self, a: u32, b: u32) -> f32 {
        self.kern
            .distance(self.metric, self.vector(a), self.vector(b))
    }

    /// Draw a top layer: `floor(-ln(u) · mL)`, `u ~ unif(0,1]`. Geometric
    /// decay — each layer keeps ~`1/m` of the one below, the skip-list shape.
    fn random_layer(&mut self) -> u8 {
        // xorshift64*: tiny, deterministic, no rand crate (repo ethos).
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let u = ((self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64)
            .max(f64::MIN_POSITIVE); // u = 0 would give ln(0) = -inf
        let l = (-u.ln() * self.ml).floor();
        // Cap: with m=16, P(layer > 15) < 16^-15 — the cap never fires in
        // practice, it just keeps a pathological RNG from allocating layers.
        l.min(15.0) as u8
    }

    // ---- Algorithm 2: search one layer -------------------------------------

    /// Beam search on `layer` from `entry_points`, beam width `ef`.
    /// Returns up to `ef` (distance, id) pairs, ascending by distance.
    ///
    /// `admit` decides whether a node may enter the *result* set; every node
    /// is still traversable regardless (tombstones and filtered search both
    /// need "route through, don't return" semantics — dropping nodes from
    /// traversal is what causes the post-filter recall cliff).
    fn search_layer(
        &self,
        q: &[f32],
        entry_points: &[u32],
        ef: usize,
        layer: usize,
        admit: &mut dyn FnMut(&Hnsw, u32) -> bool,
    ) -> Vec<(f32, u32)> {
        // visited guards traversal; candidates is a min-heap (closest first);
        // results is a max-heap (worst-kept first) capped at ef.
        let mut visited: HashMap<u32, ()> = HashMap::new();
        let mut candidates: BinaryHeap<Reverse<(Dist, u32)>> = BinaryHeap::new();
        let mut results: BinaryHeap<(Dist, u32)> = BinaryHeap::new();

        for &ep in entry_points {
            if let Entry::Vacant(v) = visited.entry(ep) {
                v.insert(());
                let d = self.dist_to(q, ep);
                candidates.push(Reverse((Dist(d), ep)));
                if admit(self, ep) {
                    results.push((Dist(d), ep));
                }
            }
        }

        while let Some(Reverse((Dist(cd), c))) = candidates.pop() {
            // Stop when the closest unexplored candidate is farther than the
            // worst kept result and the beam is full: the frontier can no
            // longer improve the result set (greedy termination, Alg. 2).
            if results.len() >= ef {
                if let Some(&(Dist(worst), _)) = results.peek() {
                    if cd > worst {
                        break;
                    }
                }
            }
            for &e in &self.nodes[c as usize].neighbors[layer] {
                if let Entry::Vacant(v) = visited.entry(e) {
                    v.insert(());
                    let d = self.dist_to(q, e);
                    let worst = results.peek().map(|&(Dist(w), _)| w);
                    if results.len() < ef || d < worst.unwrap_or(f32::INFINITY) {
                        candidates.push(Reverse((Dist(d), e)));
                        if admit(self, e) {
                            results.push((Dist(d), e));
                            if results.len() > ef {
                                results.pop();
                            }
                        }
                    }
                }
            }
        }

        let mut out: Vec<(f32, u32)> = results.into_iter().map(|(Dist(d), id)| (d, id)).collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }

    /// Greedy descent step used above the target layer: `ef = 1` beam,
    /// admitting everything (routing doesn't care about tombstones).
    fn greedy_step(&self, q: &[f32], ep: u32, layer: usize) -> u32 {
        let mut admit_all = |_: &Hnsw, _: u32| true;
        self.search_layer(q, &[ep], 1, layer, &mut admit_all)
            .first()
            .map(|&(_, id)| id)
            .unwrap_or(ep)
    }

    // ---- Algorithm 4: heuristic neighbor selection --------------------------

    /// Pick up to `m` neighbors for a node with vector `q_vec` from
    /// `candidates` (ascending by distance to it).
    ///
    /// A candidate is kept only if it is closer to the new node than to every
    /// neighbor already kept — i.e. it covers a direction no kept neighbor
    /// covers. Rejected candidates are kept aside and used to top up to `m`
    /// (the paper's `keepPrunedConnections`), so nodes in dense regions still
    /// get their full link budget.
    fn select_neighbors(&self, candidates: &[(f32, u32)], m: usize) -> Vec<u32> {
        let mut kept: Vec<(f32, u32)> = Vec::with_capacity(m);
        let mut pruned: Vec<(f32, u32)> = Vec::new();
        for &(d, e) in candidates {
            if kept.len() >= m {
                break;
            }
            let covers_new_direction = kept.iter().all(|&(_, r)| d < self.dist_between(e, r));
            if covers_new_direction {
                kept.push((d, e));
            } else {
                pruned.push((d, e));
            }
        }
        // keepPrunedConnections: fill remaining slots with the best rejects.
        let mut fill = pruned.into_iter();
        while kept.len() < m {
            match fill.next() {
                Some(p) => kept.push(p),
                None => break,
            }
        }
        kept.into_iter().map(|(_, id)| id).collect()
    }

    /// Re-select `node`'s neighbors at `layer` after a new link pushed it
    /// over its budget (`m_max0` at layer 0, `m` above).
    fn shrink_neighbors(&mut self, node: u32, layer: usize) {
        let limit = if layer == 0 {
            self.params.m_max0
        } else {
            self.params.m
        };
        if self.nodes[node as usize].neighbors[layer].len() <= limit {
            return;
        }
        let nvec: Vec<f32> = self.vector(node).to_vec();
        let mut cands: Vec<(f32, u32)> = self.nodes[node as usize].neighbors[layer]
            .iter()
            .map(|&e| (self.kern.distance(self.metric, &nvec, self.vector(e)), e))
            .collect();
        cands.sort_by(|a, b| a.0.total_cmp(&b.0));
        let selected = self.select_neighbors(&cands, limit);
        self.nodes[node as usize].neighbors[layer] = selected;
    }

    // ---- Algorithm 1: insert ------------------------------------------------

    /// Insert a vector with its row key; returns the new node's id.
    ///
    /// Panics if `v.len() != dim` — the engine validates dimensions at the
    /// type level (`Vector(dim)` columns) before reaching the index.
    pub fn insert(&mut self, v: &[f32], key: &[u8]) -> u32 {
        assert_eq!(v.len(), self.dim, "vector dimension mismatch");
        let id = self.nodes.len() as u32;
        let l = self.random_layer();
        self.vectors.extend(v);
        self.nodes.push(Node::new(l, key.to_vec()));

        let Some(mut ep) = self.entry else {
            self.entry = Some(id); // first element: it *is* the graph
            return id;
        };

        let top = self.nodes[ep as usize].max_layer;

        // Phase A (paper lines 5–7): ride the express lanes down to l+1.
        let mut lc = top as usize;
        while lc > l as usize {
            ep = self.greedy_step(v, ep, lc);
            lc -= 1;
        }

        // Phase B (lines 8–17): from min(top, l) down to 0, beam-search the
        // neighborhood, pick diverse links, connect bidirectionally, and
        // shrink anyone who went over budget.
        let mut eps = vec![ep];
        let mut admit_all = |_: &Hnsw, _: u32| true;
        for lc in (0..=(l.min(top) as usize)).rev() {
            let w = self.search_layer(v, &eps, self.params.ef_construction, lc, &mut admit_all);
            let neighbors = self.select_neighbors(&w, self.params.m);
            for &n in &neighbors {
                self.nodes[id as usize].neighbors[lc].push(n);
                self.nodes[n as usize].neighbors[lc].push(id);
                self.shrink_neighbors(n, lc);
            }
            // Next layer starts from this layer's whole beam (paper line 16).
            eps = w.into_iter().map(|(_, i)| i).collect();
            if eps.is_empty() {
                eps = vec![ep];
            }
        }

        if l > top {
            self.entry = Some(id); // new highest layer: new global entry
        }
        id
    }

    // ---- Algorithm 5: k-NN search --------------------------------------------

    /// Approximate k-nearest-neighbors: `(distance, node id)` ascending.
    /// `ef` is the layer-0 beam width — the recall/latency knob; it is
    /// clamped up to `k` since a beam narrower than `k` cannot hold `k`
    /// results. Tombstoned nodes route but are never returned.
    pub fn search(&self, q: &[f32], k: usize, ef: usize) -> Vec<(f32, u32)> {
        let mut admit_live = |h: &Hnsw, id: u32| !h.nodes[id as usize].deleted;
        self.search_internal(q, k, ef, &mut admit_live)
    }

    /// Filtered k-NN: like [`search`](Hnsw::search), but a result must also
    /// satisfy `pass` (given the node id and its row key). Non-passing nodes
    /// are still *traversed* — filtering the traversal itself is what causes
    /// the post-filter recall cliff, because the beam loses its stepping
    /// stones through non-matching regions.
    ///
    /// With very selective predicates the beam may still fill with fewer
    /// than `k` passing nodes; the engine escalates `ef` or falls back to a
    /// pre-filtered exact scan (see the planner). The index stays honest and
    /// returns what the beam found.
    pub fn search_filtered(
        &self,
        q: &[f32],
        k: usize,
        ef: usize,
        pass: &mut dyn FnMut(u32, &[u8]) -> bool,
    ) -> Vec<(f32, u32)> {
        let mut admit = |h: &Hnsw, id: u32| {
            !h.nodes[id as usize].deleted && pass(id, &h.nodes[id as usize].key)
        };
        self.search_internal(q, k, ef, &mut admit)
    }

    fn search_internal(
        &self,
        q: &[f32],
        k: usize,
        ef: usize,
        admit: &mut dyn FnMut(&Hnsw, u32) -> bool,
    ) -> Vec<(f32, u32)> {
        assert_eq!(q.len(), self.dim, "query dimension mismatch");
        let Some(mut ep) = self.entry else {
            return Vec::new();
        };
        // Greedy descent to layer 1 (ef = 1), then the real beam at layer 0.
        for lc in (1..=self.nodes[ep as usize].max_layer as usize).rev() {
            ep = self.greedy_step(q, ep, lc);
        }
        let mut out = self.search_layer(q, &[ep], ef.max(k), 0, admit);
        out.truncate(k);
        out
    }

    /// Exact k-NN by scanning every live vector — O(n·dim). The ground truth
    /// the recall harness measures the graph against, and the correctness
    /// oracle for tests. Also the engine's pre-filter path (scan the few
    /// rows matching a selective predicate, rank them exactly).
    pub fn exact_search(&self, q: &[f32], k: usize) -> Vec<(f32, u32)> {
        let mut all: Vec<(f32, u32)> = (0..self.nodes.len() as u32)
            .filter(|&id| !self.nodes[id as usize].deleted)
            .map(|id| (self.dist_to(q, id), id))
            .collect();
        all.sort_by(|a, b| a.0.total_cmp(&b.0));
        all.truncate(k);
        all
    }

    /// Tombstone the node holding `key` (linear scan; the engine tracks
    /// key→id if it needs this hot). Returns whether a live node was found.
    pub fn delete_by_key(&mut self, key: &[u8]) -> bool {
        for n in &mut self.nodes {
            if !n.deleted && n.key == key {
                n.deleted = true;
                self.tombstones += 1;
                return true;
            }
        }
        false
    }

    /// How many nodes a layer-0 BFS from the entry point can reach.
    /// Diagnostic: `reachable < len` means some vectors can never be
    /// returned no matter how large `ef` is — the recall ceiling.
    pub fn reachable_from_entry(&self) -> usize {
        let Some(ep) = self.entry else { return 0 };
        let mut seen = vec![false; self.nodes.len()];
        let mut stack = vec![ep];
        seen[ep as usize] = true;
        let mut count = 1;
        while let Some(n) = stack.pop() {
            for &e in &self.nodes[n as usize].neighbors[0] {
                if !seen[e as usize] {
                    seen[e as usize] = true;
                    count += 1;
                    stack.push(e);
                }
            }
        }
        count
    }

    /// Estimated resident bytes (vector arena + adjacency + keys).
    pub fn memory_bytes(&self) -> usize {
        let vec_bytes = self.vectors.len() * 4;
        let graph_bytes: usize = self
            .nodes
            .iter()
            .map(|n| n.key.len() + n.neighbors.iter().map(|l| l.len() * 4).sum::<usize>() + 48)
            .sum();
        vec_bytes + graph_bytes
    }
}

// ---- concurrency wrapper -----------------------------------------------------

use std::sync::RwLock;

/// A reader-parallel HNSW: many concurrent searches OR one insert.
///
/// Why `RwLock` and not finer-grained/lock-free: searches are pure reads, so
/// a read lock gives genuine multi-core search throughput with zero unsafe
/// and zero dependencies. Per-node locking (what hnswlib does) or epoch-based
/// reclamation would also parallelize *inserts*, but inserts mutate several
/// nodes' adjacency lists at once (bidirectional links + shrinks), and Rust
/// makes the cost of getting that wrong very visible. Within ferrodb the
/// engine currently serializes writers anyway (`Arc<Mutex<Database>>` in
/// pgwire), so writer parallelism would be theater. Honest scope: readers
/// scale, writers queue.
pub struct ConcurrentHnsw {
    inner: RwLock<Hnsw>,
}

impl ConcurrentHnsw {
    pub fn new(index: Hnsw) -> ConcurrentHnsw {
        ConcurrentHnsw {
            inner: RwLock::new(index),
        }
    }

    pub fn search(&self, q: &[f32], k: usize, ef: usize) -> Vec<(f32, u32)> {
        self.inner
            .read()
            .expect("hnsw lock poisoned")
            .search(q, k, ef)
    }

    pub fn insert(&self, v: &[f32], key: &[u8]) -> u32 {
        self.inner
            .write()
            .expect("hnsw lock poisoned")
            .insert(v, key)
    }

    /// Run `f` with shared access to the underlying index.
    pub fn read<R>(&self, f: impl FnOnce(&Hnsw) -> R) -> R {
        f(&self.inner.read().expect("hnsw lock poisoned"))
    }

    /// Run `f` with exclusive access to the underlying index.
    pub fn write<R>(&self, f: impl FnOnce(&mut Hnsw) -> R) -> R {
        f(&mut self.inner.write().expect("hnsw lock poisoned"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_index(n: usize) -> Hnsw {
        // n points on a 1-D line embedded in 2-D: exact NN is obvious.
        let mut h = Hnsw::new(2, Metric::L2, HnswParams::default(), 42);
        for i in 0..n {
            h.insert(&[i as f32, 0.0], &(i as u64).to_be_bytes());
        }
        h
    }

    #[test]
    fn empty_index_returns_nothing() {
        let h = Hnsw::new(4, Metric::L2, HnswParams::default(), 1);
        assert!(h.search(&[0.0; 4], 5, 50).is_empty());
    }

    #[test]
    fn single_element_is_found() {
        let mut h = Hnsw::new(3, Metric::L2, HnswParams::default(), 1);
        h.insert(&[1.0, 2.0, 3.0], b"k1");
        let r = h.search(&[1.0, 2.0, 3.0], 1, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, 0);
        assert_eq!(h.key(0), b"k1");
    }

    #[test]
    fn finds_exact_nearest_on_a_line() {
        let h = grid_index(200);
        for probe in [0.2f32, 57.4, 99.9, 150.1, 199.0] {
            let r = h.search(&[probe, 0.0], 3, 64);
            let expect = probe.round().clamp(0.0, 199.0) as u32;
            assert_eq!(r[0].1, expect, "probe {probe}");
        }
    }

    #[test]
    fn search_matches_exact_search_on_small_set() {
        let h = grid_index(300);
        let got = h.search(&[123.4, 0.0], 10, 128);
        let want = h.exact_search(&[123.4, 0.0], 10);
        let g: Vec<u32> = got.iter().map(|&(_, i)| i).collect();
        let w: Vec<u32> = want.iter().map(|&(_, i)| i).collect();
        assert_eq!(g, w);
    }

    #[test]
    fn k_larger_than_len_returns_everything() {
        let h = grid_index(5);
        assert_eq!(h.search(&[2.0, 0.0], 50, 50).len(), 5);
    }

    #[test]
    fn layer0_is_fully_connected_from_entry() {
        // BFS at layer 0 must reach every node: a disconnected base layer
        // means some vectors are unfindable — the failure mode the neighbor
        // heuristic exists to prevent.
        let h = grid_index(500);
        let mut seen = vec![false; h.len()];
        let mut stack = vec![h.entry.unwrap()];
        seen[h.entry.unwrap() as usize] = true;
        while let Some(n) = stack.pop() {
            for &e in &h.nodes[n as usize].neighbors[0] {
                if !seen[e as usize] {
                    seen[e as usize] = true;
                    stack.push(e);
                }
            }
        }
        assert!(seen.iter().all(|&s| s), "layer 0 must be connected");
    }

    #[test]
    fn degree_bounds_are_respected() {
        let h = grid_index(500);
        for n in &h.nodes {
            for (l, adj) in n.neighbors.iter().enumerate() {
                let limit = if l == 0 { h.params.m_max0 } else { h.params.m };
                assert!(adj.len() <= limit, "layer {l} degree {}", adj.len());
            }
        }
    }

    #[test]
    fn layer_distribution_decays_geometrically() {
        let h = grid_index(2000);
        let mut counts = [0usize; 16];
        for n in &h.nodes {
            counts[n.max_layer as usize] += 1;
        }
        // With mL = 1/ln(16), P(layer ≥ 1) = 1/16: expect ~125 of 2000.
        // Loose bounds — this asserts the shape, not the exact draw.
        assert!(counts[0] > 1700, "layer0-only count: {}", counts[0]);
        let above: usize = counts[1..].iter().sum();
        assert!((40..=320).contains(&above), "nodes above layer 0: {above}");
    }

    #[test]
    fn deleted_nodes_route_but_do_not_return() {
        let mut h = grid_index(100);
        let key = 50u64.to_be_bytes();
        assert!(h.delete_by_key(&key));
        let r = h.search(&[50.0, 0.0], 3, 64);
        assert!(r.iter().all(|&(_, id)| id != 50), "tombstone returned");
        // Its neighbors are still reachable *through* it.
        let ids: Vec<u32> = r.iter().map(|&(_, i)| i).collect();
        assert!(ids.contains(&49) || ids.contains(&51));
        assert_eq!(h.tombstones(), 1);
    }

    #[test]
    fn filtered_search_respects_predicate() {
        let h = grid_index(200);
        // Only even ids pass.
        let mut pass = |id: u32, _key: &[u8]| id.is_multiple_of(2);
        let r = h.search_filtered(&[100.3, 0.0], 5, 128, &mut pass);
        assert_eq!(r.len(), 5);
        assert!(r.iter().all(|&(_, id)| id.is_multiple_of(2)));
        assert_eq!(r[0].1, 100); // nearest even to 100.3
    }

    #[test]
    fn concurrent_searches_share_the_index() {
        use std::sync::Arc;
        let h = Arc::new(ConcurrentHnsw::new(grid_index(300)));
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let h = Arc::clone(&h);
                std::thread::spawn(move || {
                    for i in 0..50 {
                        let probe = ((t * 50 + i) % 300) as f32;
                        let r = h.search(&[probe, 0.0], 1, 32);
                        assert_eq!(r[0].1, probe as u32);
                    }
                })
            })
            .collect();
        for j in handles {
            j.join().unwrap();
        }
    }

    #[test]
    fn deterministic_given_seed() {
        let a = grid_index(100);
        let b = grid_index(100);
        for (x, y) in a.nodes.iter().zip(&b.nodes) {
            assert_eq!(x.max_layer, y.max_layer);
            assert_eq!(x.neighbors, y.neighbors);
        }
    }
}
