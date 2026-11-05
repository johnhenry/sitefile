#!/bin/sh
# Regenerate the .wasm test fixtures from their .wat sources (needs wabt's wat2wasm).
set -eu
cd "$(dirname "$0")"
for f in *.wat; do wat2wasm "$f" -o "${f%.wat}.wasm"; done
echo "built $(ls *.wasm | wc -l | tr -d ' ') fixtures"
