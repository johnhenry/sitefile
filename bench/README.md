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
- **The wasm example is fast only because compiled modules are cached.**
  Cranelift takes ~55 ms per module, and the Rust build used to pay it on every
  run (62 ms for the example, 1.4× faster than TypeScript). Compiled modules
  are now cached at `$SITEFILE_HOME/cache/wasm` (and compiled once per process),
  so a warm build takes about 10 ms (14×); the first build, or one with
  `SITEFILE_NO_WASM_CACHE=1`, still pays the compile. The wasm row in
  REPORT.md was re-measured alone (`node bench/run.mjs --only wasm --runs 21`,
  warm cache) on the same machine; before the change the same run gave ~110 ms
  for Rust, in a noisier session than the 62 ms originally reported.

## Incremental rebuilds (watch mode)

`node bench/incremental.mjs [--runs 3] [--only text]` starts `build --watch --profile` on a copy of each generated
site, makes one edit several times, and reads each rebuild's time from the trace, with the memo and incremental
export (default) and without them (`SITEFILE_NO_INCREMENTAL=1`). "Build" is the rebuild without the export (parse,
imports, operations: what the memo can skip); "Export, full" writes the whole output to a temporary directory and
swaps it in, "Export, incremental" checks the previous output and writes only the changed files
([ARCHITECTURE.md](../docs/ARCHITECTURE.md#incremental-export)). The last rebuild's output is checked against a clean
build: the same tree digest and the same exported directory, file for file. Raw data: `results/incremental.json`.

Apple M2, median of 3, measured while other builds were running on the machine, so single rows are noisy (the
"Build" columns especially); the pattern held across runs:

| Site | Edit | Impl | Build, full | Build, incremental | Speedup | Export, full | Export, incremental | Speedup | Rebuild, full | Rebuild, incremental | Speedup |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| blog-1000 | one post body | typescript | 4296 ms | 4786 ms | 0.9× | 1662 ms | 278 ms | 6.0× | 5958 ms | 5125 ms | 1.2× |
| blog-1000 | one post body | rust | 1903 ms | 1250 ms | 1.5× | 902 ms | 158 ms | 5.7× | 2781 ms | 1408 ms | 2.0× |
| blog-1000 | index template | typescript | 1649 ms | 204 ms | 8.1× | 1435 ms | 172 ms | 8.3× | 3079 ms | 375 ms | 8.2× |
| blog-1000 | index template | rust | 1130 ms | 107 ms | 10.6× | 742 ms | 97 ms | 7.7× | 1862 ms | 200 ms | 9.3× |
| blog-1000 | a file no step reads | typescript | 1645 ms | 303 ms | 5.4× | 1950 ms | 192 ms | 10.2× | 3768 ms | 495 ms | 7.6× |
| blog-1000 | a file no step reads | rust | 2599 ms | 134 ms | 19.5× | 1149 ms | 186 ms | 6.2× | 3705 ms | 354 ms | 10.5× |
| blog-5000 | one post body | typescript | 2590 ms | 1876 ms | 1.4× | 7576 ms | 1181 ms | 6.4× | 10166 ms | 2964 ms | 3.4× |
| blog-5000 | one post body | rust | 2778 ms | 710 ms | 3.9× | 6512 ms | 555 ms | 11.7× | 9290 ms | 1247 ms | 7.5× |
| static-5000 | one static file | typescript | 1275 ms | 1014 ms | 1.3× | 3821 ms | 231 ms | 16.5× | 5067 ms | 1251 ms | 4.1× |
| static-5000 | one static file | rust | 1021 ms | 789 ms | 1.3× | 1186 ms | 45 ms | 26.5× | 2355 ms | 834 ms | 2.8× |
| csv-2000 | one CSV row | typescript | 51 ms | 72 ms | 0.7× | 1841 ms | 76 ms | 24.2× | 1901 ms | 140 ms | 13.5× |
| csv-2000 | one CSV row | rust | 43 ms | 43 ms | 1.0× | 689 ms | 14 ms | 49.1× | 734 ms | 57 ms | 13.0× |
| archive-2000 | a file no step reads | typescript | 57 ms | 11 ms | 5.3× | 1443 ms | 51 ms | 28.1× | 1493 ms | 66 ms | 22.6× |
| archive-2000 | a file no step reads | rust | 32 ms | 8 ms | 3.9× | 652 ms | 15 ms | 44.3× | 685 ms | 23 ms | 30.2× |

- **The export is no longer the bottleneck.** Writing only the changed files makes the export 6-49× faster: what is
  left is about one `lstat` per output file (the check that the output is as the previous build left it) plus the
  files that changed. For 2000-5000 small files, where the full export was 70-90% of a rebuild, whole rebuilds are now
  3-30× faster (csv-2000: 1.9 s to 140 ms in TypeScript, 734 ms to 57 ms in Rust).
- **The build phase gets much faster when the expensive steps are skipped**: an edit that no step reads, or a
  template other pages do not use, skips Markdown parsing and the 1000-page RENDER (5-20× on the build phase).
  Editing one post re-parses one document; its pages still all re-render on blog-1000, whose post template lists
  every post (a nav), so every page depends on every title, and the export then writes all 1000 pages (still 6×
  faster than a full export: it skips the unchanged feed, assets and badges and never removes the old tree).
  blog-5000 has no nav: only the edited post's page re-renders.
- **What remains**: every input is still read and hashed (static-5000's COPY reads 5000 files), and cheap templates
  (csv-2000) gain nothing in the build phase; hashing each element costs about what rendering it does.

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
