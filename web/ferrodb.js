// JS glue for the ferrodb WebAssembly engine.
//
// The wasm module exports a small C ABI (see crates/wasm/src/lib.rs). Strings
// cross the boundary as length-prefixed byte buffers we read straight out of
// wasm linear memory. No wasm-bindgen, no dependencies.

export async function loadFerrodb(wasmBytes) {
  const { instance } = await WebAssembly.instantiate(wasmBytes, {});
  return new Ferrodb(instance.exports);
}

export class Ferrodb {
  constructor(exports) {
    this.w = exports;
    this.db = this.w.db_new();
    if (this.db === 0) throw new Error("failed to open in-memory database");
  }

  // Copy a JS string into wasm memory; returns {ptr, len}. Caller must dealloc.
  _write(str) {
    const bytes = new TextEncoder().encode(str);
    const ptr = this.w.alloc(bytes.length);
    new Uint8Array(this.w.memory.buffer, ptr, bytes.length).set(bytes);
    return { ptr, len: bytes.length };
  }

  // Read (and free) a length-prefixed string returned by the engine.
  _read(ptr) {
    const mem = new Uint8Array(this.w.memory.buffer);
    const len =
      (mem[ptr] | (mem[ptr + 1] << 8) | (mem[ptr + 2] << 16) | (mem[ptr + 3] << 24)) >>> 0;
    const bytes = mem.slice(ptr + 4, ptr + 4 + len);
    this.w.free_string(ptr);
    return new TextDecoder().decode(bytes);
  }

  // Run SQL; returns {columns, rows} | {message} | {error}.
  exec(sql) {
    const { ptr, len } = this._write(sql);
    const res = this.w.db_exec(this.db, ptr, len);
    this.w.dealloc(ptr, len);
    return JSON.parse(this._read(res));
  }

  // A table's B+-tree: {leaf, keys, children} | {error}.
  tree(table) {
    const { ptr, len } = this._write(table);
    const res = this.w.db_tree(this.db, ptr, len);
    this.w.dealloc(ptr, len);
    return JSON.parse(this._read(res));
  }
}
