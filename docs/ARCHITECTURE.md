# Architecture specification v0.1

## Purpose
A general-purpose static artifact composer, initially optimized for websites. Dockerfile-inspired syntax, not Dockerfile compatibility.

## Core invariants
- Build output is a filesystem tree, not a running container.
- Resources may be files, trees, or structured values; do not coerce values into trees implicitly.
- Stages are immutable snapshots. Operations return new snapshots.
- Imports are named, immutable inputs. Importing alone never changes output.
- Every output path has a deterministic owner; conflicting writes fail unless an explicit override is declared.
- Acquisition, transformation, and placement are distinct operations internally.
- No implicit network access during rendering.
- Paths are POSIX-style within virtual trees; host paths are restricted to declared context.
- Virtual paths are Unicode NFC, and no two paths in a tree are equal under simple case folding, so an artifact can be materialized on case-insensitive and normalizing file systems.
- Build args are public; secrets never become part of artifact metadata or cache keys in plaintext.

## Logical modules
1. language: lexer/parser, spans, typed AST, semantic validator.
2. planner: stage graph, dependencies, cycle detection, cache keys.
3. resources: protocol registry and local/HTTPS resolvers.
4. artifacts: file/tree/value types, digests, metadata.
5. composer: copy, overlay, remove, conflict policies.
6. render: safe variable interpolation with explicit escaping modes.
7. executor: bounded concurrent graph scheduling.
8. cache: content-addressed blobs, tree manifests, lockfile.
9. exporter: atomic directory materialization.
10. cli: build, validate, inspect.

## Resource types
File {digest, bytes, mode, mediaType?}
Tree {digest, entries}
Value {digest, format, value}
URI resolution returns one of these, with provenance.

## Security
- Local source sandbox; reject absolute escapes and `..` traversal.
- Restrict remote redirects and DNS/address targets; SSRF protections configurable.
- Bound download size, decompression ratio, timeouts, and recursion.
- Verify expected digests; do not execute fetched content.
- Symlink policy explicit; avoid following links outside context.
- Atomic export and safe destination replacement.
- No arbitrary shell execution in MVP.
- Renderer plugins (`PLUGIN`/`TRANSFORM`) are user-declared local modules run in a separate Node process under `--permission` (own-folder reads only; no writes, processes, network or environment). This is a seat belt for trusted code, not a sandbox for hostile code; see docs/PLUGINS.md.

## Execution
Parse -> validate -> resolve imports -> plan -> execute operations -> export.
Instruction order defines write precedence; independent reads may execute concurrently.
Initial executor: local Node.js. BuildKit and OCI are future adapters, not dependencies.
## Incremental rebuilds

`build --watch` and `serve --watch` keep a memo (`src/memo.ts`, `rust/src/memo.rs`) for the session, in the style of
Bazel's action cache: each step's result is stored under a key that is the SHA-256 of the step's identity and the
content digests of everything it reads, and a later build reuses the result when the key is the same.

- **Imports**: the key is the import's planned spec (URI, `AS`, `SORT BY`, Sitefile position) and the digest of the
  file or tree it read (remote imports: the fetched bytes and final URL; they are still fetched and locked every
  build). Parsing (Markdown, JSON, YAML, CSV, archives, web bundles) is skipped on a hit; collections are memoized
  per document (file digest, path, label), so editing one post re-parses one post.
- **Operations**: an operation's *files* are keyed by the operation (with its line, the ARG/LET values and the
  Sitefile label), the digest of its source (local file or tree digest, the key of an import or of a stage's final
  tree) and the keys of the imports it names: a template's variables, `#each` lists and `#if` conditions, a FOR
  list, the names in destinations, options and arguments. A COPY of a file also records whether the destination is
  a directory in the input tree. Then the files are added to the current tree (conflict checks run as usual); the
  resulting tree is keyed by the input tree's key plus the step's, like image layers, so an unchanged prefix of a
  stage is reused whole. SITEMAP reads the whole tree, so only that chained key can reuse it.
- **FOR elements**: `RENDER … FOR` pages are keyed by the template, the element and the imports the template names
  other than the loop variable; `TRANSFORM … FOR` results by the plugin or tool, source and element. A template that
  lists every post (a nav) depends on the whole list, so all its pages re-render; one that does not re-renders only
  the edited post's page.
- **TRANSFORM**: memoized only when its code has an identity: a wasm module's digest, a JS plugin's folder digest, a
  tool's path, size and mtime (as for the disk cache); a plugin that answers `cache: false` is never reused.
- **What a hit replays**: the warnings the step printed (stored with it). A tool's stderr is not replayed, as on a
  TRANSFORM cache hit. Failures are never memoized, so a failing step fails again the same way.

The memo is **in memory only**, by design. A watch session is where rebuilds repeat, it already holds the previous
output, and keeping the memo there means no new on-disk format, no stale entries across Sitefile versions or
implementations, no extra `$SITEFILE_HOME` space to account for under `SITEFILE_CACHE_MAX`, and no question of trusting
cached trees written by another process. The expensive cross-process step, TRANSFORM, already has its disk cache.
One-shot `build` keeps no memo. Entries not used by the latest build are dropped (after a failed build, the last
successful build's entries are kept), so the memo holds about one build's intermediate results.

What is not incremental: every input is still read and hashed (content digests, never timestamps), and the export
is written in full and swapped in atomically; for large sites with cheap templates the export dominates a rebuild
(bench/README.md). The guarantee is that an incremental build is identical to a clean build: same files, modes,
contents and origins, the same warnings and errors. `test/incremental.test.ts` and `rust/tests/incremental.rs` check
it by making random edits (posts, templates, data, static files, ARGs, the Sitefile, invalid input) across hundreds of
rebuilds and comparing each with a clean build.

`--profile` records each phase (parse, imports, stages, operations, TRANSFORM calls with cache state, export) as a
Chrome trace (`src/profile.ts`, `rust/src/profile.rs`; format in docs/CLI.md); it observes the build and never changes
its output.

## Non-goals for MVP
Dockerfile compatibility, Docker daemon, OCI publishing, plugin execution, `EXEC`, multi-protocol breadth, dynamic server hosting.
