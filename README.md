# Sitefile

Docker-inspired static artifact builder. Compose immutable filesystem trees from
local and remote sources, render templates, and export a deployable directory —
no container runtime, no shell.

```
# examples/hello/Sitefile
ARG TITLE = "Hello"
LET GREETING = "Welcome to ${TITLE}."
COPY ./public/ /
RENDER --escape=html ./templates/index.html TO /index.html
EXPORT / AS directory
```

```bash
npm install && npm run build
node lib/src/cli.js build examples/hello --build-arg TITLE="My site"
```

This writes `examples/hello/dist/index.html` (with the title substituted) and
`examples/hello/dist/style.css`.

## Status

Milestones **M0** (local vertical slice), **M1** (remote inputs) and **M2**
(trees and stages: multi-stage builds, `COPY --from`, archive imports,
`.sitefileignore`; see `examples/multistage`) are implemented and tested; see [docs/TASKS.md](docs/TASKS.md). Requires Node.js 22+,
no runtime dependencies.

| Instruction | Supported |
| --- | --- |
| `FROM scratch\|<dir>\|<stage>\|image <ref> [AT <dir>] [AS name]` | yes; several FROMs make a multi-stage build, the last stage is exported; `image` starts from a site image's files (a theme: `examples/theme`, `examples/themed-site`), locked in `Sitefile.lock` when it comes from a registry |
| `ARG NAME[: type] [= value]` | yes, `string`/`number`/`boolean`, `--build-arg` |
| `LET NAME[: type] = value` | yes |
| `IMPORT name FROM <path or https URL> [AS file\|text\|json\|yaml\|csv\|markdown\|wbn\|tree] [INTEGRITY sha256:…] [SORT BY key [DESC]] [SCHEMA <file>] [DRAFTS true\|false]` | yes; `AS tree` unpacks tar, tar.gz, zip and Web Bundles; `AS markdown` reads posts with frontmatter (`SCHEMA` checks it against a YAML/JSON schema); `AS yaml`/`csv`/`wbn` give data for templates |
| `COPY [--overwrite] [--from=<stage>] <source> <dest>` | yes; source is a local path, `name:/path`, `<<EOF` inline content, or with `--from` an absolute path in an earlier stage |
| `RENDER [--escape=html] [--overwrite] [--from=<stage>] <template> TO <dest> [FOR item IN list [BY key] [PAGE n [FIRST <dest>]]]` | yes; template is a file or `<<EOF` inline body; `${NAME}`, `${json.path}`, `${#each}`, `${#if}`, `${x\|raw}`; `FOR` renders one page per element, `BY` one per tag, `PAGE` paginates |
| `PLUGIN name FROM ./plugins/x.mjs\|x.wasm [INTEGRITY sha256:…]` | yes; a custom renderer: a sandboxed JS module (runs only with `--allow-plugin name`) or an import-free WebAssembly module (no flag needed; see [docs/PLUGINS.md](docs/PLUGINS.md), `examples/wasm/`) |
| `TRANSFORM [--include/--exclude=<pattern>] [<source>] WITH name TO <dest> [OPTIONS k=v …] [FOR item IN list]` | yes; e.g. text → animated SVG (`examples/motion/`), responsive images (`examples/image/`); a directory source runs once per file (`TO /img/thumbs/*.png`) |
| `TOOL name` + `TRANSFORM … WITH name TO <dest> [OPTIONS …] [FOR …] ARGS …` | yes; run an external program (e.g. `ffmpeg`) you allow with `--allow-exec name`: argv only, no shell, temp files, scrubbed environment (see [docs/PLUGINS.md](docs/PLUGINS.md)) |
| `SITEMAP BASE <url> [TO <dest>]` | yes; a sitemaps.org `/sitemap.xml` of the `.html` pages so far |
| `FEED <import> BASE <url> LINK <path> [TO <dest>] [TITLE <text>]` | yes; an Atom `/feed.xml` from Markdown posts (`title` and `date` required) |
| `REDIRECT <from> <to> [STATUS 301\|302\|303\|307\|308]`, `HEADER <pattern> <Name>: <value>` | yes; `/_redirects` and `/_headers` for Netlify / Cloudflare Pages (and `sitefile serve`) |
| `CHECKLINKS [--warn] [--external]` | yes; fails (or warns) on links to files that do not exist, with file and line; `--external` requests http(s) links under the network policy |
| `FINGERPRINT [--overwrite] <pattern>… [MANIFEST <dest>]` | yes; renames assets to `name.<hash>.ext` and rewrites the HTML and CSS that reference them, plus a JSON manifest |
| `EXPORT <path> [AS directory\|zip\|tar\|tgz\|wbn\|oci] [BASE <https-url>] [AT <root>]` | yes; archives are deterministic, `wbn` is an unsigned Web Bundle (needs `BASE` or `--base-url`), `oci` is a single-layer OCI image of the files (under `AT`, default `/srv/www`) |

Templates can also live in the Sitefile itself (see `examples/inline/`):

```
RENDER --escape=html <<HTML TO /index.html
<h1>${TITLE}</h1>
HTML
```

A blog from Markdown posts with frontmatter (see `examples/blog/`):

```
ARG DRAFTS: boolean = false                          # --build-arg DRAFTS=true previews drafts
IMPORT posts FROM ./posts AS markdown SORT BY date DESC SCHEMA ./post.schema.yaml DRAFTS ${DRAFTS}
RENDER --escape=html ./templates/index.html TO /page/${page.number}/index.html FOR page IN posts PAGE 10 FIRST /index.html
RENDER --escape=html ./templates/post.html TO /posts/${post.slug}/index.html FOR post IN posts
RENDER --escape=html ./templates/tag.html TO /tags/${tag.key}/index.html FOR tag IN posts BY tags
FEED posts BASE ${SITE_URL} LINK /posts/${item.slug}/index.html TITLE ${SITE_TITLE}
SITEMAP BASE ${SITE_URL}
FINGERPRINT /style.css MANIFEST /asset-manifest.json
CHECKLINKS
REDIRECT /feed /feed.xml
HEADER /*.css Cache-Control: public, max-age=3600
```

Data files are readable too (see `examples/data/`):

```
IMPORT site FROM ./site.yaml AS yaml                 # ${site.nav.0.title}
IMPORT team FROM ./team.csv AS csv SORT BY joined    # FOR person IN team
IMPORT bundle FROM ./vendor/hello.wbn AS wbn         # ${#each bundle.entries as e}
IMPORT hello FROM ./vendor/hello.wbn AS tree         # COPY hello:/ /hello/
```

Custom renderers are plugins that run in a sandboxed Node process (see `examples/motion/`):

```
PLUGIN motion FROM ./plugins/motion.mjs
TRANSFORM ./title.txt WITH motion TO /title.svg OPTIONS style=typewriter
TRANSFORM WITH motion TO /cards/${post.slug}.svg OPTIONS text=${post.title} FOR post IN posts
```

A JavaScript plugin is code from the repository you are building, so it runs only when you allow it:

```
sitefile build --allow-plugin motion     # or --allow-plugins for every PLUGIN
```

WebAssembly plugins (`PLUGIN … FROM ./x.wasm`) import nothing, can only compute, and need no flag (see `examples/wasm/`).

Images: the first-party image plugin (`plugins/image/image.wasm`, WebAssembly) decodes PNG, JPEG, GIF and WebP, resizes with a fixed Lanczos-3 filter and writes PNG, JPEG or lossless WebP, with a metadata file listing each variant's path, width, height and a ready `srcset` (see [docs/PLUGINS.md](docs/PLUGINS.md#the-image-plugin), `examples/image/`):

```
PLUGIN image FROM ./plugins/image.wasm INTEGRITY sha256:…
TRANSFORM ./photos/sky.jpg WITH image TO /img/ OPTIONS widths=400,800,1200 quality=80
```

External programs are explicit opt-ins: declare a `TOOL`, and allow it on the command line.

```
TOOL ffmpeg
TRANSFORM WITH ffmpeg TO /intro.mp4 ARGS -f lavfi -i "color=c=black:s=1280x720:d=3" -fflags +bitexact -map_metadata -1 {output}
```

```
sitefile build --allow-exec ffmpeg
```

TRANSFORM results are cached in `$SITEFILE_HOME/cache/transform` (`--no-cache` to skip; `sitefile cache info|prune`).
CI runs and teammates can share them through an OCI registry: `--cache-to <registry/name:tag>` pushes the results a
build used, `--cache-from <ref>` reuses them (see [docs/CLI.md](docs/CLI.md#registry-cache)).

## CLI

```
sitefile build [context] [-o dist] [--format directory|zip|tar|tgz|wbn] [--base-url URL]
               [--build-arg K=V | --build-arg K] [--build-arg-file vars.json|.yaml|.env] [--force]
               [--frozen-lockfile | --update-lock] [--strict-pins] [--allow-http] [--allow-private-network]
               [--allow-hardlinks] [--allow-exec name[=/abs/path]]... [--allow-plugin name]... [--allow-plugins]
               [--no-cache] [--cache-from registry/name:tag]... [--cache-to registry/name:tag] [--watch] [--target stage] [--profile trace.json]
sitefile validate [context] [--json] [--target stage]
sitefile inspect  [context] [--json] [--target stage]
sitefile graph    [context] [--json | --dot]   # dependency graph of imports, stages and operations
sitefile why <path> [context] [--json]         # which instructions wrote an exported file, and what overwrote it
sitefile vendor   [context] [--vendor-dir vendor/sitefile]   # download remote inputs for offline builds
sitefile outdated [context] [--json]     # compare remote imports and BASE images with their pins (read-only)
sitefile bake [targets...] [-f sitefile.bake.yaml] [--print [--json]] [-j n]   # build several variants

# images and servers (see below and docs/CLI.md)
sitefile build [context] -t name[:tag]        # also store the build as an image
sitefile images | image inspect|diff|prune | tag | rmi | save
sitefile cache info | cache prune [--older-than days]   # TRANSFORM result cache
sitefile cache du                                       # cache sizes; SITEFILE_CACHE_MAX bounds them (default 1G)
sitefile serve [-d] [--name n] [-p [host:]port] [--spa] <image|./dir>
sitefile serve --watch [-p [host:]port] [--spa] [context]   # build in memory, serve, live reload
sitefile servers [-a] | server stop|rm [-f]|restart|logs [-f] <name>
```

`--target <stage>` (like `docker build --target`) stops after a named stage and exports that stage's whole
tree, in the EXPORT format but without the EXPORT path or `_redirects`/`_headers`; an unknown name is a usage
error listing the stages. See [docs/CLI.md](docs/CLI.md#build-target---target-stage).
`sitefile graph` and `sitefile why` are described in [docs/CLI.md](docs/CLI.md#explaining-a-build-graph-and-why).

## Variants: `sitefile bake`

Like `docker buildx bake`, `sitefile bake` builds several variants of a site (locales, staging and production)
from one bake file, `sitefile.bake.yaml` (or `.yml`, `.json`), with the outputs side by side:

```yaml
groups:
  default: [site]
targets:
  site:
    matrix:                         # one target per combination: site-en-staging, site-en-production, ...
      locale: [en, fr]
      env: [staging, production]
    arg-files: ["env/${env}.env"]   # ${name} is a matrix value
    args: {LOCALE: "${locale}"}
    output: dist/${env}/${locale}
```

```bash
cd examples/bake && sitefile bake       # four builds, up to 4 at a time (-j), sharing the TRANSFORM cache
sitefile bake --print                   # the resolved targets (--json for tools), without building
```

Targets also take `context`, `file`, `target` (a stage), `format`, `tags` and `inherits`. Every target runs even
when one fails (then the exit code is 1); outputs that overlap or a tag used twice are refused before anything is
built. See [docs/CLI.md](docs/CLI.md#variants-sitefile-bake).

## Watch mode

```bash
sitefile build --watch examples/hello     # rebuild dist/ whenever the context, Sitefile or a --build-arg-file changes
sitefile serve --watch examples/hello     # build in memory, serve on 127.0.0.1, reload open pages after each rebuild
```

Rebuilds are debounced (100 ms) and changes made during a build become one follow-up build. The output
directory, `.git`, `Sitefile.lock` and the store are not watched. A failed rebuild prints the error and keeps the
last good output (exports are swapped in atomically; `serve --watch` keeps serving the last good build). Live
reload is a small script added to HTML responses only under `serve --watch`, listening on the reserved
`/__sitefile/reload` event stream; it is never written to exports. Rebuilds are incremental: steps whose inputs
are unchanged (by content digest) are reused from the previous build, in memory, and the result is identical to a
clean build (`SITEFILE_NO_INCREMENTAL=1` turns it off). See [docs/CLI.md](docs/CLI.md#watch-mode).

## Profiling a build

```bash
sitefile build examples/blog --profile trace.json   # open in ui.perfetto.dev or chrome://tracing
```

`--profile` writes a Chrome trace of the build: parsing, each import, plugin start, stage, operation, TRANSFORM call
(with its cache hit or miss), the BASE pull and the export, so slow steps stand out. See
[docs/CLI.md](docs/CLI.md#profiling---profile).

## Images and servers

Builds can be kept in a local, Docker-like **image store**, which is a standard
[OCI image layout](https://github.com/opencontainers/image-spec/blob/main/image-layout.md)
in `$SITEFILE_HOME` (default `~/.sitefile`). An image is the site's files as one
layer; its ID is the manifest digest, and building identical output gives the same ID.

```bash
sitefile build examples/hello -t hello:1          # no -o: stored only, nothing written to the context
sitefile images                                    # REPOSITORY TAG IMAGE ID CREATED FILES SIZE
sitefile save hello:1 -o hello.oci.tar             # or .zip / .tar.gz / .wbn / a directory
sitefile serve -d --name site -p 8080 hello:1      # serve it, in the background
sitefile servers; sitefile server logs site; sitefile server stop site; sitefile server rm site
```

`serve` serves GET/HEAD only on `127.0.0.1` (never `0.0.0.0` unless you ask for it in `-p`), with ETags,
correct MIME types and no directory listings; it serves `/404.html` for missing paths, `/about` from
`/about.html`, and honours the site's `_redirects` and `_headers` (exact paths and `/*` splats). `EXPORT / AS oci` (or `build -o site.oci.tar`) writes the
same OCI image without the store; on its own it contains files only, but `EXPORT / AS oci BASE nginx:alpine
AT /usr/share/nginx/html` (or `--base`) builds a runnable image on top of a pulled base, and `sitefile push` / `pull`
move images to and from any OCI registry. `--provenance` and `--sbom` attach a SLSA provenance statement and a
CycloneDX file list to the image as OCI 1.1 referrers (deterministic, pushed with it; the image ID does not change). See
[docs/CLI.md](docs/CLI.md).

## Safety defaults

- Local reads stay inside the build context (no `..`, absolute paths or symlinks out).
  Hard-linked files are refused (`E_HARDLINK`) unless `--allow-hardlinks`, since a
  hard link can alias a file outside the context.
- A `.sitefileignore` (gitignore-style subset) at the context root excludes files
  from directory copies; explicitly named files are never ignored.
- Remote imports are `https:` only, to public addresses only (checked at connect
  time and on every redirect), size- and time-limited, never decompressed or executed.
- Unpinned URLs are recorded in `Sitefile.lock` and must match on later builds.
  `--strict-pins` refuses any remote import or BASE image that is not pinned
  (`INTEGRITY`, `@sha256:` or a lockfile entry), for reproducible CI builds.
- `IMPORT … FROM git+https://host/repo.git#ref:path` imports from Git without running
  `git`: protocol v2 over the same HTTPS client and policy, every object's SHA-1 checked,
  the commit recorded in `Sitefile.lock`, symlinks kept inside the tree, submodules
  refused; `sitefile build https://host/repo.git#ref:dir` builds a repository
  ([docs/LANGUAGE.md](docs/LANGUAGE.md#git-sources-githttps)).
- Private APIs and repositories: `--secret id=token,env=API_TOKEN` with
  `IMPORT … SECRET token AS BEARER` (or `AS HEADER X-Api-Key`). Secrets go only to the
  import's own origin and never into the output, caches, `Sitefile.lock`, vendor files,
  provenance, traces or messages.
- `sitefile vendor` stores every remote import (and the BASE image's blobs) under
  `vendor/sitefile/` by content digest; builds then read them from there, verified,
  with no network access ([docs/LANGUAGE.md](docs/LANGUAGE.md#vendoring-sitefile-vendor)).
- Writing different content to an existing path fails unless `--overwrite` is given.
  Two output paths that differ only in case (`/A.html`, `/a.html`) always fail
  (`E_PATH_COLLISION`), and every path is Unicode NFC, so a build gives the same
  files and image IDs on macOS, Windows and Linux.
- Export is staged in a temporary sibling and swapped in; a non-empty destination
  outside the context is only replaced with `--force`.
- Archive exports (`zip`, `tar`, `tgz`, `wbn`) write one file via a temporary sibling and
  rename; `--out` may not be the context, its parent, root or home, nor a directory or
  symlink, and an existing file outside the context needs `--force`.

## Export formats

```bash
node lib/src/cli.js build examples/hello --format zip     # examples/hello/dist.zip
node lib/src/cli.js build examples/hello -o hello.tar.gz      # format from the extension
node lib/src/cli.js build examples/hello --format wbn --base-url https://example.com/
```

Archives are reproducible: sorted entries, fixed timestamps (or `SOURCE_DATE_EPOCH`),
uid/gid 0, modes 0644/0755. See [docs/LANGUAGE.md](docs/LANGUAGE.md#export-formats).

## Rust implementation

`rust/` is a full port, maintained alongside the TypeScript version and checked
against it by `node conformance/run.mjs` (identical output on every example and
case). The Rust binary is the shipped product. TypeScript is the reference
specification and the library API, and both get every feature (see
[docs/RUST.md](docs/RUST.md#roles-decided-in-82)). It starts in ~4 ms instead of ~70 ms and is 1.3–2× faster on large
sites, with a fraction of the memory; see [docs/RUST.md](docs/RUST.md) and
[bench/README.md](bench/README.md).

```bash
(cd rust && cargo build --release) && rust/target/release/sitefile build examples/blog
```

## Editor support

Highlighting works on its own; the language features (diagnostics,
completion, hover, go to definition, references, rename, outline) come from
the Rust binary's language server, `sitefile lsp`
([docs/LSP.md](docs/LSP.md)), so put it on `PATH` with
`cargo install --path rust` or point the editor at it.

- **VS Code**: [editors/vscode/](editors/vscode/), a TextMate grammar plus a
  language client that starts `sitefile lsp` (`sitefile.server.path`). Not
  published; `(cd editors/vscode && npm install && npm run package)`, then
  `code --install-extension editors/vscode/sitefile-0.2.0.vsix`.
- **Zed**: [editors/zed/](editors/zed/), tree-sitter highlighting and the
  `sitefile-lsp` language server; **zed: install dev extension** on
  `editors/zed`.
- **Neovim**: [editors/neovim/](editors/neovim/), tree-sitter queries and a
  `vim.lsp.config` snippet for `sitefile lsp`.
- **Helix**: [editors/helix/](editors/helix/), queries and a `languages.toml`
  with the grammar and the `sitefile-lsp` server.

The grammar behind Zed, Neovim and Helix is
[editors/tree-sitter-sitefile/](editors/tree-sitter-sitefile/). Without an
extension, `"files.associations": { "Sitefile": "dockerfile" }` is a usable
stopgap in VS Code.

## Docs

- [docs/CLI.md](docs/CLI.md) — every command, the image store layout, reference grammar
- [docs/LANGUAGE.md](docs/LANGUAGE.md) — syntax and the exact semantics chosen for v0.1
- [docs/RUST.md](docs/RUST.md) — the Rust port: layout, parity checks, known differences
- [docs/LSP.md](docs/LSP.md) — the language server for editors (`sitefile lsp`, Rust binary)
- [docs/PLUGINS.md](docs/PLUGINS.md) — writing renderer plugins, and the sandbox
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — invariants and module layout
- [docs/MILESTONES.md](docs/MILESTONES.md), [docs/TASKS.md](docs/TASKS.md)
- [AGENTS.md](AGENTS.md) — rules for coding agents working on this repo

## Development

```bash
npm test        # compile with tsc, then run node:test suites
npm run typecheck
```
