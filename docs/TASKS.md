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
- [x] #17 Export formats: `EXPORT ... AS zip|tar|tgz|wbn [BASE url]`, `--format`, `--base-url`; deterministic archives and an unsigned Web Bundle (`test/export-formats.test.ts`)
- [x] Inline content: `COPY <<EOF` and `RENDER <<EOF … TO` heredocs (`test/inline.test.ts`, `test/cli.test.ts`, `examples/inline/`)
- [x] #16 Blog support: `IMPORT … AS markdown` with frontmatter, `SORT BY`, `RENDER … FOR`, template `${#each}` / `${#if}` / `|raw` (`test/blog.test.ts`, `test/markdown.test.ts`, `test/frontmatter.test.ts`, `examples/blog/`)
- [x] #20 Data imports: `AS yaml`, `AS csv`, `AS wbn`, Web Bundles via `AS tree`, JSON/YAML directory collections, `SORT BY` on any list (`test/data-imports.test.ts`, `test/yaml.test.ts`, `test/csv.test.ts`, `test/wbn-reader.test.ts`, `examples/data/`)
- [x] `--build-arg-file` (.json, .yaml, .env) and `--build-arg NAME` from the environment (`test/build-args.test.ts`)
- [x] #22 Custom renderers: `PLUGIN` + `TRANSFORM` in a `--permission` sandbox (`test/plugins.test.ts`, `test/motion-plugin.test.ts`, `examples/motion/`)
- [x] #30 Allow-listed exec adapter: `TOOL` + `TRANSFORM … ARGS`, `--allow-exec` (`test/exec-tool.test.ts`, incl. an ffmpeg clip when ffmpeg is installed)
- [x] #31 TRANSFORM result cache in `$SITEFILE_HOME/cache/transform`, lazy plugin start, `--no-cache`, `cache info|prune`, `export const cache = false` (`test/transform-cache.test.ts`)
- [x] #74 Bounded caches: `SITEFILE_CACHE_MAX` (default 1 GiB, `0` off) LRU-evicts TRANSFORM results, wasm modules, untagged images and unused blobs after each build (never tagged images), mtime recency with grace periods, `cache du|ls`, `cache prune --max-size` (`test/cache-limit.test.ts`, `rust/src/cache_limit.rs`)
- [x] #23 Local image store (an OCI image layout): `build -t`, `images`, `image inspect|diff|prune`, `tag`, `rmi`, `save`, reference grammar, lock file, digest verification (`test/store.test.ts`)
- [x] #24 Site servers: `serve [-d]`, `servers`, `server stop|rm|logs|restart`; `node:http` server with ETag/304, SPA fallback, traversal rejection (`test/server.test.ts`)
- [x] `EXPORT ... AS oci [AT root]`, `--oci-root`, `--platform`: reproducible single-layer OCI image, archive and layout (`test/oci.test.ts`)
- [x] #28 Registry `push` / `pull` over the OCI distribution spec: token/Basic auth, Docker config + credential helpers, digest-verified streaming pulls, blob existence checks and mounts, redirect/SSRF policy (`test/registry.test.ts`, `test/engine.test.ts`)
- [x] #29 Runnable images: `EXPORT / AS oci BASE <image>` / `--base`, base config and layers preserved, base digest in `Sitefile.lock` `images` (`test/registry.test.ts`, `test/engine.test.ts`)
- [x] OCI exports name the image the way `docker save` does (`io.containerd.image.name` + tag in `ref.name`) so `docker load` finds it by name (`test/oci.test.ts`, `test/engine.test.ts`)
- [x] #34 WebAssembly plugins: `PLUGIN x FROM ./x.wasm`, import-free ABI v1, worker + timeout, Rust example (`test/wasm-plugins.test.ts`, `examples/wasm/`)
- [x] #37 Rust port (`rust/`), conformance suite (39/39), comparison benchmarks (`bench/`)
