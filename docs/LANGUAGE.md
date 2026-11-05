# Sitefile language draft v0.1

Line-oriented, Dockerfile-inspired, case-insensitive instruction keywords. Quoted strings, comments, and backslash continuations. No implicit shell.

Instructions:
- `FROM scratch|<local dir>|<stage> [AS name]`
- `ARG NAME[: type] [= value]` external build parameter
- `LET NAME = value` local immutable binding
- `IMPORT name FROM <uri> [AS json|text|tree|file] [INTEGRITY sha256:<hex>]`
- `COPY [--from=<stage>] <source> <destination>`; source can be local path or `name:/path`
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

Design decisions still open: typed expression grammar, override syntax, template engine extension API, lockfile format.

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
- `FROM scratch|<base> [AS name]` starts a **stage**. Every instruction except
  `ARG` must come after the first `FROM`; each `FROM` after that starts a new
  stage, and COPY/RENDER apply to the most recent one.
  - `scratch` (bare word; `"scratch"` is a path) is an empty tree.
  - `<base>` that is a bare identifier naming an *earlier* stage starts from a
    copy of that stage's final tree (Docker-like). Anything else is a
    context-relative directory, with COPY's sandbox rules (`E_PATH_ESCAPE`,
    symlinks inside the context only, `.git`/export dir skipped); a file is
    `E_TYPE`, a missing path `E_NOT_FOUND`. A stage name wins over a same-named
    directory; write `./name` to force the directory. Files seeded from a
    directory get the origin `Sitefile:<line> FROM`; files inherited from a
    stage keep their original origin.
  - Stage names are identifiers, unique, and share no namespace with
    ARG/LET/IMPORT: a collision in either order is `E_DUPLICATE`. A name used
    before its stage is complete (itself or a later stage) is `E_UNDEFINED`, so
    there are no cycles.
  - **Scope:** stages scope only the tree. ARG, LET and IMPORT stay one global
    namespace in source order, wherever they appear, and an IMPORT is usable
    from any later stage.
  - **EXPORT** is still exactly one and last, and exports from the **last
    stage**. Earlier stages exist only to feed later ones.
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
  - `AS tree` on a **file** (local or remote) means "extract this archive";
    archives are never extracted implicitly. See Archives below. A local
    directory with `AS tree` imports the directory as before.
  - Import names must not equal an ARG/LET name or another import.
- `COPY [--overwrite] [--from=<stage>] <source> <dest>`
  - With `--from=<stage>`, `<source>` is an absolute path (starts with `/`, no
    `..`) inside the final tree of an earlier, completed stage: `/out/` or
    `/a.txt`. File, directory, trailing-slash and conflict rules below apply
    unchanged. A missing path is `E_NOT_FOUND`; an unknown, current or later
    stage is `E_UNDEFINED` (at the flag).
  - Otherwise `<source>` is a context-relative path, or `name:` / `name:/inner/path` for
    an import. Write `./foo:bar` for a local file whose name contains `:`.
    A URL source fails: use IMPORT then COPY.
  - `<dest>` must be absolute.
  - **File source:** if `<dest>` ends with `/`, or already exists as a directory
    in the tree, the file goes to `<dest>/<basename>`; otherwise `<dest>` is the
    new file's path.
  - **Directory source:** its *contents* are placed under `<dest>`. A trailing
    slash on either side makes no difference.
- `RENDER [--escape=none|html] [--overwrite] [--from=<stage>] <template> TO <dest>`
  - `<template>` is a source like COPY's (including `--from`, so a template can be produced by an earlier stage) and must be a UTF-8 file. `<dest>` is
    a file path (no trailing slash).
  - Templates use `${NAME}` for ARG/LET, `${name}` for a text import, and
    `${name.key.0.key}` for a scalar in a JSON import. `$${` writes a literal
    `${`. Unknown names fail with the template's line and column.
  - `--escape=html` escapes `& < > " '` in every substituted value.
- **Inline content (heredocs).** The source of `COPY` and the template of
  `RENDER` can be written in the Sitefile itself:

  ```
  RENDER --escape=html <<HTML TO /index.html
  <h1>${TITLE}</h1>
  HTML
  COPY <<CSS /style.css
  h1 { color: teal }
  CSS
  ```

  - `<<NAME` (an identifier) must start a word outside quotes, in place of the
    source operand. The body is every following line up to a line that is
    exactly `NAME`; each body line keeps its `\n`. `<<-NAME` strips leading
    tabs from body lines and the closing line.
  - The body is raw: no comments, blank-line skipping, continuations or
    escapes. A missing closing line, a second `<<` on one instruction, or `<<`
    anywhere but the source fails with `E_SYNTAX`; so does combining it with
    `--from`.
  - `RENDER` bodies are templates with the same rules as template files, and
    errors report the Sitefile line and column. `COPY` bodies are written byte
    for byte (no `${}` substitution) with mode `0644`, to a file destination
    (`E_PATH` for a directory). Conflict rules and `--overwrite` apply as usual.
  - Only `COPY` and `RENDER` lines are scanned for `<<`; elsewhere it is plain text.
  - See `examples/inline/`.
- `EXPORT <path> [AS directory]` must be the last instruction, exactly once;
  it exports from the last stage.
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
  the context) are always skipped.
- Regular files with more than one hard link are refused with `E_HARDLINK`:
  `realpath` cannot tell whether a hard link aliases a file outside the
  context. `--allow-hardlinks` (`allowHardlinks`) permits them; by passing it
  you assert that every hard-linked file in the context is safe to publish.
  pnpm hard-links packages from its store into `node_modules`, so a context
  that includes a pnpm `node_modules` needs either `.sitefileignore` to exclude
  it (preferred) or `--allow-hardlinks`. The check applies to files that are
  actually read (explicit sources and the non-ignored files of a directory
  walk), so ignored hard links never trigger it.
- File modes are reduced to `0644` or `0755`.

#### `.sitefileignore`
An optional file at the context root filters directory walks (`COPY ./dir`,
`IMPORT ./dir`, ...). It is never applied to a source named explicitly
(`COPY ./drafts/a.txt /a.txt` works even if `drafts/` is listed). Syntax is a
gitignore subset:
- Blank lines and `#` comments are skipped; `\#` and `\!` match literal
  leading characters.
- `!pattern` re-includes; the last matching rule wins.
- A trailing `/` matches directories only.
- A leading `/` or a `/` in the middle anchors the pattern to the context root;
  otherwise it matches at any depth.
- `*` and `?` match within one path segment; a `**` segment matches any number
  of directories. Character classes (`[abc]`) are not supported.
- As in gitignore, a file cannot be re-included when a parent directory is
  excluded: excluded directories are not descended into.
- Rules do not affect `.git` and the export directory, which are always
  skipped. `.sitefileignore`, `Sitefile` and `Sitefile.lock` are not
  auto-ignored (explicit is better); add them to the file to keep them out of
  your output. A `.sitefileignore` that is a symlink must stay inside the
  context; a dangling one fails the build (`E_NOT_FOUND`) rather than being
  skipped, since skipping it could publish files it was meant to exclude.

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
  for https in addition to Node's default store (bundled roots plus
  `NODE_EXTRA_CA_CERTS` / `--use-system-ca`), for tests and enterprise private
  CAs. Certificate and hostname verification are never disabled; there is no
  CLI flag for it.

### Archives (`IMPORT … AS tree` on a file)
- Formats are detected from magic bytes, not names: tar (ustar or v7), tar.gz /
  tgz (gzip), and zip (stored and deflate; no zip64, no encryption). Anything
  else fails with `E_TYPE`. Supported tar extras: POSIX ustar prefix (old GNU
  headers keep timestamps there and are read without it), GNU long names, pax
  `path`/`size` (a malformed record is `E_ARCHIVE`). Other pax keys are ignored.
  Zip directories are recognized by a trailing `/` or by a directory mode.
  Decompression is asynchronous, so other imports keep making progress.
- `INTEGRITY` and the lockfile apply to the archive bytes exactly as for file
  imports, and the import's provenance digest is the archive's digest.
- Directories are implicit. Modes are reduced to `0644`/`0755`. A single
  top-level directory is **not** stripped: `COPY name:/top/ /` does that.
- Unsafe entries are rejected, never skipped: absolute paths (including `C:`),
  `..` segments, backslashes, NUL (`E_ARCHIVE_UNSAFE`), and symlink, hard link,
  device and fifo entries (`E_ARCHIVE_UNSAFE`). Duplicate paths, or a file and
  directory at one path, fail with `E_CONFLICT`. Malformed or truncated
  archives fail with `E_ARCHIVE`.
- Limits (`E_ARCHIVE_LIMIT`), configurable through `BuildOptions.archiveLimits`:
  `maxEntries` 10 000, `maxTotalBytes` 500 MiB of uncompressed content, and
  `maxRatio` 100 (uncompressed:compressed, measured on the gzip stream or the
  whole zip; not enforced below 1 MiB of output, since tar padding compresses
  extremely well). Inflation is bounded with zlib's `maxOutputLength`, so a
  bomb is stopped at the limit, never fully inflated; zip entries are never
  inflated beyond their declared size. The download cap still applies first.

### Export
- Output is written to a temporary sibling directory, then swapped in.
- An existing non-empty destination is replaced only when it is inside the
  build context, or with `--force`. A destination that is, or contains, the
  context is always refused.
- Race hardening: the exporter records the dev/ino of the destination's parent
  directory and of the existing destination, creates its temp and backup
  siblings only in that verified parent, and re-checks everything immediately
  before the swap (parent real path and identity unchanged, destination still
  the same real directory and not a symlink, or still absent). Any change
  aborts with `E_EXPORT_UNSAFE`. After the swap it confirms the new
  destination is the directory it wrote, and it deletes the old directory
  only if its dev/ino matches the one it validated.
- Residual risk: Node has no `openat`/`renameat2`, so checks are path-based
  and cannot be bound to a directory handle. The two renames (old aside, new
  into place) are not atomic together, so the destination briefly does not
  exist. An attacker with write access to the parent can still swap a path
  between a check and the next rename; this is detected afterwards (the
  previous output is preserved at the backup path and nothing unvalidated is
  deleted), not prevented. Renaming onto an originally absent destination
  that an attacker just created as an empty directory succeeds on POSIX. If
  the parent directory itself is moved away mid-export, the temp directory
  moves with it and is left behind. Do not export into directories writable
  by untrusted users.
