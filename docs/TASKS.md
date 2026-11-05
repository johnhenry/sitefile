# Tasks

Checked items are implemented and covered by tests (`npm test`).

## M0 — Executable vertical slice
- [x] Logical-line scanner: continuations, comments, quoting, escapes, `${NAME}` interpolation, accurate line/column spans (`test/language.test.ts`)
- [x] Parser to a typed AST for FROM, ARG, LET, IMPORT, COPY, RENDER, EXPORT; `RUN` and future instructions rejected (`test/language.test.ts`)
- [x] Semantic validator: FROM/EXPORT placement, definitions before use, duplicate names, unknown import refs (`test/language.test.ts`)
- [x] Planner: evaluate ARG (typed, `--build-arg`) and LET, normalize paths, classify sources (`test/build.test.ts`)
- [x] Immutable `Tree` with deterministic owners and conflict policy (`--overwrite`) (`test/build.test.ts`)
- [x] Local context sandbox: traversal, absolute paths, symlink escapes, cycles, ignore list (`test/local.test.ts`, `test/build.test.ts`)
- [x] COPY file/directory and trailing-slash semantics (`test/build.test.ts`)
- [x] RENDER with `--escape=html|none`, template error positions (`test/render.test.ts`, `test/build.test.ts`)
- [x] Atomic directory export with safe replacement rules (`test/exporter.test.ts`, `test/cli.test.ts`)
- [x] CLI: `build`, `validate`, `inspect` (`test/cli.test.ts`)
- [x] Acceptance: `examples/hello` → `dist/index.html` + `dist/style.css` (`test/cli.test.ts`)

## M1 — Remote inputs
- [x] IMPORT local files and directories, `AS file|text|json|tree` (`test/build.test.ts`)
- [x] HTTPS fetcher: https-only default, credential rejection, connect-time SSRF checks, manual redirects re-checked per hop, size limit, timeout, no decompression (`test/https.test.ts`, `test/netpolicy.test.ts`)
- [x] `INTEGRITY sha256:` pins (`test/build.test.ts`)
- [x] `Sitefile.lock`: record, drift detection, `--frozen-lockfile`, `--update-lock` (`test/lockfile.test.ts`, `test/build.test.ts`)
- [x] Acceptance: pinned remote fixture copied into output (`test/build.test.ts`, served from a local test server)
- [x] Text and JSON imports readable from templates (`${name}`, `${name.a.0.b}`) — an early slice of M3

## Next

Open work is tracked as GitHub issues in `johnhenry/sitefile`:

- [x] #1 `FROM <local tree>` and multi-stage `FROM … AS name` (M2) (`test/language.test.ts`, `test/build.test.ts`, `test/cli.test.ts`)
- [x] #2 `COPY --from=<stage>` (M2), also `RENDER --from` (`test/build.test.ts`)
- [x] #3 Remote tree imports (archives, with decompression-ratio limits) (`test/archive.test.ts`, `test/build.test.ts`)
- [x] #4 `.sitefileignore`
- [x] #5 Export check-then-swap race: parent and destination dev/ino re-validated before and after the swap (`test/exporter.test.ts`)
- [x] #6 Hard links to files outside the context
- [x] #7 TLS test fixture for `fetchRemote`
- [x] Inline content: `COPY <<EOF` and `RENDER <<EOF … TO` heredocs (`test/inline.test.ts`, `test/cli.test.ts`, `examples/inline/`)
