---
title: Writing one in Rust
---
Build a `cdylib` for `wasm32-unknown-unknown` that exports `memory`, `sitefile_alloc`
and `sitefile_transform`. The request and result are JSON. This site's badge plugin is
about two hundred lines of standard-library Rust, including its own tiny JSON parser,
so it needs no crates at all and builds offline. Any language that targets WebAssembly
without imports can do the same: C with clang, Zig, AssemblyScript or hand-written WAT.
