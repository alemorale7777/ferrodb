# M9 — HNSW Vector Index: Phase 0 Audit & Integration Plan

*2026-07-09. Status: implemented — see `crates/vector`, the engine's M9 hooks, and `docs/book/src/vectors.md`. Written as the Phase-0 plan before any code; kept as the decision record.*

The goal: add an HNSW (Hierarchical Navigable Small World) vector index as a
**secondary index** — architecturally parallel to how the B+-tree is the primary
index, and deliberately mirroring how pgvector extends Postgres. Vector search
returns row keys; rows are fetched through the existing B+-tree. The hero
feature is **filtered vector search**: ANN + relational predicates in one engine.

---

## 1. What the audit found

### Storage layer (`crates/storage`)
- Single file of 4 KiB pages with CRC32C trailers (`page.rs`), behind a
  clock-sweep **no-steal** buffer pool (`buffer.rs`).
- `Blob` trait abstracts the byte store (`File` natively, `MemBlob` for WASM) —
  anything we persist should ideally respect this so the WASM build keeps working.
- B+-tree values are inlined only below `PAGE_DATA_SIZE/4` ≈ **1 KB**; larger
  values go to **overflow page chains** (`btree/tree.rs::encode_value`).
  Consequence: a 768-dim `f32` vector is 3 KB — every row carrying one will use
  an overflow chain. This works today with zero changes, but it means reading a
  vector via the table path costs multiple page fetches.
- WAL is **full-page redo**, redo-only recovery, reset after each force
  (`wal.rs`). Anything that lives in data-file pages is automatically
  crash-safe; anything outside the pager is not covered by the WAL.

### Engine layer (`crates/engine`)
- Catalog = a B+-tree keyed by table name; `TableSchema` (columns, data-tree
  root, `next_rowid`, `row_count`) hand-encoded in `catalog.rs`. Encoding is
  versioned-by-tolerance: `row_count` was added later and old catalogs decode
  fine (`decode_schema` tolerates missing tail bytes). We follow that pattern.
- Rows are **MVCC version chains** stored as B+-tree values; visibility decided
  per snapshot at read time (`mvcc.rs::visible_index`). Key insight for us:
  **the table indexes bytes, not truth** — visibility is applied at fetch.
- Planner produces a `Plan` tree with `Access::{Seq, IndexSeek, IndexRange}`
  (`plan.rs`, `lib.rs::scan_with_pushdown`). A vector index adds a new access
  path here eventually, but Phases 1–4 don't touch the planner at all.
- `Database` is `&mut self` everywhere; `pgwire` serializes connections behind
  `Arc<Mutex<Database>>`. There is no intra-engine concurrency today.

### SQL layer (`crates/sql`)
- `DataType` is a `Copy` enum of 4 variants; `Value` mirrors it plus `Null`.
- Hand-written lexer + Pratt parser; types parsed in `parser.rs::parse_type`.

### Conventions
- No third-party crates for core machinery (no serde, no sqlparser, no btree) —
  hand-rolled encodings everywhere. **The vector module must match this ethos.**
- Tests: in-module unit tests + `crates/*/tests/*.rs` integration tests +
  proptest model-checking in storage (`btree_prop.rs`). Baseline: 71 tests, all
  green (re-verified at the start of this session).
- CI: fmt + clippy `-D warnings` + full suite. `wasm32-unknown-unknown` must
  keep compiling (`crates/wasm` has a hand-rolled C ABI).

---

## 2. Design decisions (with rejected alternatives)

### D1. New workspace crate: `crates/vector`
Distance kernels, HNSW graph, and index persistence live in a new `vector`
crate; `engine` depends on it the way it depends on `storage`.

- *Rejected: module inside `storage`* — storage is a pager; HNSW is an
  in-memory graph with its own persistence model. Different worlds.
- *Rejected: module inside `engine`* — makes the index untestable/benchable in
  isolation and tangles the dependency graph (engine already depends on sql).

### D2. `Vector(dim)` type
- `DataType::Vector(u16)` (stays `Copy`), `Value::Vector(Vec<f32>)`.
- Literal syntax: pgvector-style **text literal** `'[0.1, 0.2, ...]'`, coerced
  to a vector by `coerce()` when the target column is `Vector(dim)`, with
  dimension validation. No lexer changes needed.
- Row encoding in `tuple.rs`: `u32` element count + raw `f32` LE bytes.
- Catalog: new type tag `4` followed by the `u16` dim (variable-width tags are
  fine; decode already walks byte-by-byte).
- *Rejected: `[1,2,3]` array literal grammar* — new token/expr machinery for
  zero interview value; pgvector itself uses the quoted form.

### D3. HNSW owns a private copy of the vectors
The graph stores vectors contiguously in its own arena (`Vec<f32>`, dim-strided).
The table row remains the durable source; the index copy is for traversal speed.
Distance evaluation during search must be a pointer chase + SIMD kernel, not a
B+-tree lookup through an overflow chain (that would be ~3 page fetches per
distance — catastrophic).

### D4. Node id ↔ row key mapping
HNSW nodes get dense `u32` ids. The index keeps `id → PK key bytes`
(the exact bytes the B+-tree is keyed by, from `tuple::value_to_key` /
`rowid_key`). Search returns row keys; the engine fetches rows via the
existing `table_get_chain` + MVCC visibility. This is the "pgvector returns
TIDs, the heap fetch resolves them" parallel, one-to-one.

### D5. MVCC interaction: the index is not MVCC-aware (like Postgres)
Inserts add to the HNSW graph immediately, tagged with the row key. If the
transaction aborts, the graph node becomes a **ghost**: search may return its
row key, but the B+-tree fetch applies snapshot visibility and drops it.
Deletes leave the node in place (tombstone set); `VACUUM` is the natural place
to reclaim. This is exactly how Postgres indexes behave, and it's a much better
interview story than pretending we solved MVCC-aware ANN.

### D6. Persistence: sidecar file, not pager pages
The index serializes to `<db>.hnsw-<table>-<col>`: header (magic, version, dim,
metric, M, ef_construction, entry point, count) + adjacency lists per layer +
id→key table + the vector arena, which is **mmap'd on load**.

- Crash safety: the index is **derived data**. WAL never logs graph mutations.
  The header stores a checkpoint marker; on open, if the sidecar is missing,
  torn, or stale relative to the table, we **rebuild from the base table**
  (a real database's `REINDEX`). Honest, simple, defensible.
- *Rejected: paging the graph through the buffer pool* — pgvector genuinely
  does this and it's the principled endgame, but it forces graph nodes into
  4 KiB pages with page-id links and WAL-logged splits; that's a milestone of
  its own. This is the #1 "what breaks at 10M vectors" interview answer: our
  mmap approach needs the vector arena resident; pgvector's paged approach
  degrades gracefully under memory pressure.
- Note: sidecar uses `std::fs` + `mmap` — not the `Blob` trait — so the vector
  index will be a no-op on WASM initially (feature-gated). Flagged as a known
  limitation rather than blocking on it.

### D7. Concurrency
The `vector` crate exposes a thread-safe index: `RwLock` over the graph
(many concurrent searches, exclusive inserts), per the crate's public API and
tests. Inside FerroDB it currently sits behind the engine's existing
single-writer model (`Arc<Mutex<Database>>` in pgwire), so the RwLock is
about the crate's standalone story and honesty in interviews: "the index
supports concurrent reads; the engine's session model is the current
bottleneck, same as the rest of FerroDB."
- *Rejected: lock-free reads with epoch GC (crossbeam)* — real HNSW libs do
  this, but it drags in a dependency and subtle unsafe; RwLock read-parallelism
  is already a true, explainable claim.

### D8. SIMD on stable Rust
Repo requires stable; `std::simd` is nightly. Plan: scalar reference kernels +
`std::arch` x86-64 AVX2/FMA intrinsics behind `is_x86_feature_detected!`,
`unsafe` confined to the kernel bodies with safety comments, `cfg`-gated so
non-x86 (and wasm) targets compile scalar-only. Property tests assert
SIMD ≈ scalar within epsilon.
- *Rejected: `wide` crate* — safe and portable, but hand-written intrinsics
  with runtime detection is the systems-depth signal this project exists for,
  and matches the no-crates ethos.

### D9. Index metadata in the catalog
`TableSchema` gains a `Vec<IndexInfo>` tail (name, column, kind=HNSW, metric,
M, ef_construction) using the same tolerate-missing-tail decoding as
`row_count`. Old catalogs keep decoding with zero indexes.

---

## 3. Phase map (what touches what)

| Phase | Deliverable | Files |
|---|---|---|
| 1 | Distance kernels: cosine/L2/dot, scalar + AVX2, normalize-on-insert | new `crates/vector/src/distance.rs` |
| 2 | HNSW insert/search with Algorithm-4 neighbor selection | `crates/vector/src/{hnsw,node}.rs` |
| 3 | Recall harness: brute-force ground truth, recall@10, ef_search sweep | `crates/vector/src/bin/` or bench bin |
| 4 | Sidecar persistence + mmap, rebuild-on-mismatch | `crates/vector/src/persistence.rs` |
| 5 | `Vector(dim)` type, catalog `IndexInfo`, insert hooks, `CREATE INDEX`, k-NN query path → B+-tree fetch | `sql/{token,parser,ast}.rs`, `engine/{catalog,tuple,lib}.rs` |
| 6 | Filtered search: pre/post-filter + predicate-aware traversal, vs filtered brute force | `vector/src/hnsw.rs`, `engine/lib.rs` |
| 7 | (stretch) quantization; pgvector operator syntax `<->` | — |

Existing 71 tests stay green at every phase boundary.

---

## 4. Open questions for Alej

1. **Crate name & milestone framing** — `crates/vector` as "M9" in the README
   roadmap style, OK?
2. **Recall dataset** — default plan is clustered synthetic Gaussians (clearly
   labeled) so the harness is self-contained and CI-runnable, with a loader for
   real embeddings (GloVe/SIFT subset) if you drop a file in. Acceptable, or do
   you want a real dataset vendored in from the start?
3. **SQL surface for Phase 5** — minimal viable: `CREATE INDEX ... USING HNSW`,
   and k-NN via a function-style `SELECT ... ORDER BY distance(embedding, '[...]') LIMIT k`
   recognized by the planner. The pgvector `<->` operator can wait for Phase 7. OK?
4. **WASM** — fine to feature-gate the vector index out of the wasm build for
   now (mmap doesn't exist there)?
5. **Ghost policy (D5)** — comfortable defending "index returns candidates,
   MVCC fetch filters them" in an interview? This one you'll get asked about.
