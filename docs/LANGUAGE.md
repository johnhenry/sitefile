# Sitefile language draft v0.1

Line-oriented, Dockerfile-inspired, case-insensitive instruction keywords. Quoted strings, comments, and backslash continuations. No implicit shell.

Instructions:
- `FROM scratch [AS name]` or `FROM <tree-uri> [AS name]`
- `ARG NAME[: type] [= value]` external build parameter
- `LET NAME = value` local immutable binding
- `IMPORT name FROM <uri> [AS json|text|tree|file] [INTEGRITY sha256:<hex>]`
- `COPY <source> <destination>`; source can be local path or `name:/path`
- `RENDER <template> TO <destination>`; interpolation `${NAME}` only in MVP
- `EXPORT <tree-path> AS directory` (CLI specifies destination)
- Future: REMOVE, TRANSFORM, GENERATE, ASSERT, META, EXEC, OCI output.

Path rules: absolute destinations are virtual artifact paths; relative local sources resolve from project context. No implicit archive extraction. No implicit remote COPY in MVP (use IMPORT then COPY). Distinguish file and directory copying; specify trailing slash semantics with tests.

Variables: `ARG` is externally configurable; `LET` is local. Missing required values are errors. Escaping is explicit; initial renderer is for text, not automatically HTML-safe.

Example:
```
FROM scratch
ARG TITLE = "Hello"
COPY ./public/ /
RENDER ./templates/index.html TO /index.html
EXPORT / AS directory
```

Design decisions still open: typed expression grammar, override syntax, remote tree import behavior, template engine extension API, lockfile format.

---

## v0.1 implementation decisions

These pin down what the draft above leaves open. Tests cover each rule.

### Lexical rules
- One instruction per logical line. A physical line ending in an odd number of
  `\` continues onto the next line; the pieces are joined with one space.
  Whitespace around the join is trimmed, even inside quotes. Blank lines inside
  a continuation are skipped; a `\` on the last line just ends the instruction.
- A line whose first non-blank character is `#` is a comment, also inside a
  continuation. `#` elsewhere is ordinary text (URLs contain `#`).
- Flags (`--overwrite`, `--escape=…`) come before operands. Unknown or repeated
  flags are syntax errors.
- Value words (ARG/LET types, IMPORT `AS` kinds, `--escape` values,
  `AS directory`) are case-insensitive. Only the bare word `scratch` (not
  `"scratch"`) means an empty base.
- `INTEGRITY` must be a literal lowercase `sha256:<64 hex>`.
- Keywords (`FROM`, `AS`, `TO`, `INTEGRITY`, …) are case-insensitive. Names are
  case-sensitive identifiers: `[A-Za-z_][A-Za-z0-9_]*`.
- An operand is a bare word (up to whitespace), a `"double-quoted"` string, or a
  `'single-quoted'` string.
  - Bare and double-quoted text interpolate `${NAME}`; `\` escapes the next
    character (`\$`, `\"`, `\\`; in double quotes also `\n`, `\t`).
  - Single-quoted text is literal: no escapes, no interpolation.
  - A `$` not followed by `{` is literal.

### Instructions
- `FROM scratch [AS name]` must come before every instruction except `ARG`, and
  exactly once. Other bases and multi-stage builds are M2 (`E_UNSUPPORTED`).
- `ARG NAME[: string|number|boolean] [= value]`. No default means required;
  `--build-arg NAME=value` overrides. Unknown `--build-arg` names are warnings.
- `LET NAME[: type] = value`. ARG and LET share one namespace; rebinding fails.
- Operands of every instruction can use `${NAME}` for ARG/LET names defined on
  an earlier line. Dotted access is only allowed inside RENDER templates.
- `IMPORT name FROM <uri> [AS file|text|json|tree] [INTEGRITY sha256:<hex>]`
  - `<uri>` is a context-relative path or an `https://` URL (`http://` only with
    `--allow-http`). Other schemes fail with `E_PROTOCOL`.
  - Default kind: `tree` for a local directory, otherwise `file`.
  - `text` must be valid UTF-8; `json` must parse. Both produce **values**:
    they can be read by RENDER templates but not copied (`E_TYPE`).
  - Remote `tree` imports are M2.
  - Import names must not equal an ARG/LET name or another import.
- `COPY [--overwrite] <source> <dest>`
  - `<source>` is a context-relative path, or `name:` / `name:/inner/path` for
    an import. Write `./foo:bar` for a local file whose name contains `:`.
    A URL source fails: use IMPORT then COPY.
  - `<dest>` must be absolute.
  - **File source:** if `<dest>` ends with `/`, or already exists as a directory
    in the tree, the file goes to `<dest>/<basename>`; otherwise `<dest>` is the
    new file's path.
  - **Directory source:** its *contents* are placed under `<dest>`. A trailing
    slash on either side makes no difference.
- `RENDER [--escape=none|html] [--overwrite] <template> TO <dest>`
  - `<template>` is a source like COPY's and must be a UTF-8 file. `<dest>` is
    a file path (no trailing slash).
  - Templates use `${NAME}` for ARG/LET, `${name}` for a text import, and
    `${name.key.0.key}` for a scalar in a JSON import. `$${` writes a literal
    `${`. Unknown names fail with the template's line and column.
  - `--escape=html` escapes `& < > " '` in every substituted value.
- `EXPORT <path> [AS directory]` must be the last instruction, exactly once.
  The CLI's `--out` picks the host destination.
- `RUN` is rejected: the MVP never runs shell commands.

### Writes and conflicts
- Writing a path that already holds identical bytes and mode is a no-op that
  keeps the first writer as the owner.
- Writing different content fails with `E_CONFLICT` naming the first owner,
  unless the instruction has `--overwrite`.
- A file can never replace a directory or the reverse.

### Local sources
- Absolute paths and `..` that escapes the context fail with `E_PATH_ESCAPE`.
- Symlinks are followed only when their real target stays inside the context;
  otherwise `E_PATH_ESCAPE`. Directory symlink cycles are skipped.
- When copying directories, `.git` and the export directory (if it is inside
  the context) are skipped.
- File modes are reduced to `0644` or `0755`.

### Remote sources and the lockfile
- Only `https:` by default. Credentials in URLs are rejected. Every resolved
  address must be public unless `--allow-private-network` (NAT64 addresses are
  judged by the IPv4 address they embed); the check runs at
  connect time, so DNS rebinding cannot slip past it. Redirects are followed
  manually (max 5) and every hop is re-checked.
- Bodies are capped (`--max-download-bytes`, default 50 MiB), requested with
  `accept-encoding: identity`, and never decompressed or executed.
- `Sitefile.lock` (JSON, next to the Sitefile) records each remote URL's
  sha256 and size. An unpinned URL already in the lockfile must match it
  (`E_LOCK_MISMATCH`; rerun with `--update-lock`). `--frozen-lockfile` fails
  instead of adding entries (`E_LOCK_MISSING`).
- Library option `fetchPolicy.ca` (PEM string or Buffer) trusts an extra CA
  for https in addition to the built-in roots, for tests and enterprise private
  CAs. Certificate and hostname verification are never disabled; there is no
  CLI flag for it.

### Export
- Output is written to a temporary sibling directory, then swapped in.
- An existing non-empty destination is replaced only when it is inside the
  build context, or with `--force`. A destination that is, or contains, the
  context is always refused.
