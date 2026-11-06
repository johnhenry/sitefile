# Sitefile language draft v0.1

Line-oriented, Dockerfile-inspired, case-insensitive instruction keywords. Quoted strings, comments, and backslash continuations. No implicit shell.

Instructions:
- `FROM scratch|<local dir>|<stage> [AS name]` (optional for a single-stage build: the first instruction other than `ARG` starts an implicit `FROM scratch`)
- `STAGE <name> [FROM scratch|<local dir>|<stage>]` starts a named stage; same as `FROM <base> AS <name>` (base defaults to `scratch`)
- `ARG NAME[: type] [= value]` external build parameter
- `LET NAME = value` local immutable binding
- `IMPORT name FROM <uri> [AS json|text|tree|file] [INTEGRITY sha256:<hex>]`
- `COPY [--from=<stage>] <source> <destination>`; source can be local path or `name:/path`
- `RENDER <template> TO <destination>`; interpolation `${NAME}` only in MVP
- `EXPORT <tree-path> [AS directory|zip|tar|tgz|wbn|oci] [BASE <https-url>] [AT </srv/www>] [COMPRESSION gzip|none]` (CLI specifies destination)
- `PLUGIN`, `TOOL` and `TRANSFORM` (custom renderers and allow-listed external programs): see Plugins below.
- `SITEMAP`, `FEED`, `REDIRECT` and `HEADER` (sitemap.xml, an Atom feed, `_redirects`, `_headers`): see Site files below.
- Future: REMOVE, GENERATE, ASSERT, META, EXEC.

Path rules: absolute destinations are virtual artifact paths; relative local sources resolve from project context. No implicit archive extraction. No implicit remote COPY in MVP (use IMPORT then COPY). Distinguish file and directory copying; specify trailing slash semantics with tests.

Variables: `ARG` is externally configurable; `LET` is local. Missing required values are errors. Escaping is explicit; initial renderer is for text, not automatically HTML-safe.

Example:
```
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
- `FROM scratch|<base> [AS name]` starts a **stage**. Each `FROM` starts a new
  stage, and COPY/RENDER apply to the most recent one.
  - **Implicit `FROM scratch`.** If the first instruction other than `ARG`
    comes before any `FROM`, an unnamed `FROM scratch` stage starts there, so a
    single-stage Sitefile needs no `FROM` at all (`ARG`s before it are global
    build parameters, exactly as before an explicit `FROM scratch`). The
    implicit stage has no source line; messages about it use that first
    instruction's line. A file with only `ARG`s and no `EXPORT` is
    `missing EXPORT`. Write `FROM` for multi-stage builds and for `<dir>` or
    `<stage>` bases; an explicit `FROM scratch` behaves as it always did.
  - **Unnamed non-final stages warn.** A stage without `AS name` can be
    neither a `FROM` base nor a `COPY --from` source, and only the last stage
    is exported, so its output can never be used. The build prints
    `warning: <file>:<line>: unnamed stage is not the last one, so its output can never be used; name it with FROM … AS <name>`
    (stderr, from `build`, `validate` and `inspect`; the build still succeeds).
  - **`STAGE name [FROM base]`** is a more readable way to start a named
    stage (like an Earthly or Make target). It is exactly `FROM base AS name`,
    and `STAGE name` alone is `FROM scratch AS name`. The name is required and
    follows the stage-name rules below; `<base>` is anything `FROM` accepts
    (`scratch`, an earlier stage, a context directory). `STAGE` and `FROM` mix
    freely, and a `STAGE` counts as a stage start for the implicit-`FROM` rule.
    A bare `FROM` with no base is still a syntax error. Errors about a `STAGE`
    line say `STAGE`, e.g. `STAGE b FROM a: stage 'a' is not defined before this line`.

    ```
    STAGE assets
    COPY ./static/ /static/

    STAGE site FROM assets
    RENDER ./index.html TO /index.html

    FROM scratch
    COPY --from=site / /
    EXPORT /
    ```
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
  - `<uri>` is a context-relative path, an `https://` URL (`http://` only with
    `--allow-http`), or a Git URL `git+https://host/repo.git#ref:path` (see
    Git sources below). Other schemes fail with `E_PROTOCOL`.
  - Default kind: `tree` for a local directory (or a directory in a Git
    repository), otherwise `file`.
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
  - Only `COPY`, `RENDER` and `TRANSFORM` lines are scanned for `<<`
    (`TRANSFORM <<NAME WITH …` passes the body to the plugin as its source,
    byte for byte); elsewhere it is plain text.
  - See `examples/inline/`.
- `EXPORT <path> [AS directory|zip|tar|tgz|wbn|oci] [BASE <url>] [AT <root>] [COMPRESSION gzip|none]` must be the last
  instruction, exactly once; it exports from the last stage (`--target <stage>` exports a named stage's whole
  tree instead; see [CLI.md](CLI.md#build-target---target-stage)). The format
  defaults to `directory`; `tar.gz` is accepted as an alias for `tgz`; names
  are case-insensitive. `BASE` (an operand, so `${VAR}` works) is the URL the
  tree is mounted at and is required by `wbn` (from here or `--base-url`).
  With `AS oci`, `BASE` is instead a **container image** to build on (see *oci* below). `--base` overrides it.
  `AT` (an operand) is the directory the files occupy inside an `oci` image
  (default `/srv/www`; `--oci-root` overrides it); with any other format it is
  an error (`E_VALIDATION`).
  `COMPRESSION gzip|none` (a word, case-insensitive; `oci` only, otherwise
  `E_VALIDATION`) chooses how the site layer is stored: `gzip` (the default,
  `tar+gzip`) or `none` (a plain `tar` layer). Any other value is `E_SYNTAX`.
  `--oci-compression` overrides it. The clauses come in this order:
  `AS`, `BASE`, `AT`, `COMPRESSION`.
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
- A JavaScript plugin runs only if the CLI allows it with `--allow-plugin name`
  (or `--allow-plugins`); otherwise the build fails with `E_UNSUPPORTED` at the
  `PLUGIN` line, naming the flag. A WebAssembly plugin needs no flag.

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

### Site files: SITEMAP, FEED, REDIRECT and HEADER (see `examples/blog/`)
Common files a static host reads, generated from the tree and the imports.
Every one takes `--overwrite` (see Writes and conflicts) and operands that may
use `${NAME}`.

- `SITEMAP [--overwrite] BASE <url> [TO <dest>]` writes a
  [sitemaps.org](https://www.sitemaps.org/protocol.html) file (default
  `/sitemap.xml`) listing every `.html` file in the **current stage's tree at
  that point** (so put it after the pages are written), except `/404.html`,
  sorted by path (UTF-8 bytes). `/index.html` is listed as `<base>/`,
  `/a/index.html` as `<base>/a/`, anything else as `<base><path>`; each path
  segment is percent-encoded like `encodeURIComponent`, and the result is XML
  escaped. There is no `lastmod` (the output depends only on the tree).
- `BASE` (SITEMAP and FEED) must be an absolute `http://` or `https://` URL
  with a host and no credentials, query, fragment, whitespace or control
  characters (`E_VALIDATION` otherwise); trailing slashes are dropped, so
  `https://example.com/blog/` and `https://example.com/blog` are the same.
- `FEED [--overwrite] <import> BASE <url> LINK <path> [TO <dest>] [TITLE <text>]`
  (clauses in this order) writes an [Atom 1.0](https://www.rfc-editor.org/rfc/rfc4287)
  feed (default `/feed.xml`) with one entry per element of a list import,
  **in list order** (use `SORT BY date DESC`). `<import>` must be an IMPORT on
  an earlier line (`E_UNDEFINED`) whose value is a list of objects (`E_TYPE`),
  usually `AS markdown`.
  - Each element needs `title` and `date` (text, numbers or booleans):
    `FEED posts: document '/a.md' has no 'date' (add it to its frontmatter)`
    (`E_VALIDATION`). A Markdown document's `date` comes from its file name when
    the frontmatter has none. `date` is `YYYY-MM-DD` (midnight UTC) or an
    RFC 3339 date-time (`2026-01-05T10:30:00Z`, `…+02:00`, a space or `T`
    separator, optional seconds and fraction); anything else, or an impossible
    date, is `E_VALIDATION`.
  - `LINK` is evaluated per element with `${item}` / `${item.key}` (only LINK
    may use `item`; it shadows nothing else). The result must be an absolute
    artifact path (`E_PATH` otherwise) and becomes `<base><path>` like a
    sitemap entry, so `/posts/${item.slug}/index.html` links to `…/posts/x/`.
  - The feed's `id` is its own URL (`<base><dest>`), with `<link href="<base>/">`
    and a `rel="self"` link; `updated` is the newest entry date (compared as
    instants, so time zones are respected; ties keep the first), or
    `1970-01-01T00:00:00Z` for an empty list (which also warns). Each entry
    has `title`, `link` and `id` (the entry URL), `updated` (the date, with
    `T00:00:00Z` added to a plain date), `author` when the element has a
    non-empty `author`, `summary` from a non-empty `excerpt`, and
    `content type="html"` from `content` (the rendered Markdown). `TITLE`
    defaults to the BASE URL. Characters XML 1.0 forbids become U+FFFD.
- `REDIRECT [--overwrite] <from> <to> [STATUS 301|302|303|307|308]` and
  `HEADER [--overwrite] <path-pattern> <Name>: <value …>` accumulate, **in
  source order and from every stage**, into `/_redirects` and `/_headers` in
  the [Netlify](https://docs.netlify.com/routing/redirects/) /
  [Cloudflare Pages](https://developers.cloudflare.com/pages/configuration/redirects/)
  formats. They are written **at EXPORT**, at the root of the exported
  directory (`EXPORT /www` gets `/www/_redirects`), after every stage has
  run.
  - `/_redirects` has one `from to status` line per REDIRECT (the status is
    always written; the default is 301). `from` must be an absolute path and
    `to` an absolute path or an `http(s)://host…` URL; neither may contain
    spaces or control characters (`E_VALIDATION`). Host syntax such as
    `/blog/*` and `:splat` is passed through as written; `sitefile serve`
    understands exact paths and a trailing `/*` (docs/CLI.md).
  - `HEADER /assets/* Cache-Control: public, max-age=3600`: the name is the
    word ending in `:` (`E_SYNTAX` if there is none), the value is the rest of
    the line, its words joined with one space and trimmed. The name must be an
    HTTP token (`E_VALIDATION`), and the value must be non-empty and contain no
    control characters other than tab, so CR/LF cannot inject headers. The
    pattern is an absolute path like `from`. `/_headers` has one block per
    pattern, in order of first use: the pattern, then `  Name: value` lines in
    source order.
  - If the tree already has `/_redirects` or `/_headers` (from a COPY, say),
    the normal conflict rules apply: identical bytes are fine, different
    content is `E_CONFLICT` naming that COPY, unless one of the REDIRECT (or
    HEADER) lines has `--overwrite`, which replaces the file.

### Writes and conflicts
- Writing a path that already holds identical bytes and mode is a no-op that
  keeps the first writer as the owner.
- Writing different content fails with `E_CONFLICT` naming the first owner,
  unless the instruction has `--overwrite`.
- A file can never replace a directory or the reverse.
- Two paths that differ only in case fail with `E_PATH_COLLISION` at the
  write that creates the second one, with that instruction's line and column
  and both spellings: `Sitefile:3:1: '/a.html' differs only in case from
  '/A.html' (written by Sitefile:2 COPY); case-insensitive file systems cannot
  hold both`. Directories count too: after `/Docs/x`, writing `/docs/y` fails
  (`'/docs' differs only in case from '/Docs' (created by …)`), as do a file
  `/docs` or `/DOCS/x/z`. `--overwrite` does not change this: it replaces a
  file at the *same* path, and a differently cased path is a second file that
  case-insensitive file systems (the macOS and Windows defaults) cannot hold
  next to the first, whether it arrives by directory export or by unpacking a
  zip, tar or OCI image. Rename one of them. The same check
  applies to every tree (directory walks, archive imports, plugin outputs,
  pulled images), so an archive or OCI export can never hold such a pair.
- "Differ only in case" means equal under Unicode **simple case folding**:
  each code point is mapped through `CaseFolding.txt` entries with status `C`
  or `S` (Unicode 17.0.0), after NFC (below). So `Σ`, `σ` and `ς` are one
  name, as are `K` and the Kelvin sign `K` (U+212A), but full and Turkic
  foldings are not applied: `ß` and `ss`, `İ` and `i` are different names. Both
  implementations use the same generated table (`tools/gen-casefold.mjs`).
  A file system that folds more widely than this is caught at export time
  instead (see "Export").

### Unicode normalization of paths
- Every virtual path is Unicode **NFC**: COPY/RENDER/EXPORT operands and other
  Sitefile paths, file and directory names read from the context, archive
  and web bundle entry names, paths a plugin returns, rendered destinations
  built from data, and request paths in `serve`. NFC and NFD spellings
  (`café` as U+00E9 or as `e` + U+0301) are therefore the same path and give
  the same tree, the same output bytes and the same image ID on every OS;
  macOS, which often hands out NFD names, and Linux agree.
- A local source is found whichever normalization its on-disk name uses and
  whichever the Sitefile uses: a non-ASCII path segment is matched by NFC form
  against its directory listing (macOS does this itself; on Linux Sitefile
  does). Two names in one context directory that differ only in normalization
  (possible on Linux) are `E_PATH_COLLISION`, both in directory walks and when
  one of them is named explicitly.
- Within one archive, two entries that differ only in normalization are the
  same path (a duplicate entry, `E_CONFLICT`).

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
- `--strict-pins` (build, validate, inspect) enforces reproducibility, like
  Nix's fixed-output derivations: every remote input must already be pinned,
  or the command fails with `E_UNPINNED` **before anything is fetched**. A
  remote `IMPORT` is pinned by `INTEGRITY` or a `Sitefile.lock` entry for its
  URL (a Git import by a commit in its URL or its lockfile entry); the `BASE`
  image of an `oci` export (or `--base`) by `@sha256:` or a
  lockfile `images` entry. With `--no-lockfile` only the Sitefile's own pins
  count. Local imports never need one. `--strict-pins` conflicts with
  `--update-lock` (a usage error), since that would accept changed content.
  `inspect` and non-oci exports never resolve a `BASE`, so it is not checked
  there. `graph --strict-pins` checks like `validate`; `why` and `--target`
  builds check every `IMPORT` of the Sitefile (a target still resolves them all).
  `--cache-from`/`--cache-to` references are caches, not inputs, and are never
  checked. With `--provenance`, a strict build sets
  `externalParameters.strictPins: true` and gives each remote dependency an
  annotation `dev.sitefile.pin` (`integrity`, `lockfile`, or `digest` for the
  base); without `--strict-pins` the provenance is unchanged.
  There is no Sitefile-level switch yet (the flag needs no new syntax;
  a future `# sitefile=N` directive may add one). Typical CI use:
  `sitefile build --frozen-lockfile --strict-pins`.
- Library option `fetchPolicy.ca` (PEM string or Buffer) trusts an extra CA
  for https in addition to Node's default store (bundled roots plus
  `NODE_EXTRA_CA_CERTS` / `--use-system-ca`), for tests and enterprise private
  CAs. Certificate and hostname verification are never disabled; there is no
  CLI flag for it.

### Git sources (`git+https://…`)
`IMPORT name FROM git+https://host/repo.git#ref:path` imports a directory (or one file) of a Git
repository at a commit, and `sitefile build https://host/repo.git#ref:dir` builds a repository
(see [CLI.md](CLI.md#building-from-a-git-url)).
- **The URL.** `git+https://` (`git+http://` only with `--allow-http`); any other transport
  (`git+ssh:`, `git+file:`, `git://`) is `E_PROTOCOL`, as are credentials in the URL and a query
  string. After `#` come the **ref** and, after the first `:`, the **path** inside the repository:
  `#main`, `#v1.2:docs`, `#refs/heads/main:site/`, `#:docs` (the remote's HEAD), or no fragment at
  all (HEAD, whole repository). The ref follows `git check-ref-format` (`E_VALIDATION` otherwise); a
  path with `..` is `E_PATH_ESCAPE`. `${NAME}` interpolation works as in any operand.
- **Refs and commits.** A ref of exactly 40 lowercase hex digits is a **commit** (a pin). Any other ref
  is resolved on the server: `refs/…` exactly, otherwise `refs/tags/<ref>` then `refs/heads/<ref>`
  (Git's order); annotated tags are peeled to their commit. An unknown ref is `E_NOT_FOUND`.
- **Sitefile.lock** records each Git URL (as written) with the commit it resolved to, in a `git`
  section: `"git": {"git+https://host/r.git#main:docs": {"commit": "<40 hex>"}}`. A locked ref is
  **not** resolved again: the build fetches the locked commit (like Cargo's git dependencies), so a
  moving branch does not change the build until `--update-lock` re-resolves it. `--frozen-lockfile`
  requires the entry (`E_LOCK_MISSING`). An unpinned, unlocked ref warns
  (`IMPORT x is not pinned; pin commit <hex> (#<hex>:path) for a reproducible build`).
  A URL whose commit differs from its lockfile entry is `E_LOCK_MISMATCH`.
- **`--strict-pins`**: a commit in the URL or a lockfile entry pins a Git import (`dev.sitefile.pin`
  is `commit` or `lockfile` in the provenance). `INTEGRITY` does not apply to Git imports
  (`E_VALIDATION`): the commit id already names the content.
- **Kinds.** A directory imports as a tree (`AS tree`, the default), or as a Markdown, JSON or YAML
  collection (`AS markdown|json|yaml`, `SORT BY` as for local directories). A file imports like a
  downloaded file (`AS file|text|json|yaml|csv|markdown|wbn`, or `AS tree` to extract an archive).
  Files keep their executable bit (`0755`), everything else is `0644`. The provenance digest is the
  tree digest (or the file's sha256) and the commit is recorded as `gitCommit` (`--provenance`:
  `"digest": {"gitCommit": …, "sha256": …}`, `downloadLocation` = the URL with the commit as ref).
- **Protocol.** Sitefile speaks Git's smart HTTP **protocol v2** itself; no `git` program runs, so
  there are no hooks, no Git config, no credential helpers and no filters (`.gitattributes` is just a
  file; Git LFS files arrive as their pointer files). One capability request
  (`GET <repo>/info/refs?service=git-upload-pack`, `Git-Protocol: version=2`), an `ls-refs` request
  when the ref is not a commit, and one shallow `fetch` (`want <commit>`, `deepen 1`) of that commit.
  Dumb HTTP servers and servers without protocol v2 are refused (`E_GIT`), as are SHA-256
  repositories. All requests use the HTTPS client of remote imports, so the same network policy
  applies: public addresses only (checked at connect time) unless `--allow-private-network`, TLS
  verification, the size cap (`--max-download-bytes`, per request) and the timeout. Redirects are
  followed (and re-checked) for the capability request only; the repository URL it ends at is used
  for the rest, and a redirect of a `POST` is `E_REDIRECT`.
- **Verification.** The packfile is parsed by Sitefile (zlib objects, `OFS_DELTA` and `REF_DELTA`,
  bases inside the pack only); its trailing SHA-1 is checked, every object's id is recomputed from
  its content, and the commit must be the one asked for, so the files imported are exactly those the
  commit names. Inflated objects are bounded by the archive limits (`maxTotalBytes`, and
  `maxEntries` for the files of the tree; `E_ARCHIVE_LIMIT`). A malformed response is `E_GIT`.
- **What a tree may contain.** The same rules as a local directory and an archive: names are
  normalized to NFC (two names with one NFC form are `E_PATH_COLLISION`), names equal under case
  folding are `E_PATH_COLLISION`, and an entry named `.git` (any case), `.`/`..`, or containing a
  backslash is `E_ARCHIVE_UNSAFE`. **Symbolic links** are resolved like local symlinks, inside the
  imported tree only: a relative target that stays inside it is followed (files and directories); an
  absolute target or one that leaves it is `E_PATH_ESCAPE`; a dangling link is `E_NOT_FOUND`; more
  than 40 links in a row is a loop (`E_UNSUPPORTED_FILE`); a directory link back to one of its own
  ancestors is skipped. **Submodules** (gitlinks) are refused (`E_UNSUPPORTED_FILE`): import the
  submodule's repository with its own `IMPORT`.
- Private repositories need credentials, which URLs may not carry; they are not supported yet.

### Vendoring (`sitefile vendor`)
`sitefile vendor [context]` (like `cargo vendor`) downloads every remote input so
later builds work offline and without the network flags:
- It fetches each remote `IMPORT` and the `BASE` image (of an `oci` export, or
  `--base`) **exactly as a build would**: same network policy and limits, same
  `INTEGRITY` and `Sitefile.lock` checks (`--frozen-lockfile`, `--update-lock`),
  and `Sitefile.lock` is updated the same way. It always fetches; an existing
  vendor directory is not read.
- It then **replaces** the vendor directory (`--vendor-dir`, default
  `vendor/sitefile`, a path inside the context) as a whole, written to a
  temporary sibling and swapped in:
  - `blobs/sha256/<hex>`: content-addressed bytes: each import body (an archive
    imported `AS tree` is stored as the archive), and for a `BASE` image the blobs
    an offline pull needs: what the tag resolved to (an image index for
    multi-platform images), the manifest of the `--platform` variant (default
    `linux/amd64`), its config and layers. Only that platform is vendored.
  - `manifest.json`: `{"version": 1, "resources": {url: {integrity, size,
    resolved}}, "images": {ref: {digest, platform, manifest}}}` with sorted keys,
    so vendoring the same inputs gives the same bytes. `ref` is the lockfile key
    (`registry/name:tag`), or `registry/name[:tag]@sha256:...` for a digest pin.
    Git imports add `"git": {url: {commit, kind, integrity, size[, mode]}}` (only
    when there are any): `kind` is `tree`, stored as a deterministic tar of the
    imported files (mtime 0, modes `644`/`755`), or `file`, stored as its bytes
    with its `mode`. A build uses a vendored Git import when its commit is the one
    the build would fetch (the URL's commit, else the lockfile's); a ref with no
    lockfile entry uses the vendored commit (and records it), and `--update-lock`
    resolves the ref on the server again.
  A directory that resolves outside the context, a symlink, or an existing
  non-empty directory without `manifest.json` is refused (`E_PATH_ESCAPE`,
  `E_EXPORT_UNSAFE`).
- **Builds** (`build`, `inspect`, `serve --watch`) use `vendor/sitefile` when its
  `manifest.json` exists, or the directory named by `--vendor-dir` (which must
  exist: `E_NOT_FOUND` otherwise). A vendored URL is read from its blob instead of
  the network, so no fetch, redirect or `--allow-http`/`--allow-private-network`
  is involved; the bytes are checked against their digest (`E_INTEGRITY`, "run
  'sitefile vendor' again"), then against `INTEGRITY` and the lockfile exactly
  like a download. A URL the manifest lacks is fetched as usual, with a warning
  (`... is not vendored in vendor/sitefile; fetching it`). For a `BASE`, a tag
  with no lockfile entry uses the digest the vendor manifest recorded (and records
  it in the lockfile; `--frozen-lockfile` still requires the lockfile entry, and
  `--update-lock` re-resolves the tag from the registry), and the vendored blobs
  are copied into the image store, verified, before the pull, so the pull needs
  no network. Blobs that are not vendored (another platform) are pulled as usual.
- The vendor directory is never part of a directory copy (`COPY ./ /`), like
  `.git`; a file in it can still be named explicitly.
- It composes with the other build modes: `--target` builds and `why` resolve
  every `IMPORT` as a full build does, so they read the same vendored copies;
  `--cache-from`/`--cache-to` references are TRANSFORM caches, not inputs, so
  they are neither vendored nor affected by it.
- **Nothing secret is vendored**: URLs with credentials are rejected before any
  fetch, and registry tokens and passwords stay in memory. `resolved` is the final
  URL after redirects, as `Sitefile.lock` already records it.
- Commit the directory for offline or air-gapped CI, or cache it between runs.

### Checking pins (`sitefile outdated`)
`sitefile outdated [context] [--json]` (like `cargo outdated` / Renovate) reports,
for every remote input, what it is pinned to and what it resolves to now. It
**never changes anything** (no lockfile write, no image store, no pull);
`build --update-lock` stays the way to accept new content. What can honestly be
checked:
- **https IMPORTs** have no notion of "a newer version": a URL is just bytes. The
  URL is fetched again (a full GET under the same network policy and limits as a
  build) and its sha256 is compared with the pin, `INTEGRITY` first, else the
  `Sitefile.lock` entry. `current`: same bytes; `changed`: the URL now serves
  different bytes (a build would fail with `E_INTEGRITY`/`E_LOCK_MISMATCH`);
  `unpinned`: no pin (the digest it would get is shown). The lockfile records no
  `ETag`/`Last-Modified`, so no conditional request is possible, and a changed
  validator would not prove changed content anyway; the download is the check.
  A version inside a URL (`https://cdn.example/lib@1.2.3/x.js`) is not
  interpreted: discovering `lib@1.3.0` would need knowledge of each CDN.
- **BASE images** (the `oci` export's `BASE`, or `--base`): a tag is resolved again
  (one manifest GET) and compared with the digest in the reference
  (`name:tag@sha256:...`) or in the lockfile's `images`. `changed` means the tag
  has moved since it was pinned. A digest-only reference (`name@sha256:...`) is
  `digest-only`: there is no tag to follow, and nothing is requested.
- **Newer version tags**: when the tag looks like a version, `[v]N[.N[.N]][-suffix]`
  (`1`, `1.27`, `v2.0.1`, `1.27-alpine`), the registry's tag list
  (`/v2/<name>/tags/list`, following `Link` pages, at most 10 000 tags) is read
  and the greatest tag **of the same shape** (same `v` prefix, number of numeric
  components and suffix) that is numerically greater is shown as the newer tag:
  `1.25-alpine` suggests `1.27-alpine`, never `1.27.1-alpine` or `1.28`. This is a
  heuristic over tag names, not semver: tags carry no ordering metadata. A registry
  that refuses to list tags simply gives none.
- **Git imports**: the ref is resolved again (one `ls-refs`; nothing is downloaded) and compared with
  the commit in `Sitefile.lock`: `changed` means the ref has moved. A commit in the URL is
  `digest-only` (pinned by `commit`, nothing is requested); `pin` and `latest` are commits.
- A failed fetch or registry call is an `error` row (with its code and message),
  not a failure of the command; the exit code is 0 unless the Sitefile itself
  cannot be read or planned. `--no-lockfile` ignores `Sitefile.lock`.
- It reads the same inputs as a build: every `IMPORT` of the Sitefile (as a
  `--target` build still resolves them all) and the same `BASE`. It always asks
  upstream, never a vendor directory (`sitefile vendor` copies are what is
  pinned, not what is current), and `--cache-from`/`--cache-to` references are
  caches, not inputs.
- `--json` prints `{"inputs": [{kind, name, source, pin, pinnedBy, latest, status,
  newerTag, error}], "summary": {current, changed, unpinned, digest-only, error}}`;
  `pinnedBy` is `integrity`, `lockfile`, `digest`, `commit` or `null`, `error` is
  `{code, message}` or `null`.

### Archives (`IMPORT … AS tree` on a file)
- Formats are detected from magic bytes, not names: tar (ustar or v7), tar.gz /
  tgz (gzip), and zip (stored and deflate; no zip64, no encryption). Anything
  else fails with `E_TYPE`. Supported tar extras: POSIX ustar prefix (old GNU
  headers keep timestamps there and are read without it), GNU long names, pax
  `path`/`size` (a malformed record is `E_ARCHIVE`). Other pax keys are ignored.
  Zip directories are recognized by a trailing `/` or by a directory mode.
  A gzip stream may hold several members; zero bytes after the last one are
  ignored, any other trailing data must be another valid member.
  Decompression is asynchronous, so other imports keep making progress.
- `INTEGRITY` and the lockfile apply to the archive bytes exactly as for file
  imports, and the import's provenance digest is the archive's digest.
- Directories are implicit. Modes are reduced to `0644`/`0755`. A single
  top-level directory is **not** stripped: `COPY name:/top/ /` does that.
- Entry names are normalized to NFC, and names that differ only in case are
  `E_PATH_COLLISION` (see "Writes and conflicts").
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
output, whatever the order files were added, and the TypeScript and Rust
implementations write the same bytes. Compression uses Sitefile's own
deterministic DEFLATE encoder (`src/deflate.ts`, ported as `rust/src/deflate.rs`;
the algorithm is specified at the top of `src/deflate.ts`), not the platform's
zlib, so the output does not depend on the Node or zlib version. It compresses
to within about 1-2% of zlib level 9.
Entries are sorted by path; there are no directory entries (directories are
implicit); uid/gid are 0 with no user/group names; modes are the tree's
`0644`/`0755`. Timestamps are fixed: zip uses the DOS epoch 1980-01-01 00:00,
tar uses mtime 0, and the gzip header is fixed (mtime 0, no name, XFL 0, OS byte Unix). If
the `SOURCE_DATE_EPOCH` environment variable is set (non-negative integer
seconds), it is used for the zip (clamped to 1980..2107, 2-second resolution)
and tar timestamps instead.

- **zip**: raw deflate (stored when deflate does not shrink the entry), CRC-32, central directory, UTF-8 names (flag bit 11), unix modes in
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
  it), gzip-compressed (`application/vnd.oci.image.layer.v1.tar+gzip`), or,
  with `COMPRESSION none` / `--oci-compression none`, stored as the plain tar
  (`application/vnd.oci.image.layer.v1.tar`: larger, but nothing to decompress,
  and the layer digest equals the diff_id). The config's `rootfs.diff_ids` is
  the sha256 of the *uncompressed* tar; the layer descriptor is the sha256 of
  the layer blob (the gzip, or the tar itself). The same build gives the same
  layer digest, manifest digest and image ID from both implementations. With a
  `BASE`, only the site layer follows `COMPRESSION`; the base's layers keep
  their own media type. The store, `save`, `serve`, `push` and `pull` read both
  layer types. `created` is `SOURCE_DATE_EPOCH`
  or `1970-01-01T00:00:00Z`; `architecture`/`os` default to `amd64`/`linux`
  (`--platform os/arch`; metadata only). Labels: `org.opencontainers.image.title`
  (the context directory name), `dev.sitefile.root`, any `--label key=value`,
  and, only with `--git-labels`, `.source` and `.revision` from the repository
  checkout (read from its `.git` directory without running anything;
  credentials in URLs are dropped). Git labels are off by default because they
  change the image ID on every commit even when the files are identical, and
  finding the checkout reads outside the build context. Build-arg values
  are never written into the image (`--provenance` records the non-secret
  ones in a separate attestation, [CLI.md](CLI.md#attestations)). The config, manifest and index are canonical JSON (sorted
  keys, no whitespace), so the same build always yields the same manifest digest.
  `--out` ending in `.tar` (`site.oci.tar`) is an *OCI archive* (a tar of the
  layout, below); any other `--out` with `--format oci` is an *OCI layout
  directory*: `oci-layout`, `index.json` (one manifest entry, annotated
  `org.opencontainers.image.ref.name` when you pass `-t`; with `--provenance` /
  `--sbom`, the attestation manifests follow it, see
  [CLI.md](CLI.md#attestations)) and `blobs/sha256/<hex>`. **The image contains only files**: it has no
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
- If the destination file system still treats two tree paths as one name
  (one that folds case or normalizes Unicode more widely than the checks in
  "Writes and conflicts"), the export fails with `E_EXPORT` naming the
  artifact path (`cannot write '/a.html': the destination file system already
  has an entry by that name …`), never the temporary path, and the previous
  output is left in place.
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
