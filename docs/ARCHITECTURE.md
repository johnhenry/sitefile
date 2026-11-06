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
10. cli: build, validate, inspect, graph, why.

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
  other than the loop variable; `TRANSFORM … FOR` results by the plugin or tool, source and element; a directory
  TRANSFORM's results per file, by the plugin or tool, the file's relative path and its content digest. A template that
  lists every post (a nav) depends on the whole list, so all its pages re-render; one that does not re-renders only
  the edited post's page.
- **TRANSFORM**: memoized only when its code has an identity: a wasm module's digest, a JS plugin's folder digest, a
  tool's path, size and mtime (as for the disk cache); a plugin that answers `cache: false` is never reused.
- **Imported Sitefiles** (`IMPORT … AS sitefile`): their imports and steps are memoized with the same rules, keyed by
  the imported Sitefile's own label and ARG/LET values; the key of a stage of theirs that this Sitefile reads is the
  key of that stage's final tree, so an edit inside the imported directory rebuilds only what depends on it.
- **`--target`** builds only the stages up to the target; every key is per step, so a step is reused exactly when
  it would compute the same thing whatever the target (the randomized tests switch `--target` between rebuilds).
- **What a hit replays**: the warnings the step printed (stored with it). A tool's stderr is not replayed, as on a
  TRANSFORM cache hit. Failures are never memoized, so a failing step fails again the same way.

The memo is **in memory only**, by design. A watch session is where rebuilds repeat, it already holds the previous
output, and keeping the memo there means no new on-disk format, no stale entries across Sitefile versions or
implementations, no extra `$SITEFILE_HOME` space to account for under `SITEFILE_CACHE_MAX`, and no question of trusting
cached trees written by another process. The expensive cross-process step, TRANSFORM, already has its disk cache.
One-shot `build` keeps no memo. Entries not used by the latest build are dropped (after a failed build, the last
successful build's entries are kept), so the memo holds about one build's intermediate results.

What is not incremental: every input is still read and hashed (content digests, never timestamps). The guarantee is
that an incremental build is identical to a clean build: same files, modes, contents and origins, the same warnings and
errors. `test/incremental.test.ts` and `rust/tests/incremental.rs` check it by making random edits (posts, templates,
data, static files, ARGs, the Sitefile, invalid input) across hundreds of rebuilds and comparing each with a clean
build, and the exported directory with a clean export.

### Incremental export

A one-shot `build` writes the whole output to a temporary sibling and swaps it in (`src/exporter.ts`). For 2000-5000
files that is most of a watch rebuild, so a `build --watch` session with a **directory** export writes only what
changed (`ExportState`, held by the session's memo; `exportIncremental` in `src/exporter.ts`,
`rust/src/export/incremental.rs`). Archive formats (zip, tar, tgz, wbn) and oci always rewrite in full, as do one-shot
builds and `SITEFILE_NO_INCREMENTAL=1` sessions.

- **What is recorded**: after each export of the session, the destination's real path, the dev/ino of it and of its
  parent, and per file the tree digest and mode plus its lstat stamp right after Sitefile wrote it (dev, ino, size,
  mtime, ctime, mode). Both trees' digests and modes are in memory, so the diff costs nothing.
- **Is the output as we left it?** Before applying anything: the same destination directory and parent (dev/ino), a
  walk of the output (one `readdir` per directory) finding exactly the recorded directories and files and nothing
  else (no extra file or empty directory, no symlink), and one `lstat` per file matching its stamp. ctime changes on
  any write, chmod or rename and cannot be set back, so an edit that keeps the size and restores the mtime is still
  seen. The check costs about one `lstat` per output file, a fraction of writing them.
- **Falls back to the full swap** when nothing is recorded (the first build), the destination changed path or
  identity, the output was modified, more than half of the files change (`--target` switches, say), or a new path
  already exists under another spelling (a case-only rename on a case-insensitive disk, a name the file system
  normalizes): the full export then reports collisions exactly as a clean build does.
- **Applying**: the new contents are written to a temporary sibling first (the slow part; the output is untouched and
  a failure there leaves it as it was). Then deleted files are unlinked, directories that became empty removed
  (deepest first), new directories created, and each staged file renamed over its path, every step checking that what
  it replaces is the file it recorded (by stamp) and that the renamed file is the one staged (by dev/ino). Nothing is
  followed through a symlink: the walk refuses any, every directory created is checked to be a real directory, and
  the destination and parent identities are re-checked right before applying.
- **The guarantee**: a failed build never touches the output (the export runs only after the build succeeded). Each
  file is at every moment either its previous or its new version (`rename` is atomic), and files that did not change
  are never rewritten. The output as a whole holds a mix of the two builds only while the renames and unlinks run,
  after all the bytes were written: milliseconds for a few changed files, not the seconds a full export takes. If a
  step fails part way (a concurrent change, a full disk), the full write-and-swap runs and replaces the whole output
  atomically; only if that fails as well is the output left partly updated, and the error is reported. A full swap
  itself has the brief window in which `dist/` does not exist (two renames); the incremental path does not.
- **Residual risks** are those of the full export (path-based checks without `openat`, so a process with write
  access to the output can race a check and the following call; we detect it afterwards and never "fix" it
  silently), plus: a change made to an untouched file between the walk and the end of the apply is not seen by that
  export (the next one sees it, since its stamp no longer matches); a modification inside the same clock tick as
  Sitefile's own write, keeping the size, would not change the stamp.

The randomized tests modify the exported directory between rebuilds (rewrite a file keeping its size and mtime,
append, add a file or an empty directory, delete a file or a directory, chmod, replace a file or a directory with a
symlink to a directory outside) and require the next export to equal a clean one byte for byte and mode for mode,
with the symlinks' targets untouched.

`--profile` records each phase (parse, imports, stages, operations, TRANSFORM calls with cache state, export) as a
Chrome trace (`src/profile.ts`, `rust/src/profile.rs`; format in docs/CLI.md); it observes the build and never changes
its output.

## Non-goals for MVP
Dockerfile compatibility, Docker daemon, OCI publishing, plugin execution, `EXEC`, multi-protocol breadth, dynamic server hosting.
