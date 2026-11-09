# Agent playbook

`@johnhenry/sitefile` — a Docker-inspired static artifact builder. Single npm
package, Node >= 26, `node:test` (`npm test`), builds to `lib/` with `tsc`
(gitignored; `prepack` rebuilds it). It has **two implementations that must
stay byte-identical**: the TypeScript reference (`src/`, the library API and
the specification) and the Rust port (`rust/`, the shipped binary and the
language server). Most changes land in both.

`CLAUDE.md` in this directory is a symlink to this file.

## The verification loop (before every push)

CI (`.github/workflows/ci.yml`) runs these in this order; match it locally.

1. `npm run typecheck`
2. `npm test`. Only two kinds of test may skip: the three `engine:` tests
   when no container engine is reachable (`docker version`/`podman version`
   fails), and the external OCI validators test when none of skopeo, crane,
   oras, umoci or podman is installed. Anything else skipped is a red flag.
   Case-sensitivity and inode tests only really run on Linux, so a green
   macOS run is not enough for `src/tree.ts`, `src/exporter.ts` or
   `src/resources/local.ts`: wait for CI.
3. `(cd rust && cargo fmt --all -- --check && cargo build --release && cargo test --release)`
4. `npm run conformance` — runs both CLIs on every example and every
   `conformance/cases/*` and requires the same exit code, error line and
   output bytes. Needs `npm run build` and the release Rust binary from step 3;
   the network cases need `openssl`.
5. `(cd editors/vscode && npm ci && npm test)` and
   `(cd editors/tree-sitter-sitefile && npm ci && npm test)` when you touch
   the language, the grammar or the editor code.
6. `npm pack --dry-run` — read the file list: `lib/src/` (no source maps),
   `README.md`, `LICENSE`, `CHANGELOG.md`, `package.json`.
7. A fresh clone:
   `git clone . /tmp/sitefile-verifyN && cd $_ && npm ci && npm test`.

## Repo-specific gotchas

- **A feature in one implementation only is a conformance failure, not a
  follow-up.** Add the conformance case or differential fixture with the
  feature (docs/RUST.md, "How parity is checked"). Real, accepted
  differences go in docs/RUST.md "Known differences", nowhere else.
- **Both sides share one deflate encoder** (`src/deflate.ts`,
  `rust/src/deflate.rs`) so that zip, tar.gz and OCI layers compare byte for
  byte. Never swap in `zlib`/`flate2` for writing: identical image IDs
  depend on it.
- **Tests must not touch `~/.sitefile`.** `test/helpers/isolate-home.ts` is
  preloaded into every test process and gives it its own `SITEFILE_HOME`;
  builds evict cache entries, so a shared home deletes the user's cache.
  Spawned CLIs inherit it — keep `--import` in the test script.
- **No new runtime dependencies** without a written rationale (npm side:
  there are none; Rust side: see the comment block in `rust/Cargo.toml`).
  The family integrates with sitefile through file formats (`.wbn`, trees,
  OCI), not imports; `@johnhenry/packfile` is a devDependency for the interop
  test only.
- **Never parse raw Sitefile text outside the parser.** A Sitefile becomes a
  typed IR (`src/language/`); execution works on the IR only.
- **Security boundaries need tests**: path traversal, symlink and hard-link
  escapes, SSRF including redirects, secrets leaking into outputs, caches,
  the lockfile or traces. Every new instruction gets tests too.
- **Never silently overwrite different content**; require an explicit
  policy (`--overwrite`, `--force`).

## Definition of done

- Typecheck, tests, Rust tests and conformance are green, with no
  unexpected skips.
- A regression test exists for any bug fixed.
- The feature is in both implementations, or its difference is listed in
  docs/RUST.md.
- docs/LANGUAGE.md or docs/CLI.md describes it, the README table is updated
  when an instruction changes, and an example exists for a new instruction.
- `CHANGELOG.md` has an entry citing the commit and PR.
- Don't claim a feature in docs before a test demonstrates it.

## Non-goals

- No `RUN` and no shell: external programs only through `TOOL` +
  `--allow-exec`, argv only, scrubbed environment (docs/PLUGINS.md).
- No container runtime dependency.
- No promise of reproducibility for unpinned URLs (they are locked in
  `Sitefile.lock`, and `--strict-pins` refuses them).

## Releases

`main` is the release branch. Bump `version` in `package.json` and the same
value in `rust/Cargo.toml` (and its `Cargo.lock` entry; `test/version.test.ts`
and `rust/tests/version.rs` fail otherwise), and add the dated `CHANGELOG.md`
entry in a PR; merging it publishes through
`.github/workflows/publish.yml` (npm trusted publishing, idempotent
`npm view` guard), which then creates the `v<version>` tag and GitHub
Release. Don't run `gh release create`. Don't rename `publish.yml`: npm
trusted publishing is bound to that filename. Rust binaries and the VS Code
extension are not released yet (johnhenry/sitefile#69).
