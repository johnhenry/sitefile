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
| `plugins::{host, wasm, exec, cache}`, `registry`, `base_image` | `plugins/*`, `registry*.ts`, `base-image.ts` |
| `executor`, `cli` | `executor.ts`, `cli.ts`, `cli/*` |
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
for connect-time SSRF checks), wasmtime (WebAssembly plugins). JavaScript
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
version has no equivalent (V8 compiles wasm quickly).

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
  Currently 92/92 (71 offline, 21 network); the network cases need `openssl`.
- **Differential fixtures** generated from the TypeScript code and checked by
  the Rust unit tests without Node: Markdown (1721 cases), YAML, frontmatter,
  CSV and templates (10 500), plugin cache keys, and byte-exact wbn, tar,
  zip and OCI config/index fixtures, and 30 deflate/gzip streams
  (`rust/tests/fixtures/deflate`). The fixture generators are next to
  them (`rust/tests/fixtures/*/generate.mjs`).
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
- **Small edge cases** documented in the port PRs (#38–#44): IPv6 zone IDs in
  `serve -p`, `toPrecision` rounding ties in `images` sizes, very deep emphasis
  nesting in Markdown.
