# Changelog

## 0.1.0 (unreleased)

- **`EXPORT … AS wbn COMPRESSION gzip`** (johnhenry/sitefile#143): a Web Bundle
  export can now be gzip(`application/webbundle`), the `.wbn` archive format of
  `@johnhenry/packfile` (`toArchive`/`fromArchive`) and `@johnhenry/fileable`
  (`encode="wbn"`). `COMPRESSION` used to be `E_VALIDATION` on anything but
  `oci`; it now applies to `oci` and `wbn`, and on `wbn` defaults to `none`, so
  existing exports are byte-for-byte unchanged. The gzip layer only wraps the
  finished bundle and uses the shared deterministic encoder (`src/deflate.ts`,
  `rust/src/deflate.rs`), so both CLIs write identical bytes. On the CLI,
  `-o site.wbn.gz` selects a gzipped wbn (also for `sitefile save`), the
  default destination of such a Sitefile is `dist.wbn.gz`, and the new
  `--compression gzip|none` overrides `EXPORT … COMPRESSION` for the export's
  format (oci or wbn; any other format is a usage error); `--oci-compression`
  stays as the oci-only spelling. A Sitefile's `COMPRESSION` belongs to its own
  `AS` format: `--format wbn` on an `AS oci COMPRESSION …` Sitefile does not
  inherit it. `test/packfile-interop.test.ts` now reads a gzip export built with
  `BASE https://packfile.invalid/` through plain `fromArchive(buffer)`; new
  conformance cases compare the bytes of both implementations.

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
- **Fixed: a replaced archive destination went unnoticed on Linux.** Before
  renaming the new archive into place, `exportFile` (and the Rust
  `export_file`) checked that the existing destination was still the file
  they inspected by device and inode number only. Linux often gives a file
  created right after an unlink the same inode number, so a destination
  swapped mid-export passed the check and was replaced. The check now also
  compares size and nanosecond `ctime`. Found by the new CI's first Linux
  run; the existing test only ever passed on macOS. Fixed in 0e0a1a1 (#142).
- **Fixed: a case-only collision inside one directory had no source line.**
  Two names that differ only in case in a directory read by `COPY` (possible
  on a case-sensitive file system) failed with `E_PATH_COLLISION` but without
  the instruction's line; both implementations now report it. Its test only
  runs on case-sensitive file systems, so it had never run. Fixed in 0e0a1a1
  (#142).
- **Docs:** README badges, `## Contents`, `## Install`, `## Adding a new
  plugin`, `## Security model` (the former "Safety defaults", with what stays
  the user's responsibility), `## Family`; `examples/README.md`;
  `AGENTS.md` reshaped into the family playbook with `CLAUDE.md` as a symlink.
