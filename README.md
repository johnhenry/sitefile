# Sitefile

Docker-inspired static artifact builder. Compose immutable filesystem trees from
local and remote sources, render templates, and export a deployable directory —
no container runtime, no shell.

```
# examples/hello/Sitefile
FROM scratch
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
| `FROM scratch\|<dir>\|<stage> [AS name]` | yes; several FROMs make a multi-stage build, the last stage is exported |
| `ARG NAME[: type] [= value]` | yes, `string`/`number`/`boolean`, `--build-arg` |
| `LET NAME[: type] = value` | yes |
| `IMPORT name FROM <path or https URL> [AS file\|text\|json\|yaml\|csv\|markdown\|wbn\|tree] [INTEGRITY sha256:…] [SORT BY key [DESC]]` | yes; `AS tree` unpacks tar, tar.gz, zip and Web Bundles; `AS markdown` reads posts with frontmatter; `AS yaml`/`csv`/`wbn` give data for templates |
| `COPY [--overwrite] [--from=<stage>] <source> <dest>` | yes; source is a local path, `name:/path`, `<<EOF` inline content, or with `--from` an absolute path in an earlier stage |
| `RENDER [--escape=html] [--overwrite] [--from=<stage>] <template> TO <dest> [FOR item IN list]` | yes; template is a file or `<<EOF` inline body; `${NAME}`, `${json.path}`, `${#each}`, `${#if}`, `${x\|raw}`; `FOR` renders one page per element |
| `PLUGIN name FROM ./plugins/x.mjs [INTEGRITY sha256:…]` | yes; a custom renderer, run sandboxed (see [docs/PLUGINS.md](docs/PLUGINS.md)) |
| `TRANSFORM [<source>] WITH name TO <dest> [OPTIONS k=v …] [FOR item IN list]` | yes; e.g. text → animated SVG (`examples/motion/`) |
| `TOOL name` + `TRANSFORM … WITH name TO <dest> [OPTIONS …] [FOR …] ARGS …` | yes; run an external program (e.g. `ffmpeg`) you allow with `--allow-exec name`: argv only, no shell, temp files, scrubbed environment (see [docs/PLUGINS.md](docs/PLUGINS.md)) |
| `EXPORT <path> [AS directory\|zip\|tar\|tgz\|wbn\|oci] [BASE <https-url>] [AT <root>]` | yes; archives are deterministic, `wbn` is an unsigned Web Bundle (needs `BASE` or `--base-url`), `oci` is a single-layer OCI image of the files (under `AT`, default `/srv/www`) |

Templates can also live in the Sitefile itself (see `examples/inline/`):

```
RENDER --escape=html <<HTML TO /index.html
<h1>${TITLE}</h1>
HTML
```

A blog from Markdown posts with frontmatter (see `examples/blog/`):

```
IMPORT posts FROM ./posts AS markdown SORT BY date DESC
RENDER --escape=html ./templates/index.html TO /index.html
RENDER --escape=html ./templates/post.html TO /posts/${post.slug}/index.html FOR post IN posts
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

External programs are explicit opt-ins: declare a `TOOL`, and allow it on the command line.

```
TOOL ffmpeg
TRANSFORM WITH ffmpeg TO /intro.mp4 ARGS -f lavfi -i "color=c=black:s=1280x720:d=3" -fflags +bitexact -map_metadata -1 {output}
```

```
sitefile build --allow-exec ffmpeg
```

TRANSFORM results are cached in `$SITEFILE_HOME/cache/transform` (`--no-cache` to skip; `sitefile cache info|prune`).

## CLI

```
sitefile build [context] [-o dist] [--format directory|zip|tar|tgz|wbn] [--base-url URL]
               [--build-arg K=V | --build-arg K] [--build-arg-file vars.json|.yaml|.env] [--force]
               [--frozen-lockfile | --update-lock] [--allow-http] [--allow-private-network]
               [--allow-hardlinks] [--allow-exec name[=/abs/path]]... [--no-cache]
sitefile validate [context] [--json]
sitefile inspect  [context] [--json]

# images and servers (see below and docs/CLI.md)
sitefile build [context] -t name[:tag]        # also store the build as an image
sitefile images | image inspect|diff|prune | tag | rmi | save
sitefile cache info | cache prune [--older-than days]   # TRANSFORM result cache
sitefile serve [-d] [--name n] [-p [host:]port] [--spa] <image|./dir>
sitefile servers [-a] | server stop|rm [-f]|restart|logs [-f] <name>
```

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
correct MIME types and no directory listings. `EXPORT / AS oci` (or `build -o site.oci.tar`) writes the
same OCI image without the store; it contains files only and is not runnable by itself. See
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
- Writing different content to an existing path fails unless `--overwrite` is given.
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

## Docs

- [docs/CLI.md](docs/CLI.md) — every command, the image store layout, reference grammar
- [docs/LANGUAGE.md](docs/LANGUAGE.md) — syntax and the exact semantics chosen for v0.1
- [docs/PLUGINS.md](docs/PLUGINS.md) — writing renderer plugins, and the sandbox
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — invariants and module layout
- [docs/MILESTONES.md](docs/MILESTONES.md), [docs/TASKS.md](docs/TASKS.md)
- [AGENTS.md](AGENTS.md) — rules for coding agents working on this repo

## Development

```bash
npm test        # compile with tsc, then run node:test suites
npm run typecheck
```
