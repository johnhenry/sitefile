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
| `IMPORT name FROM <path or https URL> [AS file\|text\|json\|tree\|markdown] [INTEGRITY sha256:…] [SORT BY key [DESC]]` | yes (remote trees: tar, tar.gz, zip archives with `AS tree`; `AS markdown` reads posts with frontmatter) |
| `COPY [--overwrite] [--from=<stage>] <source> <dest>` | yes; source is a local path, `name:/path`, `<<EOF` inline content, or with `--from` an absolute path in an earlier stage |
| `RENDER [--escape=html] [--overwrite] [--from=<stage>] <template> TO <dest> [FOR item IN list]` | yes; template is a file or `<<EOF` inline body; `${NAME}`, `${json.path}`, `${#each}`, `${#if}`, `${x\|raw}`; `FOR` renders one page per element |
| `EXPORT <path> [AS directory\|zip\|tar\|tgz\|wbn] [BASE <https-url>]` | yes; archives are deterministic, `wbn` is an unsigned Web Bundle (needs `BASE` or `--base-url`) |

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

## CLI

```
sitefile build [context] [-o dist] [--format directory|zip|tar|tgz|wbn] [--base-url URL]
               [--build-arg K=V] [--force]
               [--frozen-lockfile | --update-lock] [--allow-http] [--allow-private-network]
               [--allow-hardlinks]
sitefile validate [context] [--json]
sitefile inspect  [context] [--json]
```

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

- [docs/LANGUAGE.md](docs/LANGUAGE.md) — syntax and the exact semantics chosen for v0.1
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — invariants and module layout
- [docs/MILESTONES.md](docs/MILESTONES.md), [docs/TASKS.md](docs/TASKS.md)
- [AGENTS.md](AGENTS.md) — rules for coding agents working on this repo

## Development

```bash
npm test        # compile with tsc, then run node:test suites
npm run typecheck
```
