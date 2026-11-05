#!/bin/sh
# Rebuild ../badge.wasm (needs the wasm32-unknown-unknown Rust target).
set -eu
cd "$(dirname "$0")"
cargo build --release --target wasm32-unknown-unknown --offline
cp target/wasm32-unknown-unknown/release/sitefile_badge.wasm ../badge.wasm
echo "wrote ../badge.wasm"
