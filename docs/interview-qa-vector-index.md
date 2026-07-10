# M9 Vector Index — Interview Q&A Bank

Rehearsal answers for a skeptical staff engineer. Every answer is true of
*this* codebase — file references included so you can pull the code up live.

---

**1. Why HNSW and not IVF or a flat index?**
Flat is exact but O(n) per query — it's my ground truth (`exact_search`), not
my index. IVF partitions into Voronoi cells and probes a few; it's simpler
but has a coarse recall/latency frontier and needs retraining as data drifts.
HNSW gives a smoothly tunable frontier through one query-time knob
(`ef_search`), no training phase, and incremental inserts — and it's what
pgvector, Qdrant, Weaviate, and FAISS's `IndexHNSWFlat` ship, so its
behavior is a shared vocabulary in any team I'd join.

**2. Walk me through what happens on `INSERT` into an indexed table.**
`exec_insert` coerces the `'[...]'` literal to `Value::Vector`, validating
dimension against `VECTOR(dim)` — the dimension is part of the type. The row
version goes into the B+-tree chain; then `hnsw_after_insert` mirrors the
vector into the graph tagged with the row's key bytes. Layer assignment draws
`floor(-ln(u)·mL)`; insert descends greedily to the target layer, beam-
searches each layer with `ef_construction`, selects M diverse neighbors
(Algorithm 4), links bidirectionally, and shrinks any node over its degree
budget (`crates/vector/src/hnsw.rs::insert`).

**3. How does layer assignment work, and why is search logarithmic-ish?**
`l = floor(-ln(unif(0,1)) · mL)`, `mL = 1/ln(M)` — a geometric distribution:
P(layer ≥ 1) = 1/M, P(≥ 2) = 1/M², so each layer has ~1/M the nodes below.
That's a skip list's shape in metric space: descending the sparse layers
takes O(log n) expected hops to reach the right neighborhood, then layer 0
does an ef-bounded local search. My harness asserts the decay empirically
(`layer_distribution_decays_geometrically`).

**4. What does the neighbor-selection heuristic do that naive top-M doesn't?**
Naive top-M clusters links: inside a dense cluster all M links stay in the
cluster, so the inter-cluster graph thins out and greedy search that starts
in the wrong cluster can never cross. Algorithm 4 admits a candidate only if
it's closer to the new node than to any already-kept neighbor — cheap "new
direction?" test, approximating the relative neighborhood graph. Kept: close
*diverse* links plus long bridges. Rejects back-fill spare slots
(`keepPrunedConnections`) so dense-region nodes still use their budget.

**5. Your recall was mediocre at first. What happened?**
Two investigations, both documented in the harness. First I suspected
unreachable nodes — shrinking can drop a node's only in-link — so I wrote a
BFS diagnostic (`reachable_from_entry`); it showed 20,000/20,000 reachable.
The real bug was in my *measurement*: recall counted by node id, and my
synthetic dataset contained duplicate points (random center assignment with
replacement), so the index returned points at the exact same distance and
got scored as misses. Counting hits by distance threshold — what
ann-benchmarks does — fixed the metric: recall@10 = 1.000 on the easy
regime, honest 0.5→0.99 curve on adversarial clustered data. The lesson I
took: validate the ruler before blaming the instrument.

**6. What's `ef_search` and how do I choose it?**
The layer-0 beam width — the only per-query knob. The beam keeps the ef best
candidates seen; search stops when the nearest unexplored candidate is worse
than the worst kept result. Bigger ef = wider exploration = higher recall,
linearly more distance computations. My curve: ef=40 → 0.78 recall @ 73µs;
ef=320 → 0.99 @ 390µs. You pick per workload; RAG typically wants ≥0.95.

**7. How do concurrent reads and writes work?**
The crate exposes `ConcurrentHnsw`: an `RwLock` — many parallel searches or
one insert, zero unsafe, verified by a 4-thread test. I deliberately did not
do per-node locking or epoch reclamation: inserts mutate several adjacency
lists at once (bidirectional links + shrinks), and inside ferrodb writers are
already serialized behind `Arc<Mutex<Database>>` in the pgwire server, so
writer parallelism in the index would be theater. Honest scope: readers
scale, writers queue.

**8. Why does the index keep its own copy of the vectors?**
Search evaluates hundreds to thousands of distances; through the table each
would be a B+-tree descent plus (for a 3 KB vector) an overflow-chain walk —
multiple page fetches, checksums, decodes, microseconds each. Against the
contiguous arena it's an offset plus a 54 ns SIMD kernel. That's a ~1000×
hot-loop gap for a 2× memory cost on vector data. Every serious system
(FAISS, hnswlib, pgvector in its own way) makes the same trade.

**9. How is MVCC handled? Can a search see uncommitted data?**
The index is deliberately not MVCC-aware — same as Postgres indexes. It can
return keys for rows your snapshot can't see (aborted inserts, in-flight
writers); the B+-tree fetch runs `mvcc::visible_index` per chain and drops
them. My end-to-end test inserts in a transaction, rolls back, and proves the
ghost never surfaces (`aborted_insert_is_a_ghost_the_search_never_returns`).
Cost: ghosts waste a little beam budget until rebuild reclaims them.

**10. How do deletes work? HNSW has no delete in the paper.**
Tombstones. The node stays in the graph as a routing waypoint — ripping out
links tears holes the construction never repairs — but is excluded from
results (the `admit` closure in `search_layer`). Tombstones survive
persistence, and a rebuild (the engine's `REINDEX` analog) reclaims them.
Test: `deleted_nodes_route_but_do_not_return`.

**11. Filtered search: pre-filter vs post-filter, and the recall cliff?**
Post-filtering (top-k, then apply the predicate) collapses when the predicate
is selective: with 1% selectivity the unfiltered top-10 likely contains zero
matches — that's the cliff. Pre-filtering (scan matching rows, brute-force
rank) is exact and wins when the predicate is very selective, but is O(n) in
matches. I filter *candidate admission, not traversal*: non-matching nodes
still route the beam, matching ones fill the result set, and the admission
test resolves the row through the B+-tree under the query snapshot — so
predicate + visibility + search share a pass. If results still come up short,
`ef` escalates ×4 until the beam spans the graph, degenerating gracefully
into an exhaustive filtered scan. Test: one matching needle among 300 rows,
found (`ultra_selective_filter_still_finds_the_needle`).

**12. Why is there a Sort node above your index scan in EXPLAIN?**
Contract clarity: the index only proposes candidates. Sort re-orders them by
the exact `distance()` expression (same kernels), Limit truncates. So
approximation can *lose* a candidate but never *misorder* results — and the
exact and indexed paths agree by construction, which the tests exploit by
comparing them directly.

**13. Why a sidecar file instead of storing the graph in your 4 KiB pager?**
Graph traversal is random access across the whole structure; forcing it into
pages either thrashes the buffer pool or requires WAL-logging every adjacency
mutation — that's pgvector's design and it's a milestone of engineering on
its own. My index is *derived data*: the WAL-protected table can always
regenerate it, so the sidecar is just a warm-start checkpoint. Torn, corrupt,
or stale sidecar → rebuild. I traded crash-recovery time for enormous
implementation simplicity, and I can name exactly where that trade stops
working (see Q16).

**14. How do you know the sidecar isn't stale after a crash?**
Freshness check at load: the index must hold exactly one node per row key, so
its node count must equal the table's chain count. A sidecar written before
the last inserts undercounts → discarded → rebuild. Tested by inserting
after a checkpoint, reopening, and asserting the new row is findable
(`stale_sidecar_is_rebuilt_not_trusted`). Torn files fail the FNV-1a graph
checksum; garbage fails magic.

**15. Why does your file checksum skip the vector arena?**
The arena is mmap'd for lazy cold-start: pages fault in on first touch.
Checksumming it at open would read every page — the exact cost mmap avoids.
The graph section (small, always needed) is verified; the arena is length-
checked. In ferrodb proper the source vectors live in CRC32C-checksummed
table pages anyway, and the index can always rebuild from them.

**16. What breaks at 10M vectors, and what would you do?**
Memory: 10M × 768 f32 = ~31 GB of vectors plus ~2–4 GB adjacency; my design
needs the graph resident and the arena at least page-cache-warm. Fixes in
order: (1) scalar quantization — int8 arena, 4× smaller, small recall cost;
(2) product quantization — 16–64× compression, FAISS-style, with reranking
over exact vectors for the final k; (3) the pgvector move — page the graph
through the buffer pool with WAL integration, which also fixes rebuild-time
recovery. Also: my rebuild is O(n log n) at that scale — tens of minutes —
so incremental checkpointing of the graph becomes worth its complexity.

**17. Why hand-written AVX2 intrinsics instead of a SIMD crate or std::simd?**
`std::simd` is nightly; this repo pledges stable. The `wide` crate is safe
but this project's entire premise is building the machine, not gluing crates
— the same reason there's no sqlparser or serde anywhere. The unsafe surface
is three kernels behind one runtime `is_x86_feature_detected!` gate, each
with documented obligations (feature presence, in-bounds unaligned loads,
scalar tail for `len % 8`), and property tests pin them to the scalar
reference within relative epsilon.

**18. Why does the SIMD result differ from scalar at all?**
IEEE 754 addition isn't associative. The AVX2 kernel keeps 8 partial sums
folded at the end; scalar sums left-to-right. Different association, different
rounding. The property test asserts *closeness*, and the comment notes that
bit-exact agreement would be evidence the SIMD path silently fell back.

**19. Squared L2? Negated dot? Why?**
Only comparisons matter to a k-NN index, and `sqrt` is monotone — skipping it
saves a hot-loop op with zero effect on ordering. Dot product is a
*similarity* (bigger = closer); negating makes all three metrics agree that
smaller = closer, so the graph code never branches on metric.

**20. What's the normalize-on-insert trick?**
For cosine workloads, store unit vectors: cosine distance of originals equals
`1 − dot` of normalized, and dot is my fastest kernel (one FMA chain vs
cosine's three). pgvector does the same for `vector_cosine_ops`. A property
test proves ranking equivalence (`normalized_dot_orders_like_cosine`).

**21. How does a zero vector not poison the graph?**
Cosine of a zero vector is 0/0 = NaN, and one NaN in a metric silently breaks
every heap comparison after it. The kernel defines zero-vector cosine
distance as 1.0 (no direction ⇒ maximally distant) and the `Dist` wrapper
uses `total_cmp`, making "no NaN" a typed fact rather than a hope.

**22. Your `unsafe` blocks — justify each.**
Three sites, all in `crates/vector`: (1) AVX2 kernels — obligation is CPU
feature presence, proven by the runtime gate before any pointer to them
escapes, plus in-bounds unaligned loads with a safe scalar tail; (2)
`mmap`/`munmap` FFI — live fd, page-aligned offset enforced by the file
format, MAP_FAILED checked, unmapped exactly once in Drop; (3)
`from_raw_parts` over the mapping — valid for the mapping's lifetime (`&self`
proves no Drop), page alignment ⇒ f32 alignment, every bit pattern a valid
f32. `Send`/`Sync` for the mapping are justified by PROT_READ + MAP_PRIVATE
immutability.

**23. How would `VACUUM` interact with the vector index?**
Today: it doesn't (documented limit). The right behavior: after version-chain
reclamation, rebuild indexes whose tombstone ratio crosses a threshold —
tombstones waste beam budget, so recall-per-microsecond degrades as they
accumulate. Rebuild is exactly the sidecar-miss path that already exists.

**24. UPDATE of an indexed vector column?**
Delete + insert under the same row key: tombstone the old node, insert the
new vector (`exec_update`'s reindex branch). The old node keeps routing until
rebuild. Test: move a row's embedding across the space, assert it stops
matching its old neighborhood and starts matching the new one.

**25. Why L2-only for the SQL surface when the crate supports three metrics?**
Semantic honesty. One `distance()` function whose meaning depended on which
index happens to exist would be a footgun; pgvector solves this with distinct
operators (`<->`, `<=>`, `<#>`) and per-opclass indexes — that's the stretch
milestone. Until then `distance()` is always squared L2, index or not, and
the exact and indexed paths provably agree.

**26. How did you validate the graph, beyond recall?**
Structural invariants as unit tests: layer-0 BFS connectivity (every vector
reachable, else it's silently unfindable forever), degree bounds (`m_max0`
at layer 0, `m` above), geometric layer decay, determinism given a seed,
tombstone routing, k > n behavior, and search-vs-exact agreement on easy
data. Plus persistence round-trips asserting *bit-identical* search results
across save/load on both the mmap and owned paths.

**27. Query planning: when does the optimizer use the index?**
Pattern rule in `build_plan` → `try_vector_access`: single table, no
joins/aggregates, `ORDER BY distance(col, const)` ascending, `LIMIT k`, and
a registered HNSW index on `col`. It rewrites the scan's access path to
`Access::VectorTopK` (visible in EXPLAIN as `HnswTopK`), leaving the
Sort/Limit to guarantee exactness. Anything else falls back to the exact
row-wise `distance()` evaluation — slower, never wrong.

**28. What's honestly weakest in this implementation?**
Four things I'd say before being asked: (a) saving sidecars only on
checkpoint means reopen-after-crash pays a rebuild — safe but slow at scale;
(b) ef-escalation re-searches from scratch instead of resuming the beam;
(c) the filtered-search admission probe does a B+-tree fetch per candidate —
correct, but a bitmap of matching keys would be far cheaper for mid-
selectivity predicates; (d) the benchmark is synthetic (clearly labeled) —
the harness has a loader path for real embeddings and that's the first thing
I'd run next. Knowing the weaknesses is half of owning the design.

**29. You built this with AI assistance. What do you actually understand?**
All of it, and the artifacts show it: design decisions with rejected
alternatives recorded before code (the M9 spec), a debugging trail where a
recall regression was diagnosed to a measurement artifact rather than
hand-waved, honest limits documented next to the numbers, and every deviation
from the paper flagged and reasoned. Ask me anything on this sheet without
the sheet.

**30. Compare your implementation to pgvector's, concretely.**
Same: HNSW with the paper's defaults (M=16, ef_construction=200, Mmax0=2M),
heuristic neighbor selection, index-returns-keys / heap-resolves-rows split,
non-MVCC-aware index with visibility at fetch, normalize trick for cosine.
Different: pgvector pages the graph through Postgres's buffer manager with
WAL integration (survives crashes without rebuild; scales past RAM) and has
per-metric operator classes; I have a sidecar with rebuild-on-mismatch,
L2-only SQL surface, and — one thing pgvector's HNSW *doesn't* do —
predicate-aware traversal with ef-escalation for filtered queries (pgvector
0.8 added iterative scans to fight the same cliff; mine filters admission
inside the walk).
