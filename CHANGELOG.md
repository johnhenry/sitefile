# Changelog

## 0.0.0 — first npm release (2026-11-06)

`@johnhenry/sitefile` is a new package: Sitefile was never published to npm
under any name, and `0.0.0` is its first version. It joins the @johnhenry
family as its static build orchestrator, next to `@johnhenry/fileable` (file
trees in JSX) and `@johnhenry/packfile` (the Web Bundle reader and server).

- **Published as `@johnhenry/sitefile`** (was `sitefile`, `private: true`,
  `0.1.0` in `package.json` only). The tarball ships `lib/src/` (compiled by
  `prepack`, no source maps), the README, `LICENSE` and this file; it has no
  runtime dependencies. The Rust port stays at `0.1.0` in `rust/Cargo.toml`
  and is not published (johnhenry/sitefile#69).
- **MIT `LICENSE` file added.** `package.json` and `rust/Cargo.toml` already
  said MIT. See johnhenry/sitefile#69.
- **Node floor raised from 22 to 26** (`engines`, `.nvmrc`, CI), the family
  floor. The suite passed unchanged on Node 26.
- **CI on every push and pull request** (johnhenry/sitefile#67): typecheck,
  tests and the examples smoke test (`npm run examples`), the Rust
  fmt/build/test (clippy non-blocking until triaged), conformance between the
  two implementations with the release binary, the VS Code extension and
  tree-sitter grammar tests, actionlint, and the family's workflow lint.
  `publish.yml` publishes from `main` through npm trusted publishing.
- **Interop with `@johnhenry/packfile` is tested**
  (`test/packfile-interop.test.ts`, packfile as a devDependency only): packfile
  reads a Sitefile `.wbn` back into exactly the exported files and serves it,
  and its `fromArchive()` reads the same bundle once gzipped. The other
  direction, importing gzip-wrapped bundles from fileable or packfile with
  `AS tree`, is johnhenry/sitefile#141.
- **Docs:** README badges, `## Contents`, `## Install`, `## Adding a new
  plugin`, `## Security model` (the former "Safety defaults", with what stays
  the user's responsibility), `## Family`; `examples/README.md`;
  `AGENTS.md` reshaped into the family playbook with `CLAUDE.md` as a symlink.
