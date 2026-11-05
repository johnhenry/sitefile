# Codex instructions

You are implementing Sitefile, a Docker-inspired static artifact build tool.

## Rules
- TypeScript, Node.js >=22, ESM, strict typing. Prefer small modules and standard libraries.
- Preserve the conceptual model: Resource -> immutable File/Tree -> Operation -> Artifact.
- A Sitefile is parsed into a typed IR; execution must never parse raw text ad hoc.
- Keep imports separate from output placement; `IMPORT` names a resource, `COPY` places it.
- `FROM` initializes a stage's tree; `FROM scratch` starts empty.
- Do not execute untrusted scripts or enable arbitrary `RUN` in MVP.
- Prevent path traversal, symlink escapes, SSRF (including redirects), and accidental credential exposure.
- Remote resources should support integrity pins and lockfile recording; do not promise reproducibility for unpinned URLs.
- Use explicit errors with source file, line, column where possible.
- Add tests for every new instruction and security boundary.
- Never silently overwrite different files; require an explicit policy.
- Avoid new runtime dependencies without documenting rationale.
- Each PR: concise scope, tests, docs, and example usage.
- Do not claim features implemented until tests demonstrate them.

## Workflow
1. Read `docs/ARCHITECTURE.md`, `docs/LANGUAGE.md`, `docs/MILESTONES.md`.
2. Implement one milestone or issue at a time.
3. Run typecheck, unit tests, and end-to-end fixture tests.
4. Update documentation and mark tasks complete only after validation.
5. Preserve compatibility with existing fixtures.

## MVP acceptance
Building `examples/hello/Sitefile` produces `dist/index.html` with a substituted title and copied CSS, no container runtime required.
