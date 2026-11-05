# Sitefile language draft v0.1

Line-oriented, Dockerfile-inspired, case-insensitive instruction keywords. Quoted strings, comments, and backslash continuations. No implicit shell.

Instructions:
- `FROM scratch|<local dir>|<stage> [AS name]`
- `ARG NAME[: type] [= value]` external build parameter
- `LET NAME = value` local immutable binding
- `IMPORT name FROM <uri> [AS json|text|tree|file] [INTEGRITY sha256:<hex>]`
- `COPY [--from=<stage>] <source> <destination>`; source can be local path or `name:/path`
- `RENDER <template> TO <destination>`; interpolation `${NAME}` only in MVP
- `EXPORT <tree-path> [AS directory|zip|tar|tgz|wbn|oci] [BASE <https-url>] [AT </srv/www>]` (CLI specifies destination)
- `PLUGIN`, `TOOL` and `TRANSFORM` (custom renderers and allow-listed external programs): see Plugins below.
- Future: REMOVE, GENERATE, ASSERT, META, EXEC.

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
- **Build args from files and the environment** (CLI):
  - `--build-arg-file <path>` (repeatable) reads a flat `.json` object, a flat
    `.yaml`/`.yml` mapping, or a `.env` file (`KEY=value` lines, `export`
    prefix, `#` comments, `'literal'` or `"escaped"` quotes). Values must be
    strings, numbers or booleans. `.env` files are **parsed, never executed**:
    no `$VAR` or `$(…)` expansion.
  - `--build-arg NAME` (no `=`) takes the value from the environment variable
    `NAME`; an unset variable is a usage error.
  - Precedence: Sitefile default < files (later files win) < `--build-arg`.
    Each ARG's declared type still applies.
- `LET NAME[: type] = value`. ARG and LET share one namespace; rebinding fails.
- Operands of every instruction can use `${NAME}` for ARG/LET names defined on
  an earlier line. Dotted access is only allowed inside RENDER templates.
- `IMPORT name FROM <uri> [AS file|text|json|yaml|csv|markdown|wbn|tree] [INTEGRITY sha256:<hex>] [SORT BY key [ASC|DESC]]`
  - `AS markdown` and `SORT BY`: see Markdown and template blocks below;
    `AS yaml`, `AS csv`, `AS wbn` and directories of data files: see Data files.
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
- `RENDER [--escape=none|html] [--overwrite] [--from=<stage>] <template> TO <dest> [FOR item IN list]`
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
- `EXPORT <path> [AS directory|zip|tar|tgz|wbn|oci] [BASE <url>] [AT <root>]` must be the last
  instruction, exactly once; it exports from the last stage. The format
  defaults to `directory`; `tar.gz` is accepted as an alias for `tgz`; names
  are case-insensitive. `BASE` (an operand, so `${VAR}` works) is the URL the
  tree is mounted at and is required by `wbn` (from here or `--base-url`).
  With `AS oci`, `BASE` is instead a **container image** to build on (see *oci* below). `--base` overrides it.
  `AT` (an operand) is the directory the files occupy inside an `oci` image
  (default `/srv/www`; `--oci-root` overrides it); with any other format it is
  an error (`E_VALIDATION`).
  The CLI's `--out` picks the host destination and `--format` / `--base-url`
  override the Sitefile's `AS` / `BASE`.
- `RUN` is rejected: the MVP never runs shell commands.

### Markdown and template blocks (see `examples/blog/`)
- `IMPORT posts FROM ./posts AS markdown` reads every `.md`/`.markdown` file in
  a directory (other files are ignored) as a **list of documents**, in path
  order; a single file gives **one document**. A document is its frontmatter
  fields plus:
  - `slug`: the file name without the extension and without a leading
    `YYYY-MM-DD-`, unless the frontmatter sets `slug` (letters, digits, `.`,
    `_`, `~`, `-` only, `E_VALIDATION` otherwise)
  - `date`: that leading `YYYY-MM-DD`, unless the frontmatter sets `date`
  - `path`, `content` (the body as HTML) and `excerpt` (plain text of the
    first paragraph, about 200 characters), which frontmatter cannot override
- Frontmatter is a YAML subset between `---` lines: `key: value` scalars
  (quoted or plain strings, numbers, `true`/`false`, `null`), inline `[a, b]`
  and block `- a` lists. Dates stay strings. Anything else (nested maps,
  multi-line strings) fails with `E_FRONTMATTER` at its line.
- Markdown is a dependency-free subset: ATX/setext headings, paragraphs, hard
  breaks, fenced and indented code, blockquotes, nested lists, thematic
  breaks, emphasis, strong, `~~del~~`, code spans, links, images and
  autolinks. Not supported: tables, footnotes, reference links, task lists.
  **Raw HTML is always escaped**, and links or images with schemes other than
  `http`, `https`, `mailto`, relative or `#` lose their URL.
- `SORT BY key [ASC|DESC]` orders documents by a field: numbers and numeric
  strings numerically, everything else as strings, so ISO dates sort
  correctly (it also works on other list imports, see Data files). Documents without the field come last; ties keep path order.
- `RENDER … TO <dest> FOR item IN list` renders the template once per element
  of an imported list (markdown documents or a JSON array). The destination
  may use `${item}` / `${item.key}`; every resulting path is normalized like any
  destination, and two elements writing one path fail with `E_CONFLICT`.
  The template sees `item` plus every ARG, LET and IMPORT.
- Templates support blocks (in addition to `${name}` and `${name.key}`):
  - `${#each list as item}…${else}…${/each}` repeats for each element; the
    `${else}` part renders for an empty list. A missing key such as
    `${#each post.tags as t}` on a post without tags counts as empty.
  - `${#if value}…${else}…${/if}` and `${#if !value}`: missing, `null`,
    `false`, `''`, `0` and empty lists are false.
  - `${value|raw}` inserts a value without escaping, for `${post.content|raw}`
    under `--escape=html`. Every other value is still escaped.
  - A block tag alone on its line removes that whole line from the output.
  - Unbalanced blocks, a second `${else}` or an unknown `|filter` fail with
    `E_TEMPLATE` at the tag.

### Data files: YAML, CSV and Web Bundles (see `examples/data/`)
All of these are **values**: templates read them, `FOR` iterates over them,
and they can't be COPY'd (`E_TYPE`). Parse errors are `E_DATA` with the file's
line and column.
- `AS yaml`: a safe YAML subset: nested block mappings and sequences
  (including lists of mappings), flow `[…]` / `{…}`, block scalars `|` / `>`
  with chomping, comments, and the same scalars as frontmatter (dates stay
  strings). Not supported: anchors/aliases, tags, multiple documents, complex
  keys, merge keys. An empty document is `null`.
- `AS csv`: RFC 4180 (quoted fields, `""` escapes, embedded commas and
  newlines, CRLF, BOM). The header row gives the keys; every row is an object
  of **strings**; a row with the wrong number of fields is an error.
- `AS wbn`: a Web Bundle (`b1` or `b2`) as data:
  `{ version, primaryUrl, entries: [{ url, path, status, contentType, headers, size, content? }] }`,
  entries in URL order. `content` is the body as text for textual responses
  (text/*, JSON, XML, JavaScript, SVG) that are valid UTF-8.
- `AS tree` on a `.wbn` file unpacks the bundle into files (detected by its
  magic bytes, like tar and zip): URL pathnames become paths (`…/` →
  `…/index.html`), percent-decoded; if the bundle mixes origins each path is
  prefixed with `/<host>`. Only status-200 responses become files. Unsafe
  paths are `E_ARCHIVE_UNSAFE`; the archive limits apply.
- A **directory** imported `AS json` or `AS yaml` is a list with one entry per
  `.json` / `.yaml` / `.yml` file, in path order. An object becomes
  `{ slug, date?, path, ...fields }` (slug and date from the file name as for
  Markdown; fields can override them); any other value becomes
  `{ slug, path, value }`.
- `SORT BY` applies to any list import (markdown, json, yaml, csv); the value
  must be a list of objects. Numbers **and numeric strings** (such as CSV
  cells) compare numerically.

### Plugins: PLUGIN and TRANSFORM (see `docs/PLUGINS.md`, `examples/motion/`)
- `PLUGIN name FROM ./plugins/x.mjs|x.wasm [INTEGRITY sha256:…]` declares a renderer:
  a local `.mjs` module in its own folder (not the context root), or a
  WebAssembly module with no imports (anywhere; see docs/PLUGINS.md). URLs fail
  with `E_PROTOCOL`. Plugin names share the ARG/LET/IMPORT/stage namespace.
- `TRANSFORM [--overwrite] [--from=stage] [<source>] WITH name TO <dest>
  [OPTIONS key=value …] [FOR item IN list]` sends the source (a file, an
  inline body, or a value import) and the options to the plugin and writes
  what it returns: one file at `<dest>`, or several under `<dest>/`. Without a
  source, the plugin gets the `FOR` element, or only its options. `<dest>` and
  option values may use `${item.key}`.
- Plugins run in a separate Node process with `--permission`: read access to
  their own folder only; no writes, processes, workers, network or
  environment; a per-call time limit (`--plugin-timeout`). Failures are
  `E_PLUGIN`. This protects against accidents, not deliberately malicious code:
  pin plugins with `INTEGRITY`.

- `TOOL name` declares an external program (`name` is an identifier and the
  program's file name, e.g. `ffmpeg`). It runs only if the CLI allows it with
  `--allow-exec name[=/abs/path]`; otherwise the build fails with
  `E_UNSUPPORTED` naming the flag. Tool names share the namespace above.
- A TRANSFORM `WITH` a tool ends with `ARGS <argument> …` (the rest of the
  line; so `OPTIONS` and `FOR` come first). Each word is one argv entry
  (quote words that contain spaces); there is no shell. `${…}` interpolation
  applies as in other operands. The host replaces `{input}` (the source file,
  written to a temp dir; needs a source), `{output}` (required: the one file
  the tool must write, taking its extension from `<dest>`) and
  `{option.key}` (an OPTIONS value). Other braces (such as ffmpeg's `%{pts}`)
  are literal. A tool takes a file source (or none, with `FOR` or alone), and
  `<dest>` is a file, not a directory. Failures, timeouts and oversized
  outputs are `E_PLUGIN`. See `docs/PLUGINS.md`.
- Results are cached between builds (`docs/PLUGINS.md`, "Caching").

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

### Export formats
`--out` is a directory for `directory` and a **file** for every other format.
Default `--out`: `<context>/dist`, `dist.zip`, `dist.tar`, `dist.tar.gz`,
`dist.wbn`. The format is chosen by, in order: `--format`; the `--out`
extension (`.zip`, `.tar`, `.tar.gz`/`.tgz`, `.wbn`), so `-o site.zip` writes a
zip; then the Sitefile's `EXPORT ... AS`; then `directory`. `.oci.tar` selects `oci`.

All archive writers are deterministic: the same tree gives byte-identical
output (for the same Node/zlib version), whatever the order files were added.
Entries are sorted by path; there are no directory entries (directories are
implicit); uid/gid are 0 with no user/group names; modes are the tree's
`0644`/`0755`. Timestamps are fixed: zip uses the DOS epoch 1980-01-01 00:00,
tar uses mtime 0, and the gzip header mtime is 0 (OS byte fixed to Unix). If
the `SOURCE_DATE_EPOCH` environment variable is set (non-negative integer
seconds), it is used for the zip (clamped to 1980..2107, 2-second resolution)
and tar timestamps instead.

- **zip**: `node:zlib` raw deflate (stored when deflate does not shrink the
  entry), CRC-32, central directory, UTF-8 names (flag bit 11), unix modes in
  the external attributes (version made by = 3 << 8). No zip64: more than
  65535 entries, a file of 4 GiB or more, or an archive of 4 GiB or more fails
  with `E_EXPORT`.
- **tar**: POSIX ustar. A name over 100 bytes is split into `prefix`/`name` at
  a `/`; if it still does not fit (prefix over 155 bytes, or no usable `/`), a
  pax `x` header carries the full `path`. Files of 8 GiB or more fail with
  `E_EXPORT`.
- **tgz**: gzip of that tar.
- **oci**: an [OCI image](https://github.com/opencontainers/image-spec) (v1.1) of
  the site's files. The image has one layer: a deterministic tar of the tree
  placed under the root (default `/srv/www`, `AT`/`--oci-root`; the parent
  directory entries `srv` and `srv/www` are emitted so runtimes can unpack
  it), gzip-compressed (`application/vnd.oci.image.layer.v1.tar+gzip`). The
  config's `rootfs.diff_ids` is the sha256 of the *uncompressed* tar; the
  layer descriptor is the sha256 of the gzip. `created` is `SOURCE_DATE_EPOCH`
  or `1970-01-01T00:00:00Z`; `architecture`/`os` default to `amd64`/`linux`
  (`--platform os/arch`; metadata only). Labels: `org.opencontainers.image.title`
  (the context directory name), `dev.sitefile.root`, any `--label key=value`,
  and, only with `--git-labels`, `.source` and `.revision` from the repository
  checkout (read from its `.git` directory without running anything;
  credentials in URLs are dropped). Git labels are off by default because they
  change the image ID on every commit even when the files are identical, and
  finding the checkout reads outside the build context. Build-arg values
  are never written. The config, manifest and index are canonical JSON (sorted
  keys, no whitespace), so the same build always yields the same manifest digest.
  `--out` ending in `.tar` (`site.oci.tar`) is an *OCI archive* (a tar of the
  layout, below); any other `--out` with `--format oci` is an *OCI layout
  directory*: `oci-layout`, `index.json` (one manifest entry, annotated
  `org.opencontainers.image.ref.name` when you pass `-t`) and
  `blobs/sha256/<hex>`. **The image contains only files**: it has no
  entrypoint and is not runnable on its own. Use it as a source of files, e.g.
  `COPY --from=<image> /srv/www /usr/share/nginx/html` in a Dockerfile, **unless
  you give it a `BASE`**:

  `EXPORT / AS oci BASE docker.io/library/nginx:alpine AT /usr/share/nginx/html`
  (or `--base`) makes a *runnable* image. The base (a bare name means Docker Hub) is
  pulled for the target platform (`--platform`, default `linux/amd64`; a base of another
  platform is `E_REGISTRY`) into the local store, and the result is: the base's layers,
  then the site layer. The config is the base's (Entrypoint, Cmd, Env, ExposedPorts,
  WorkingDir, User, ... preserved) with `rootfs.diff_ids` and `history` extended, labels
  merged (`--label` and sitefile's own win over the base's), and `created` kept from the
  base unless `SOURCE_DATE_EPOCH` is set. The site layer is added on top, so it overlays
  same-named files of the base (nginx's default `index.html`) but does not delete the
  others. Docker layer media types are rewritten to their OCI equivalents (same bytes,
  same digests). The manifest and config record the base as
  `org.opencontainers.image.base.name` / `.base.digest`. An OCI archive or layout export
  contains the base blobs too, so it loads anywhere (`docker load -i site.oci.tar`);
  `-t` stores the same image in the local store (`save`, `push`, `serve` work: `serve` serves
  the site layer). Layers are held in memory while exporting an archive.

  **Reproducibility**: the output is a pure function of the files, options and the base's
  *manifest digest*. Pin the base: `BASE nginx:alpine@sha256:...` (the Sitefile is the
  record). For a tag, `Sitefile.lock` gets an `images` entry
  (`"docker.io/library/nginx:alpine": {"digest": "sha256:..."}`, the digest the tag pointed
  at, an index for multi-platform images) the first time it is resolved, and later builds fetch
  *by digest* and never re-resolve the tag. `--frozen-lockfile` fails with `E_LOCK_MISSING`
  for an unpinned, unlocked tag; `--update-lock` re-resolves it; `--no-lockfile` resolves
  the tag every build and warns. A base already in the store is used without network access.
  Pulling goes through the same registry client, limits and credentials as `sitefile pull`
  (docs/CLI.md); `--insecure-registry` applies.
- **wbn**: an **unsigned Web Bundle**, format version `b2`
  (draft-ietf-wpack-bundled-responses): top-level CBOR array
  `[magic "🌐📦", version "b2\0\0", section lengths, [index, primary?, responses], 8-byte length]`,
  with index entries `url => [offset, length]`. Checked against the WICG
  reference reader `dump-bundle` (a test runs it when it is installed).
  The CBOR is deterministic (map keys in bytewise order). Decisions:
  - Base URL: required; an absolute `https:` URL ending in `/`, with no
    credentials, query or fragment (`E_VALIDATION` otherwise). Each file
    `/a/b.css` is addressed as `<base>a/b.css` (path segments
    percent-encoded).
  - Every `index.html` is additionally addressed by its directory URL:
    `/index.html` as `<base>` and `/docs/index.html` as `<base>docs/`. Both
    index entries point at the same response.
  - Primary URL: `<base>`, in a `primary` section, if `/index.html` exists;
    otherwise there is no `primary` section.
  - Each response has `:status` `200` and `content-type` (no other headers).
    `content-type` comes from the extension: html/htm, css, js/mjs, json, svg,
    txt, xml, webmanifest (all with `; charset=utf-8`), png, jpg/jpeg, gif,
    webp, avif, ico (`image/x-icon`), wasm, woff, woff2, pdf, mp4, webm, mp3;
    anything else is `application/octet-stream`.
  - Index entries are `[variants-value "", offset, length]` where offset is
    counted from the first byte of the responses section.
  - Not signed, no integrity block, no variants. Layout follows the b2 draft as
    understood when written; validate with a Web Bundle tool before relying on
    it in a browser.

File output safety (same rules as directories, plus): `--out` that is, or
contains, the build context, the filesystem root or the home directory is
refused (`E_EXPORT_UNSAFE`); a directory or symlink at `--out` is `E_EXPORT`;
an existing file outside the context is only replaced with `--force`
(`E_EXPORT_UNSAFE`). The bytes are written to a temporary sibling
(`.<name>.sitefile-tmp-<hex>`), the parent and destination identity (dev/ino)
are re-validated, then it is renamed into place. Only the exact temp file
created is ever deleted. The archive is excluded from the build when it lies
in the context. Residual risk is the same path-based window as for
directories (see below); the single rename is atomic.

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
