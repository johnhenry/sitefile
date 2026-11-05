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

Dependencies (rationale in `rust/Cargo.toml`): serde/serde_json, sha2 (with
hardware SHA-256), base64, flate2 (zlib-rs), ureq (HTTPS with a custom resolver
for connect-time SSRF checks), wasmtime (WebAssembly plugins). JavaScript
plugins run in the same Node `--permission` sandbox as the TypeScript version,
with the same runner, so `node` must be on PATH for them.

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
  Currently 60/60 (39 offline, 21 network); the network cases need `openssl`.
- **Differential fixtures** generated from the TypeScript code and checked by
  the Rust unit tests without Node: Markdown (1721 cases), YAML, frontmatter,
  CSV and templates (10 500), plugin cache keys, and byte-exact wbn, tar,
  stored-zip and OCI config/index fixtures. The fixture generators are next to
  them (`rust/tests/fixtures/*/generate.mjs`).
- **Benchmarks**: `bench/` (see [bench/README.md](../bench/README.md)).

## Known differences

- **Compressed bytes.** Deflate output (zip entries, tar.gz, OCI layers)
  differs from Node's zlib, so OCI layer digests, manifest digests and therefore
  image IDs differ between the two implementations for the same build. Layer
  *contents*, configs and every uncompressed format are identical.
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
