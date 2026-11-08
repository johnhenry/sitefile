# Changelog

## 0.1.0 (unreleased)

- **Explicit network settings: proxy, host rules, connect/read timeouts**
  (johnhenry/sitefile#156), for networks that need a proxy and for sitefile
  Desktop's Settings screen. `--proxy` / `SITEFILE_PROXY` (an `http://` proxy:
  a CONNECT tunnel for https, absolute form for http; `HTTP(S)_PROXY` is still
  never read), `--no-proxy` / `SITEFILE_NO_PROXY` (`NO_PROXY` syntax),
  `--allow-hosts` / `SITEFILE_ALLOW_HOSTS` and `--deny-hosts` /
  `SITEFILE_DENY_HOSTS` (checked on every hop, registries included; deny beats
  allow; the new code `E_HOST_BLOCKED` names the rule), and `--connect-timeout`
  / `--read-timeout` / `SITEFILE_FETCH_TIMEOUT`, on `build`, `validate`,
  `inspect`, `graph`, `why`, `pull`, `push`, `vendor` and `outdated`. A proxy
  never widens the fetch policy: the target is resolved and checked against
  the private-address rule before the proxy is asked. Embedders pass the same
  settings as `FetchPolicy.network` / `RegistryOptions.network`
  (`NetworkSettings`, additive in both implementations). docs/CLI.md,
  "Network"; `test/network.test.ts`, `rust/src/resources/network.rs`, and the
  `net-proxy*`, `net-no-proxy`, `net-allow-hosts` and `net-deny-hosts`
  conformance cases. Added in c131eb3 (#162).

- **Library API for embedders** (johnhenry/sitefile#153), found by embedding
  the crate in sitefile Desktop. Rust: `servers::serve_detached`,
  `restart_detached`, `start_detached`, `stop_server`, `resolve_source`,
  `parse_publish` and `DetachedServe` (the CLI's `serve -d`, `server restart`
  and `server stop` now call them); `executor::store_build_as_image` with
  `ImageInfo` and `StoredImage` (what `build -t` calls);
  `BuildOptions::store_home`, so `FROM <local image>`, `BASE` and the wasm
  module cache use an embedder's store instead of `$SITEFILE_HOME`;
  `BuildOptions::cancel` with `CancelToken`, checked between imports, stages,
  operations and plugin or tool calls, failing with the new code
  `E_CANCELLED`; detached workers are reaped on a background thread, so a
  long-lived host can stop them without waiting out the grace period.
  Both implementations: TRANSFORM cache entries record their PLUGIN or TOOL
  (`"plugin"`, same bytes in both; older entries are still read, keys are
  unchanged), with `usage_by_plugin`/`usageByPlugin`,
  `prune_plugin`/`prunePlugin` and `sitefile cache prune --plugin <name>`; the
  `E_UNSUPPORTED` error of a PLUGIN or TOOL that is not allowed carries
  `needs` (kind, name, flag), with the message unchanged. CLI output is
  otherwise unchanged. docs/RUST.md, "Embedding the crate";
  `rust/tests/embed.rs`. Added in e0c555d (#154).

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
  conformance cases compare the bytes of both implementations. Added in
  3c537de (#149).

- **Example: a fileable plugin for TRANSFORM** (`examples/fileable/`,
  johnhenry/sitefile#145). A JavaScript plugin renders a `@johnhenry/fileable`
  tree into the build with fileable's in-memory `toFiles()` and returns
  `{ files }`, so a JSX file tree needs no directory or zip on disk in
  between. fileable stays out of Sitefile's dependencies: it is a
  devDependency, and `examples/fileable/vendor.mjs` copies the files its
  browser entry point loads into the plugin folder (gitignored; nothing
  generated is committed), the only folder a plugin can read. Its `cmd`
  attribute and other features that need files, processes or the environment
  fail with a clear `E_PLUGIN`. Checked by `npm run examples` and by
  conformance (both CLIs). docs/PLUGINS.md, "Example: rendering a fileable
  tree". Added in 0b5c4b3 (#147).

- **`EXPORT … AS wbn` bakes HEADER and exact REDIRECT rules into the bundle's
  responses** (johnhenry/sitefile#144, #150). Each response gets the headers of the
  `/_headers` blocks matching its URL, with `sitefile serve`'s matching
  (`serve-rules.ts`; a directory URL like `<base>docs/` matches with its
  slash and gets its own response when its headers differ). Exact-path
  `/_redirects` rules with a 3xx status become responses with that status, a
  `location` and an empty body, in `serve`'s order (a forced `301!` replaces
  the file; other rules apply only where `serve` would find nothing). Splats,
  `200` rewrites and `404` rules stay in `/_redirects`, which, like
  `/_headers`, is still in the bundle. Bundles of trees without those files
  are byte-identical to before. packfile's `createWebBundleRouter()` now
  answers with the HEADER values and redirects (`test/packfile-interop.test.ts`);
  conformance cases `ok-wbn-rules` and `ok-wbn-rules-files` compare both
  implementations' bytes. The TypeScript `headersFor` no longer drops a
  header named `__proto__` (the Rust port never did); this also
  changes the TypeScript `sitefile serve`, which now sends such a header as
  the Rust binary does. Added in 429ab24 (#150).

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
