#!/usr/bin/env bash
# Build the ferrodb engine to WebAssembly and drop it next to the playground.
set -euo pipefail
cd "$(dirname "$0")/.."
rustup target add wasm32-unknown-unknown
cargo build -p ferrodb-wasm --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/ferrodb_wasm.wasm web/ferrodb_wasm.wasm
echo "built web/ferrodb_wasm.wasm — now:  cd web && python -m http.server 8000"
