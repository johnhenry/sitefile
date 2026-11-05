# Benchmarks

`bench/` compares the TypeScript implementation (`src/`, run with Node) and
the Rust port (`rust/`). The latest numbers are in [REPORT.md](REPORT.md); raw
data is in `results/latest.json`.

```bash
npm ci && npm run build                 # TypeScript CLI → lib/
(cd rust && cargo build --release)      # Rust CLI → rust/target/release/sitefile
node bench/generate.mjs                 # deterministic synthetic sites → bench/.sites
node bench/run.mjs --runs 5             # → bench/REPORT.md, bench/results/latest.json
```

`run.mjs` times each scenario around the whole process (startup included),
after one warm-up run, and reads peak RSS from `/usr/bin/time`. `--only <text>`
runs a subset; `--rust <path>` picks another Rust binary. The two
implementations produce identical output trees for every scenario
(`node conformance/run.mjs` checks the examples and cases; the synthetic sites
use the same features).

## Summary (Apple M2, Node 26.11, median of 5)

| | TypeScript | Rust |
| --- | --- | --- |
| Start-up (`--version`) | 74 ms | 4 ms |
| Small site (examples/blog) | 77 ms | 7 ms |
| 1000-post blog | 806 ms | 614 ms |
| 5000 static files | 1298 ms | 662 ms |
| 1000-post blog → OCI image | 2025 ms | 1410 ms |
| 1000-post blog → OCI image, uncompressed layer | 623 ms | 536 ms |
| Peak memory, 1000-post blog | 188 MB | 78 MB |

- **Small builds are 11–16× faster**, almost entirely because the Rust binary
  starts in ~4 ms while Node needs ~70 ms before running any code. For
  editor-on-save rebuilds, CI steps and `validate` this is the difference that
  matters.
- **Large builds are 1.3–2.2× faster.** Both implementations do the same
  amount of work, and at this scale much of it is filesystem calls (one
  `open`/`write`/`chmod` per output file, plus removing the previous output)
  and SHA-256 hashing of every output file, which Node already does in native
  code (OpenSSL). Rust wins most where it parallelizes I/O (static copy,
  2.1×) and where V8's per-object overhead dominates (tar.gz import, 2.2×; CSV,
  1.7×).
- **Memory is 2–30× lower.** Node's baseline is ~73 MB before any work; the
  Rust binary starts at ~2 MB and shares file bytes between trees
  (`Arc<[u8]>`) instead of copying them.
- **Compressed exports (zip, tar.gz, OCI) are bounded by deflate.** Both
  implementations use the same deterministic encoder (`src/deflate.ts`,
  `rust/src/deflate.rs`), so their bytes, and OCI image IDs, are identical.
  Against the zlib level 9 the writers used before, on the 1000-post blog
  (63 MB tar): output is 0.9% (zip) to 1.4% (tar.gz, OCI) larger, and export
  is faster in both implementations (TypeScript 7-9%, Rust 15-20%; table
  below). `--oci-compression none` skips compression entirely: 3× faster in
  TypeScript, 6.2× larger (63 MB).

  | 1000-post blog | TS zlib-9 | TS shared | Rust zlib-rs-9 | Rust shared | size before (TS) | size now |
  | --- | --- | --- | --- | --- | --- | --- |
  | zip | 1990 ms | 1816 ms | 1564 ms | 1254 ms | 10 571 300 | 10 667 343 (+0.9%) |
  | tar.gz | 2139 ms | 1963 ms | 1624 ms | 1383 ms | 10 009 839 | 10 146 124 (+1.4%) |
  | OCI archive | 2180 ms | 1992 ms | 1651 ms | 1411 ms | 10 025 984 | 10 162 176 (+1.4%) |

  (A/B run interleaving `origin/main` and this encoder, median of 5, whole
  process.)
- **The wasm example is only 1.4× faster** because the Rust build compiles the
  wasm module with Cranelift on every run (~55 ms). Caching the compiled
  module would remove most of that.

## What profiling changed

The first Rust measurements were *slower* than TypeScript on large sites
(1000-post blog: 924 ms vs 855 ms). Profiling with macOS `sample` found:

1. **SHA-256 in software.** The `sha2` crate ran its portable code at
   343 MB/s while Node's OpenSSL used the CPU's SHA instructions at 2390 MB/s.
   Enabling `sha2`'s `asm` feature (hardware SHA-256 on ARMv8/x86 SHA-NI):
   2054 MB/s.
2. **A copy per template substitution.** `${other.title}` inside
   `${#each}` cloned the string before escaping it; a million substitutions
   meant a million allocations. Loop-variable strings are now written straight
   from the borrowed value, and the output buffer is pre-sized.
3. **Sequential filesystem calls.** Export now writes files and removes the
   replaced output across a few threads.
4. **Deflate backend.** `flate2` used `zlib-rs` instead of `miniz_oxide`
   (both implementations now write with their own shared encoder; flate2 only
   decompresses).

After these, the 1000-post blog went from 924 ms to 623 ms, and every scenario
is faster in Rust.
