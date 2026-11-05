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

Milestones **M0** (local vertical slice) and **M1** (remote inputs) plus
multi-stage builds (see `examples/multistage`) are
implemented and tested; see [docs/TASKS.md](docs/TASKS.md). Requires Node.js 22+,
no runtime dependencies.

| Instruction | Supported |
| --- | --- |
| `FROM scratch\|<dir>\|<stage> [AS name]` | yes; several FROMs make a multi-stage build, the last stage is exported |
| `ARG NAME[: type] [= value]` | yes, `string`/`number`/`boolean`, `--build-arg` |
| `LET NAME[: type] = value` | yes |
| `IMPORT name FROM <path or https URL> [AS file\|text\|json\|tree] [INTEGRITY sha256:…]` | yes (remote trees: M2) |
| `COPY [--overwrite] [--from=<stage>] <source> <dest>` | yes; source is a local path, `name:/path`, or with `--from` an absolute path in an earlier stage |
| `RENDER [--escape=html] [--overwrite] [--from=<stage>] <template> TO <dest>` | yes; `${NAME}`, `${json.path}` |
| `EXPORT <path> [AS directory]` | yes |

## CLI

```
sitefile build [context] [-o dist] [--build-arg K=V] [--force]
               [--frozen-lockfile | --update-lock] [--allow-http] [--allow-private-network]
sitefile validate [context] [--json]
sitefile inspect  [context] [--json]
```

## Safety defaults

- Local reads stay inside the build context (no `..`, absolute paths or symlinks out).
- Remote imports are `https:` only, to public addresses only (checked at connect
  time and on every redirect), size- and time-limited, never decompressed or executed.
- Unpinned URLs are recorded in `Sitefile.lock` and must match on later builds.
- Writing different content to an existing path fails unless `--overwrite` is given.
- Export is staged in a temporary sibling and swapped in; a non-empty destination
  outside the context is only replaced with `--force`.

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
