# ferrodb web playground

The ferrodb engine — storage, SQL, MVCC, the query optimizer — compiled to
WebAssembly and running entirely in the browser, with a **live B+-tree
visualizer** that shows the tree split as you insert rows.

## Run it

```console
$ ./build.sh                 # builds web/ferrodb_wasm.wasm (needs the Rust toolchain)
$ cd web && python -m http.server 8000
# open http://localhost:8000
```

A prebuilt `ferrodb_wasm.wasm` is committed so you can skip `build.sh` and serve
the directory directly.

## How it works

- `crates/wasm` exposes a tiny hand-written **C ABI** over
  `wasm32-unknown-unknown` — no `wasm-bindgen`, no dependencies. Strings cross
  the boundary as length-prefixed byte buffers.
- `ferrodb.js` is the glue: it allocates buffers in wasm memory, calls
  `db_exec` / `db_tree`, and reads the JSON result straight out of linear memory.
- `index.html` renders query results as a table and lays out the B+-tree as SVG.

The database is fully in-memory (`Database::open_in_memory()`), so there is no
filesystem dependency — everything happens in the wasm module's own memory.
