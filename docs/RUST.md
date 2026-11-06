# The Rust implementation

`rust/` is a port of the TypeScript reference implementation (`src/`). Both
are maintained: same language, same CLI, same output. The TypeScript version
remains the reference; when they disagree, the TypeScript behaviour is the
specification until it is deliberately changed in both.

```bash
cd rust
cargo build --release            # → target/release/sitefile
cargo test --release             # unit + CLI integration tests
./target/release/sitefile build ../examples/blog
```

## Layout

Module names mirror the TypeScript files they port, so a change in one is easy
to find in the other:

| Rust | TypeScript |
| --- | --- |
| `error`, `digest`, `paths`, `tree`, `values`, `cbor` | `errors.ts`, `digest.ts`, `paths.ts`, `tree.ts`, `values.ts`, `cbor.ts` |
| `language::{ast, scanner, parser, validate}`, `planner`, `build_args` | `language/*`, `planner.ts`, `build-args.ts` |
| `render`, `markdown`, `frontmatter`, `yaml`, `csv`, `documents` | same names |
| `resources::*` | `resources/*` |
| `export::{exporter, archive_writers, wbn, oci, formats}`, `store`, `refs`, `server`, `servers` | `exporter.ts`, `archive-writers.ts`, `wbn.ts`, `oci.ts`, `export-formats.ts`, `store.ts`, `ref.ts`, `server.ts`, `servers.ts` |
| `site` (SITEMAP, FEED, `_redirects`/`_headers` writers); the rules section of `server` | `site.ts`; `serve-rules.ts` |
| `plugins::{host, wasm, exec, cache}`, `registry`, `base_image` | `plugins/*`, `registry*.ts`, `base-image.ts` |
| `executor`, `cli` | `executor.ts`, `cli.ts`, `cli/*` |
| `watch` (watch mode: polling, ignore rules, rebuild loop) | `watch.ts`, `cli/watch.ts` |
| `lsp::*` (the language server) | none: Rust only, see below |

## Tooling exception: the language server

`sitefile lsp` (`rust/src/lsp/`, documented in [LSP.md](LSP.md)) exists only
in the Rust implementation. This is a deliberate exception to "same CLI": the
Sitefile **language** stays identical in both implementations (the server
reuses the Rust scanner, parser, validator and planner, whose behaviour
conformance checks against the TypeScript reference), while the server itself
is an editor tool with no build output to compare. The TypeScript CLI answers
`sitefile lsp` with a usage error (exit 2) that points to the Rust binary, and
both `--help` texts list the command. Keyword tables the server completes from
(`INSTRUCTIONS`, the flag and clause lists in `language::parser`, `ALL` on the
enums in `language::ast`) are the ones the Rust parser uses; a language change
updates them with the parser. The server adds no dependencies: framing is
hand-rolled and messages use serde_json.

Dependencies (rationale in `rust/Cargo.toml`): serde/serde_json, sha2 (with
hardware SHA-256), base64, flate2 (zlib-rs, for decompression only), ureq (HTTPS with a custom resolver
for connect-time SSRF checks), wasmtime (WebAssembly plugins), icu_normalizer
(NFC for paths, see "Unicode paths"). JavaScript
plugins run in the same Node `--permission` sandbox as the TypeScript version,
with the same runner, so `node` must be on PATH for them.

## Wasm module cache

Compiling a wasm plugin with Cranelift costs ~55 ms, so the Rust build keeps
compiled modules in `$SITEFILE_HOME/cache/wasm/<key>.cwasm` (default
`~/.sitefile/cache/wasm`). The key is a SHA-256 over the wasm bytes, the
wasmtime version, the target and the codegen settings. Entries are loaded with
wasmtime's `deserialize_file`, which trusts the file, so the directory is
created `0700`, entries are written `0600` through a temp file and rename, and a
directory that is a symlink or group/world-writable is not used. A corrupt or
stale entry is recompiled and replaced; import and export checks run on every
load, hits included. Any cache failure (read-only home, etc.) is ignored and
the build compiles as before. Within one process each plugin is compiled once.
Set `SITEFILE_NO_WASM_CACHE=1` to turn the disk cache off. The TypeScript
version has no equivalent (V8 compiles wasm quickly). A hit refreshes the
file's mtime, and the shared cache size limit (`SITEFILE_CACHE_MAX`, see
[CLI.md](CLI.md#cache-size-limit)) evicts least recently used modules; both
implementations measure and evict this directory, so `cache du` and the
eviction results are the same whichever one runs.

## Unicode paths

Paths are normalized to NFC and compared under Unicode simple case folding
(docs/LANGUAGE.md, "Writes and conflicts" and "Unicode normalization of
paths").

- **Case folding** uses no dependency in either implementation: neither
  `String.prototype` nor Rust's `char` offers case folding (`toLowerCase` and
  `to_lowercase` are full lowercase mappings, which differ for `ς`, Cherokee
  and others). `tools/gen-casefold.mjs` turns `CaseFolding.txt` (Unicode
  17.0.0, statuses C and S) into 209 runs, written to both
  `src/casefold-data.ts` and `rust/src/casefold_data.rs`.
- **NFC** in TypeScript is `String.prototype.normalize('NFC')` (Node's ICU).
  For Rust the choice was a vendored table or a crate. NFC needs the canonical
  decomposition, combining class and composition data plus Hangul and
  composition-exclusion rules (thousands of entries, and an algorithm that is
  easy to get subtly wrong), so a small vendored table would not be small.
  `icu_normalizer` 2 (ICU4X, the Unicode Consortium's own implementation) is
  already compiled into the binary through ureq → url → idna →
  idna_adapter, with the same version and `compiled_data` feature, so naming
  it in `Cargo.toml` adds no crate, no code and no build time;
  `unicode-normalization` would have added a second copy of the data.
- **Parity**: `test/path-safety.test.ts` and `rust/src/paths.rs`
  (`nfc_matches_typescript`, `fold_matches_typescript`) assert the same two
  digests, over the NFC form and the folding of every Unicode scalar value
  and of about 900 000 base + combining mark and Hangul sequences (except a
  few marks the two Unicode data sets disagree on; see "Known differences"). A Unicode
  version change in Node's ICU or in `icu_normalizer_data` makes both fail
  until the implementations agree again.

## How parity is checked

- **Conformance** (`node conformance/run.mjs`): builds every example (and zip,
  tar, tar.gz, wbn and OCI variants) and every directory in `conformance/cases`
  with both CLIs, and requires the same exit code, the same `error[CODE]: …`
  line, and identical output trees; `validate`/`inspect` cases compare stdout.
  Network cases (`conformance/cases/net-*`) start a local HTTPS server (a
  throwaway CA, trusted through `NODE_EXTRA_CA_CERTS`, which both CLIs honor)
  and an in-process mock OCI registry, then cover remote `IMPORT` with and
  without `INTEGRITY`, redirects, SSRF refusal, status and size errors, remote
  archives, `Sitefile.lock` contents (byte for byte), `--frozen-lockfile`,
  `--update-lock`, `EXPORT … AS oci BASE` and a `push`/`pull`/`save` round trip.
    Compressed outputs (zip, tar.gz, OCI archives, gzip and uncompressed
  layers) are compared byte for byte, and OCI images by manifest digest.
  Currently 120/120 (99 offline, 21 network); the network cases need `openssl`.
- **Differential fixtures** generated from the TypeScript code and checked by
  the Rust unit tests without Node: Markdown (1721 cases), YAML, frontmatter,
  CSV and templates (10 500), plugin cache keys, and byte-exact wbn, tar,
  zip and OCI config/index fixtures, and 30 deflate/gzip streams
  (`rust/tests/fixtures/deflate`). The fixture generators are next to
  them (`rust/tests/fixtures/*/generate.mjs`).
- **Parser fuzzing** (`rust/tests/fixtures/sitefile`): a seeded generator
  writes Sitefiles that mix valid programs, near-valid mutations (dropped,
  duplicated and swapped tokens, broken quotes, `<<`/`<<-` heredocs with and
  without their delimiter, CRLF, tabs, `\` continuations, Unicode
  whitespace and case mappings, very long lines, `${…}` edge cases,
  `STAGE`/`FROM` mixes) and noise, with what TypeScript does to each: the
  validated IR or the error (code, line, column, message), and
  `sitefile validate`'s result (warnings and `--json` output, or its error).
  `language::differential` requires the Rust parser, validator and planner
  to give identical results, and runs every input through
  `parse_sitefile_all`, the LSP's diagnostics and its editor features
  (no panics; the diagnostics agree with `validate`). The committed fixture
  is about 480 inputs; a local run takes any size:

  ```bash
  FUZZ_OUT=/tmp/sf.json FUZZ_COUNT=50000 FUZZ_SEED=7 node rust/tests/fixtures/sitefile/generate.mjs
  (cd rust && SITEFILE_PARSER_FUZZ=/tmp/sf.json cargo test --release --lib -- --ignored differential_external)
  ```

  `test/parser-fuzz-fixture.test.ts` (npm test) fails when the TypeScript
  side no longer produces the committed results; regenerate with
  `node rust/tests/fixtures/sitefile/generate.mjs`. New instructions go in
  the generator's `INSTRUCTIONS` table.
- **Benchmarks**: `bench/` (see [bench/README.md](../bench/README.md)).

## Compression

Both implementations compress with the same deterministic DEFLATE encoder
(`src/deflate.ts` and `rust/src/deflate.rs`; the specification is at the top
of the TypeScript file), not with Node's zlib or zlib-rs, so zip entries,
tar.gz exports and OCI layers are byte-identical and an image has the same
layer digests, manifest digest and image ID whichever CLI built it. The
encoder is hash-chain LZ77 with lazy matching and smallest-of
stored/fixed/dynamic blocks; output is within about 1-2% of zlib level 9.
Decompression (reading archives and pulled images) still uses `node:zlib` and
flate2. A change to the encoder must be made in both files; regenerate
`rust/tests/fixtures/deflate/fixtures.json` with its `generate.mjs`.

## Known differences

- **Extra CAs.** Both CLIs trust the PEM file named by `NODE_EXTRA_CA_CERTS`
  (Node reads it at startup; the Rust CLI reads the same variable for imports
  and registries, and ignores an unreadable file with Node's warning). As in
  Node (default store plus extra), it and `FetchPolicy.ca` are added on top of
  the default roots, which for Rust are the Mozilla roots of webpki-roots
  rather than Node's bundled copy.
- **Third-party error text.** Messages that embed a JSON parser's or the OS's
  own wording (invalid JSON files, DNS failures) use serde's and Rust's text.
- **NFC of not-yet-assigned marks.** `icu_normalizer_data` 2.4 gives a
  combining class to U+05C8–U+05C9 and U+1ADE–U+1AF0, which Node's ICU (78, Node 26)
  still treats as unassigned, so a path that puts one of them after another
  combining mark normalizes differently. The parity digests leave them out;
  they will agree once Node's ICU catches up.
- **Watch mode change detection.** `build --watch` and `serve --watch` behave the
  same (paths watched and ignored, 100 ms debounce, coalescing, messages), but
  the TypeScript CLI is notified by `fs.watch` (recursive) while the Rust CLI
  polls: every 100 ms it walks the context, the Sitefile and the
  `--build-arg-file` files (symlinks are not followed) and compares size,
  mtime, ctime, inode and type. Rust's std has no file notification API, and a
  crate such as `notify` would be a new dependency for little gain, so a
  rebuild in Rust starts up to ~100 ms later, and a very large context costs a
  directory walk per poll.
- **Small edge cases** documented in the port PRs (#38–#44): IPv6 zone IDs in
  `serve -p`, `toPrecision` rounding ties in `images` sizes, very deep emphasis
  nesting in Markdown.
