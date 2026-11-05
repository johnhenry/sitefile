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

## Non-goals for MVP
Dockerfile compatibility, Docker daemon, OCI publishing, plugin execution, `EXEC`, multi-protocol breadth, dynamic server hosting.
