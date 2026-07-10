# Vector search: the HNSW index

ferrodb M9 adds approximate nearest-neighbor search over embedding vectors —
the workload behind semantic search and RAG — as a **secondary index**,
integrated the way pgvector integrates with Postgres:

```sql
CREATE TABLE items (id INTEGER PRIMARY KEY, category TEXT, embedding VECTOR(768));
INSERT INTO items VALUES (1, 'docs', '[0.011, -0.322, ...]');
CREATE INDEX items_emb ON items USING HNSW (embedding);

SELECT id, category
FROM items
WHERE category = 'docs'
ORDER BY distance(embedding, '[0.02, -0.31, ...]')
LIMIT 10;
```

`EXPLAIN` shows the access path the optimizer chose:

```
Limit 10
  Project [id, category]
    Sort [distance(embedding, [0.02, -0.31, ... (768 dims)]) asc]
      HnswTopK items (distance(embedding, ...) LIMIT 10)
```

The design splits cleanly in two: `crates/vector` is a standalone,
dependency-free HNSW library (distance kernels → graph → persistence →
recall harness), and the engine wires it in as an index — new `VECTOR(dim)`
type, catalog entries, DML hooks, and a planner rule.

## The division of labor (the pgvector parallel)

The index maps vectors to **row keys** — the same order-preserving key bytes
the primary B+-tree is keyed by. A search returns candidate keys; the engine
fetches each through the B+-tree and applies MVCC visibility. The index never
learns about transactions:

- An **aborted insert** leaves a *ghost* node in the graph. Search may return
  its key; `mvcc::visible_index` sees `xmin` aborted and drops the row. This
  is exactly how Postgres treats dead TIDs returned by any index.
- A **delete** tombstones the graph node: it keeps *routing* traversals (its
  links remain load-bearing) but is never returned. Rebuild reclaims it.
- The plan keeps a `Sort` + `Limit` **above** the index scan. The index only
  proposes candidates; the sort re-orders them exactly and the limit
  truncates. Approximation can lose a candidate, never misorder one.

## The algorithm

HNSW (Malkov & Yashunin, 2016/2018) is a stack of proximity graphs — a skip
list generalized to metric space. Every element gets a random top layer
`floor(-ln(unif(0,1)) · mL)` with `mL = 1/ln(M)`: layer 0 holds everything,
each higher layer keeps ~1/M of the one below. Search descends the sparse
"express lanes" greedily (beam of 1), then runs an `ef`-wide beam search on
layer 0. `ef_search` is the single recall/latency knob.

The subtle part is **neighbor selection** (Algorithm 4). Linking each node to
its M *nearest* neighbors fails on clustered data: all links point inside the
cluster and greedy search can't cross between clusters. The heuristic keeps a
candidate only if it's *closer to the new node than to any neighbor already
kept* — a "does this link cover a new direction?" test that yields long-range
bridges. Rejected candidates back-fill unused slots (`keepPrunedConnections`).

Deviations from the paper, all deliberate: `extendCandidates` off (paper's own
recommendation), tombstones added (the paper has no delete; a database needs
one), and layer-0 reachability is asserted by a BFS diagnostic in the harness
rather than assumed.

## The hot loop: SIMD distance kernels

One search evaluates hundreds to thousands of distances, so the kernel is the
whole ballgame. Three metrics (L2 squared — `sqrt` is monotone, so we skip
it; cosine; negated dot so that smaller = closer uniformly), each with a
scalar reference implementation and a hand-written AVX2+FMA implementation,
selected **once** per process by CPU feature detection into plain function
pointers. `unsafe` is confined to the kernel bodies with documented
obligations; property tests hold SIMD to the scalar reference within a
relative epsilon across every `len % 8` remainder class (the sums associate
differently — bit-exact agreement would actually indicate the SIMD path
silently fell back).

Measured on this machine (`cargo run -p vector --example distbench --release`):

| dim  | dot scalar → SIMD    | L2 scalar → SIMD     | cosine scalar → SIMD |
|------|----------------------|----------------------|----------------------|
| 128  | 64 → 7 ns (**8.9×**) | 70 → 8 ns (**8.4×**) | 90 → 16 ns (5.6×)    |
| 768  | 490 → 54 ns (9.0×)   | 498 → 60 ns (8.3×)   | 523 → 84 ns (6.2×)   |
| 1536 | 1002 → 129 ns (7.8×) | 1010 → 143 ns (7.0×) | 1045 → 176 ns (6.0×) |

Cosine gains less: three FMA accumulator chains compete for the same ports —
which is why `normalize`-on-insert (store unit vectors, use dot at query
time) is the optimization pgvector also performs.

## Filtered search: the hero feature

`WHERE category = 'X' ORDER BY distance(...) LIMIT k` is where a relational
engine earns its keep against a bolted-on vector library. The naive approach
— run top-k, then filter — has a well-known **recall cliff**: with a 1%
selective predicate, the unfiltered top-10 almost surely contains zero
matches.

ferrodb threads the predicate *into the traversal*: non-matching nodes still
**route** the beam (dropping them from traversal is what strands the search),
but cannot enter the result set. The admission test resolves each candidate's
row key through the B+-tree and evaluates the predicate under the query's
snapshot — so filtering, visibility, and search share one pass. If the beam
still comes back short (ultra-selective predicate), `ef` escalates ×4 until
it covers the graph, at which point the "approximate" search has degenerated
into an exhaustive filtered scan — exactly the right fallback, and the
end-to-end test proves a single matching needle in 300 rows is found.

## Persistence: sidecar + mmap, and why not the pager

The B+-tree pages beautifully; an HNSW graph does not — traversal hops
between arbitrary nodes, so paging it means WAL-logging every adjacency
mutation (pgvector's approach, a milestone of its own). Instead the index is
**derived data**: serialized to a sidecar file (`db.hnsw-<table>-<col>`) with
a checksummed graph section and the vector arena at a 4096-aligned offset,
`mmap`'d on load (raw `mmap`/`munmap` declared by hand — no `libc` crate,
per repo ethos). The checksum deliberately excludes the arena: verifying it
would fault in every page and defeat lazy loading.

Crash story: the WAL never logs graph mutations because the WAL-protected
base table can always regenerate the index. On open, a sidecar that is
missing, torn, or *stale* (its node count disagrees with the table's key
count) is discarded and the index rebuilds — `REINDEX` semantics, proven by
tests that corrupt, truncate, and stale-ify the file.

## Measured recall (the correctness proof)

HNSW is approximate; without a recall number the implementation is
unverified. The harness (`cargo run -p vector --bin recall --release`) builds
over labeled data, computes exact ground truth by brute force, and counts
hits **by distance** (ties in the dataset shouldn't count against the index —
the first id-based counter mis-scored duplicate points, a diagnosis the
harness now documents). Dataset: clustered synthetic Gaussians, deliberately
labeled synthetic, deliberately clustered — uniform random vectors are a
misleadingly easy benchmark and clusters are what stress the neighbor
heuristic.

n=20,000, dim=64, 50 clusters, M=16, ef_construction=200, k=10, single thread:

| ef_search | recall@10 | recall@1 | mean µs/q | p95 µs/q | QPS    |
|-----------|-----------|----------|-----------|----------|--------|
| 10        | 0.499     | 0.555    | 27        | 42       | 37,199 |
| 20        | 0.641     | 0.705    | 50        | 90       | 19,787 |
| 40        | 0.775     | 0.815    | 73        | 131      | 13,615 |
| 80        | 0.877     | 0.875    | 132       | 200      | 7,558  |
| 160       | 0.949     | 0.965    | 261       | 484      | 3,818  |
| 320       | 0.987     | 0.990    | 390       | 564      | 2,562  |

Brute force on the same data: 0.7 ms/query (1,386 QPS) — the index is 3–27×
faster depending on where you sit on the recall curve. Memory: 8.3 MB
(vectors 5.1 MB + graph). On an easy regime (dim=8) the index reaches
**recall@10 = 1.000 at ef ≥ 40**, which is the implementation-correctness
signal; the clustered numbers above are the honest hard-mode curve. Build:
~5,000 inserts/s single-threaded.

## Limits, honestly

The graph must be resident (vectors can lazily page via mmap; adjacency
can't). At 10M × 768-dim that's ~31 GB of vectors — the point where
pgvector's paged, WAL-integrated design or quantization (PQ/SQ) becomes the
right answer, and the natural next milestone. Writer concurrency is
single-threaded behind the engine's existing session model; the index itself
supports parallel readers (`RwLock`, no unsafe). `VACUUM` does not yet
rebuild vector indexes to reclaim tombstones.
