# WebAssembly playground

The most visual proof that the engine is real: the *whole* of ferrodb — storage, SQL, MVCC, the
optimizer — compiles to **WebAssembly** and runs entirely in the browser, next to a **live B+-tree
visualizer** that shows the tree split as you insert rows.

## The prerequisite: no filesystem

A browser has no `File`. This is why the storage layer was built on the `Blob` trait from the start
(Chapter 2): the native build backs it with a real `File`, and the WASM build backs it with a
`Vec<u8>` (`MemBlob`). `Database::open_in_memory()` assembles a fully in-memory engine that needs no
files at all — and nothing above the storage layer had to change to make the browser build work.

## A hand-written C ABI — no wasm-bindgen

`crates/wasm` is a `cdylib` targeting `wasm32-unknown-unknown` with **no `wasm-bindgen` and no
dependencies**. It exports a tiny C ABI:

- `alloc` / `dealloc` — so JavaScript can write an input string into wasm linear memory.
- `db_new` / `db_free` — lifecycle of a `Database` handle.
- `db_exec` — run a SQL string, return results as JSON.
- `db_tree` — export a table's B+-tree as JSON.
- `free_string` — release a returned buffer.

Strings cross the boundary as **length-prefixed byte buffers** (`[u32 len][bytes...]`) that the JS
reads straight out of wasm memory — the same trick in both directions. The `Output → JSON`
formatting is a pure function, unit-tested on the host. The resulting module (~383 KB) instantiates
with **zero imports**.

## The visualizer

`web/index.html` is a SQL editor with a results table and, beside it, an **SVG rendering of the live
B+-tree**. `Database::table_tree` walks the physical tree through the buffer pool into a
`TreeNode { leaf, keys, children }`, decoding keys per the primary-key type; `to_json` serializes it
by hand. A small tidy-tree layout (measure subtree widths, place children, center parents) draws it,
color-coding internal and leaf nodes. The tree refreshes after every statement — insert rows and you
watch a single leaf grow into a root over multiple leaves in real time, the split behaviour from
Chapter 3 made visible.

A prebuilt `.wasm` is committed so the `web/` directory can be served as-is; `web/build.sh` rebuilds
it.
