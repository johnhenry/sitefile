# Milestones

## M0 — Executable vertical slice
CLI, parser, local tree composer, ARG/LET, COPY, RENDER, EXPORT, fixture tests. Acceptance: hello example generates index.html and style.css.

## M1 — Remote inputs
IMPORT named local/HTTPS file resources; size limits, integrity pins, source policies, basic lockfile. Acceptance: pinned remote fixture copied into output.

## M2 — Trees and stages
FROM local tree, multi-stage FROM/AS, COPY --from or namespace syntax, deterministic conflict rules, symlink safety.

## M3 — Structured content
JSON/YAML data imports, renderer adapter, escaping rules, template partials and layouts.

## M4 — Incremental reproducibility
Content-addressed store, graph scheduling, lockfile enforcement, provenance, inspect/explain.

## M5 — Extensibility
Plugin SDK, Git/IPFS/FTP/OCI resolvers, transforms, generate, sandboxed executable adapters.

## M6 — Distribution
OCI import/export, BuildKit adapter.
