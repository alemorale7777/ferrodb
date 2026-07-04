# ferrodb Milestone 7 — WASM Web Playground & B+-tree Visualizer (Design Spec)

> **Status:** Shipped · **Date:** 2026-07-03 · Builds on M1–M6.

## 1. Goal

Compile the whole ferrodb engine to **WebAssembly** and run it in the browser: a playground where
you type SQL and see results, next to a **live B+-tree visualizer** that shows the tree split as you
insert rows. This is the most *visual* proof that the from-scratch engine is real — a recruiter
clicks a link and watches the data structure rebalance.

## 2. Prerequisite — decouple the engine from the filesystem

The browser has no `File`. The storage layer is abstracted behind a small `Blob` trait
(seek / read / write / set_len / sync / len, plus `Send`) with two implementations: the std `File`
for native, and a `Vec<u8>`-backed `MemBlob` for WASM. `DiskManager` and `Wal` hold a
`Box<dyn Blob>`; `Database::open_in_memory()` builds a fully in-memory engine. All native file
behaviour is unchanged.

## 3. The B+-tree view

`Database::table_tree(name)` walks a table's physical B+-tree through the buffer pool into a
`TreeNode { leaf, keys, children }`, decoding keys for display per the primary-key type
(`i64` / big-endian row id / text). `to_json()` emits `{"leaf":…,"keys":[…],"children":[…]}` by
hand — no serialization crate. The visualizer consumes this JSON.

## 4. The WASM boundary — a hand-written C ABI

`crates/wasm` is a `cdylib` targeting `wasm32-unknown-unknown` with **no `wasm-bindgen`**. It exports
a minimal C ABI: `alloc` / `dealloc` (JS writes input into wasm memory), `db_new` / `db_free`,
`db_exec` (SQL → JSON), `db_tree` (table → tree JSON), and `free_string`. Return values are
length-prefixed byte buffers (`[u32 len][bytes]`) the JS reads straight out of linear memory. The
pure `Output → JSON` formatting is unit-tested on the host.

## 5. The playground

- `web/ferrodb.js` — the glue: allocate/write a string, call an export, read the length-prefixed
  result back, `JSON.parse` it.
- `web/index.html` — a SQL editor + result table, and an SVG B+-tree laid out by a small tidy-tree
  algorithm (measure subtree widths, place children, centre parents). Internal and leaf nodes are
  colour-coded; the tree refreshes after every statement. A demo table is seeded on load so the
  tree is interesting immediately.
- `web/build.sh` builds the `.wasm`; a prebuilt copy is committed so the directory can be served
  as-is.

## 6. Verification

The module instantiates with **zero imports** and 8 exports. Validated end-to-end in Node (drive the
C ABI: create / insert / select / explain / tree) and in a real browser (the optimizer picks an
`IndexSeek`; the B+-tree renders as a split root over its leaf nodes with correct separator keys).
Native tests cover the storage abstraction, the tree walk (300 inserts → multi-level tree), and the
`Output → JSON` formatting.

## 7. Success criteria

- [x] The engine runs in the browser on an in-memory database.
- [x] SQL results and a live B+-tree render; the tree updates as rows are inserted.
- [x] `cargo test --workspace` green; fmt + clippy clean; CI green; wasm32 build succeeds.
