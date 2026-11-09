# Sitefile language draft v0.1

Line-oriented, Dockerfile-inspired, case-insensitive instruction keywords. Quoted strings, comments, and backslash continuations. No implicit shell.

Instructions:
- `FROM scratch|<local dir>|<stage>|<import>+<stage>|image <ref> [AT <dir>] [AS name]` (optional for a single-stage build: the first instruction other than `ARG` and `IMPORT … AS sitefile` starts an implicit `FROM scratch`)
- `STAGE <name> [FROM scratch|<local dir>|<stage>|image <ref> [AT <dir>]]` starts a named stage; same as `FROM <base> AS <name>` (base defaults to `scratch`)
- `ARG NAME[: type] [= value]` external build parameter
- `LET NAME = value` local immutable binding; `LET name: html = <markup>` (or a `<<HTML` heredoc) is an html fragment, see HTML fragments below
- `IMPORT name FROM <uri> [AS json|text|tree|file|…|sitefile] [INTEGRITY sha256:<hex>] [ARG NAME=value]…` (`AS sitefile`: another Sitefile, whose stages are `name+<stage>`; see Sitefile imports below)
- `COPY [--from=<stage>] [--exclude=<glob>…] [--chmod=644|755] [--parents] <source> <destination>`; source can be local path or `name:/path`
- `RENDER <template> TO <destination>`; interpolation `${NAME}` only in MVP
- `EXPORT <tree-path> [AS directory|zip|tar|tgz|wbn|oci] [BASE <https-url>] [AT </srv/www>] [COMPRESSION gzip|none]` (CLI specifies destination)
- `PLUGIN`, `TOOL` and `TRANSFORM` (custom renderers and allow-listed external programs): see Plugins below.
- `SITEMAP`, `FEED`, `REDIRECT` and `HEADER` (sitemap.xml, an Atom feed, `_redirects`, `_headers`): see Site files below.
- `FINGERPRINT [--overwrite] <pattern>… [MANIFEST <dest>]` (content-hashed asset names, with the HTML and CSS that reference them rewritten): see Fingerprinting below.
- `CHECKLINKS [--warn] [--external]` (fail, or warn, on links to files that do not exist): see Link checking below.
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
- **Syntax version directive.** A comment `# sitefile=1` (like Dockerfile's
  `# syntax=`) pins the language version the file is written for, so the
  syntax can change later without breaking existing files. A file without it
  is version 1, the only version so far.
  - It is a comment line that starts `#`, then `sitefile` (ASCII
    case-insensitive), then `=`, with spaces or tabs allowed around the name and
    `=`: `# sitefile=1`, `#sitefile = 1`. The rest of the line, trimmed, is the
    version. Other comments (`# sitefile: 1`, `# sitefile-lint: …`) are ordinary
    comments.
  - It must come before the first instruction (blank lines and other comments
    may precede it) and appear at most once; otherwise `E_SYNTAX`
    (`the '# sitefile=' directive must come before the first instruction`,
    `'# sitefile=' given twice`). Inside a heredoc body it is body text.
  - The version is a whole number without leading zeros (`E_SYNTAX`:
    `invalid syntax version '1.0' in '# sitefile=' (expected a whole number such as 1)`).
    A version this sitefile does not read fails with `E_UNSUPPORTED` at the
    version: `this Sitefile uses syntax version 2; this sitefile supports version 1 (upgrade sitefile)`.
  - Editors read it too: the language server reports these errors and shows
    the directive's docs on hover; the TextMate and tree-sitter grammars
    highlight it as a directive.
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
    (and `IMPORT … AS sitefile`, see Sitefile imports) comes before any `FROM`, an unnamed `FROM scratch` stage starts there, so a
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
  - **`image <ref> [AT <dir>]`** starts the stage from the files of an OCI
    site image (issue #84), so a theme can be published as an image and a site
    built on it: `FROM image ghcr.io/me/theme:1.2 AS theme`,
    `STAGE theme FROM image ${THEME}`. See *FROM image* below.
  - Stage names are identifiers, unique, and share no namespace with
    ARG/LET/IMPORT: a collision in either order is `E_DUPLICATE`. A name used
    before its stage is complete (itself or a later stage) is `E_UNDEFINED`, so
    there are no cycles.
  - **Scope:** stages scope only the tree. ARG, LET and IMPORT stay one global
    namespace in source order, wherever they appear, and an IMPORT is usable
    from any later stage.
  - **EXPORT** is still exactly one and last, and exports from the **last
    stage**. Earlier stages exist only to feed later ones.
- **FROM image** (`FROM image <ref> [AT <dir>] [AS name]`, `STAGE name FROM image <ref> [AT <dir>]`):
  - **Telling it apart.** `image` is a keyword only when another word other
    than `AS` follows it, so `FROM image`, `FROM image AS x` and
    `STAGE x FROM image` still name a stage or a directory called `image`
    (write `./image` to be explicit). `scratch`, stage names and directories
    keep their meaning; nothing that parsed before changes.
  - **The reference** (an operand: `${VAR}` works) has the image grammar of
    `pull`: `[registry-host[:port]/]name[:tag][@sha256:<hex>]`, checked when the
    Sitefile is planned (`validate` reports `E_VALIDATION`). With a **registry
    host** (a first component containing `.` or `:`, or `localhost`) it names an
    image on that registry. **Without one** (`theme:1`, `theme@sha256:…`, an image
    ID) it names an image in the **local store** (`sitefile build -t theme:1`,
    `pull`, `load`), never Docker Hub, and the network and lockfile are never
    involved; a missing one is `E_NOT_FOUND` at the FROM line ("build it with -t
    theme:1, pull it, or name its registry"). To use Docker Hub, write
    `docker.io/…`.
  - **Pinning** (registry references, exactly like an `oci` export's `BASE`):
    `@sha256:` pins the image in the Sitefile; a tag is resolved to a digest
    once and recorded in `Sitefile.lock` under `images` (key
    `registry/name:tag`), and later builds fetch **by digest**, so a moved tag
    changes nothing until `--update-lock`. `--frozen-lockfile` fails with
    `E_LOCK_MISSING` for an unrecorded tag; `--no-lockfile` resolves the tag on
    every build and warns. `--strict-pins` requires `@sha256:` or a lockfile
    entry (`E_UNPINNED` before anything is fetched); local-store images need no
    pin, like local files. A `--target` build that stops before the stage keeps
    its lockfile entry.
  - **Fetching** goes through the registry client of `pull` and `BASE`: the same
    credentials (`sitefile login`, `~/.docker/config.json`, helpers), TLS,
    `--insecure-registry`, private-network rule (`--allow-private-network`
    for a registry on a private address; loopback is allowed) and size limits.
    An image already in the store is used without network access, and pulled
    blobs count as recently used under the cache size limit (docs/CLI.md).
    Any platform is accepted (an index is narrowed to `--platform`, default
    `linux/amd64`): only the files are read.
  - **Which files**: the image's site layer (its only layer, or the last one of
    an image built with `BASE`), verified against its digest and `diff_id`, under
    the image's root: the `dev.sitefile.root` label that `EXPORT … AS oci` writes
    (its `AT`, default `/srv/www`), or `/` for an image without one. `AT <dir>`
    (an absolute path inside the image) picks another directory, e.g. the web
    root of an unlabelled single-layer image. Other multi-layer images are
    `E_STORE` ("not a Sitefile image"). The files keep the origin
    `image <registry/name:tag>@<digest>:<path in the layer>`, which conflict
    messages and `why` show. Later instructions overlay them as usual
    (`COPY --overwrite` to replace one).
  - **Elsewhere**: `graph` shows an `image:<ref>` node (kind `image`) with a
    `base` edge to the stage; `sitefile vendor` vendors the image's blobs like a
    `BASE` (local-store images are not vendored); `sitefile outdated` lists each
    registry `FROM image` as kind `image`, named by its stage; `--provenance`
    lists it as a resolved dependency `from:<line>`. `validate`, `graph` and
    `inspect --target` before the stage never resolve it.
  - Example: `examples/theme` (the theme, built with `-t theme:1`) and
    `examples/themed-site` (a site on it).
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
  `LET name: html …` is an html fragment (see HTML fragments below); `html` is
  not a type for ARG (`E_SYNTAX`).
- Operands of every instruction can use `${NAME}` for ARG/LET names defined on
  an earlier line. Dotted access is only allowed inside RENDER templates.
- `IMPORT name FROM <uri> [AS file|text|json|yaml|csv|markdown|wbn|html|tree|sitefile] [INTEGRITY sha256:<hex>] [SORT BY key [ASC|DESC]] [SCHEMA <file>] [DRAFTS true|false] [SECRET <id> AS HEADER <Name> | SECRET <id> AS BEARER]… [ARG NAME=value]…`
  - `AS sitefile` and `ARG`: see Sitefile imports below.
  - `SCHEMA`: see Schemas below (not with `AS sitefile`; an imported Sitefile's own SCHEMA paths are read from its own context).
  - `DRAFTS`: see Tag pages, pagination and drafts below (also not with `AS sitefile`).
  - `SECRET`: see Build secrets below.
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
- `COPY [--overwrite] [--from=<stage>] [--exclude=<pattern>]… [--chmod=644|755] [--parents] <source> <dest>`
  - With `--from=<stage>`, `<source>` is an absolute path (starts with `/`, no
    `..`) inside the final tree of an earlier, completed stage: `/out/` or
    `/a.txt`. `--from=<import>+<stage>` reads a stage of a Sitefile imported `AS sitefile`. File, directory, trailing-slash and conflict rules below apply
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
  - **`--exclude=<pattern>`** (repeatable, like Dockerfile's `COPY --exclude`)
    leaves out files of a **directory** source. Each pattern is one
    `.sitefileignore` line (see below: `*`, `?`, `**`, a trailing `/` for
    directories, a leading or inner `/` to anchor, `!` to re-include, later
    patterns win, an excluded directory is not re-entered), matched against
    paths **relative to the source directory**: `COPY --exclude=*.md
    --exclude=drafts/ ./site /` copies `site/` without its Markdown files and
    its `drafts` directory. It applies to local directories (excluded
    directories are not read at all, so a hard-linked `node_modules` excluded
    this way does not trigger `E_HARDLINK`), `name:` tree imports and `--from`
    stage directories, in addition to `.sitefileignore`. The value is the rest
    of the flag word, literal (no quotes, `${…}` or spaces). A value that is no
    rule (`--exclude=#x`, `--exclude=/`) is `E_SYNTAX`; a file source is
    `E_TYPE` (`--exclude filters a directory, but './a.txt' is a file`); with an
    inline body, `E_SYNTAX`. If nothing is left, the build warns
    (`--exclude leaves no files of './site' to copy`).
  - **`--chmod=644|755`** (also `0644`, `0755`) sets the mode of every copied
    file, including an inline body (`COPY --chmod=755 <<SH /run.sh`). Trees only
    hold `0644` and `0755` files, so any other value is `E_SYNTAX`
    (`--chmod must be 644 or 755 …`). The mode is part of the write: the same
    bytes with a different mode at the same path are `E_CONFLICT` without
    `--overwrite`.
  - **`--parents`** keeps the source's own path under `<dest>`, which is then
    always a directory: `COPY --parents ./src/css/a.css /out` writes
    `/out/src/css/a.css`, `COPY --parents ./src/css /out/` puts the directory's
    contents under `/out/src/css/`. The kept path is context-relative for a
    local source and the path inside the import (`site:/css` → `css`) or stage
    (`--from=s /www/a` → `www/a`) otherwise; a whole context, import or stage
    keeps nothing. There is no `/./` pivot. Not allowed with an inline body
    (`E_SYNTAX`).
- `RENDER [--escape=none|html] [--overwrite] [--from=<stage>] [--inline-svg] <template> TO <dest> [FOR item IN list [BY key] [PAGE n [FIRST <dest>]]]` (`BY`, `PAGE`: see Tag pages, pagination and drafts below)
  - `<template>` is a source like COPY's (including `--from`, so a template can be produced by an earlier stage) and must be a UTF-8 file. `<dest>` is
    a file path (no trailing slash).
  - Templates use `${NAME}` for ARG/LET, `${name}` for a text import, and
    `${name.key.0.key}` for a scalar in a JSON import. `$${` writes a literal
    `${`. Unknown names fail with the template's line and column.
  - `--escape=html` escapes `& < > " '` in every substituted value.
  - `${file "/path" | helper …}` writes tags and values for files of the
    stage's tree (see File helpers below); `--inline-svg` lets its `svg`
    helper paste SVG markup into the page.
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
  - Only `COPY`, `RENDER`, `TRANSFORM` and html `LET` lines (`LET name: html`)
    are scanned for `<<` (`TRANSFORM <<NAME WITH …` passes the body to the
    plugin as its source, byte for byte; an html LET's body is its template);
    elsewhere it is plain text, so `LET a = <<EOF` is still the string `<<EOF`.
  - See `examples/inline/`.
- `EXPORT <path> [AS directory|zip|tar|tgz|wbn|oci|car|html|mhtml|warc|swbn|wacz|epub] [BASE <url>] [AT <root>] [COMPRESSION gzip|none] [KEY <secret-id>] [FOR <host>]` must be the last
  instruction, exactly once; it exports from the last stage (`--target <stage>` exports a named stage's whole
  tree instead; see [CLI.md](CLI.md#build-target---target-stage)). The format
  defaults to `directory`; `tar.gz` is accepted as an alias for `tgz`; names
  are case-insensitive. `BASE` (an operand, so `${VAR}` works) is the URL the
  tree is mounted at and is required by `wbn` (from here or `--base-url`).
  With `AS oci`, `BASE` is instead a **container image** to build on (see *oci* below). `--base` overrides it.
  `AT` (an operand) is the directory the files occupy inside an `oci` image
  (default `/srv/www`; `--oci-root` overrides it); with any other format it is
  an error (`E_VALIDATION`).
  `COMPRESSION gzip|none` (a word, case-insensitive; `oci` and `wbn` only,
  otherwise `E_VALIDATION`). With `AS oci` it chooses how the site layer is
  stored: `gzip` (the default, `tar+gzip`) or `none` (a plain `tar` layer).
  With `AS wbn`, `gzip` wraps the finished bundle in gzip, the
  `gzip(application/webbundle)` archive that `@johnhenry/packfile`'s
  `fromArchive()` and `@johnhenry/fileable`'s `encode="wbn"` use, and `none`
  (the default, so existing exports do not change) writes the plain bundle.
  Any other value is `E_SYNTAX`. `--compression` overrides it (and
  `--oci-compression`, for oci); it belongs to the EXPORT's own format, so
  `--format wbn` on an `AS oci COMPRESSION …` Sitefile does not inherit it.
  `KEY <secret-id>` (swbn only, and required there; otherwise `E_VALIDATION`) names the build secret
  that holds the signing key (see *swbn* below).
  The clauses come in this order: `AS`, `BASE`, `AT`, `COMPRESSION`, `KEY`.
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
  - `${#each list by key as group}…${/each}` repeats for each group of the
    list's elements by the field `key`, exactly as `RENDER … FOR … BY` groups
    them (see Tag pages below): `group` is `{ key, count, items }`, groups in
    `SORT BY` order of their keys, the same `E_TYPE` errors (`${#each posts by
    tags}: 'tags' of document '/a.md' is not text or a list of text`). No
    element with the field means no groups, so `${else}` renders. This is how
    one page lists every tag with its count.
  - `${#if value}…${else}…${/if}` and `${#if !value}`: missing, `null`,
    `false`, `''`, `0` and empty lists are false.
  - `${value|raw}` inserts a value without escaping, for `${post.content|raw}`
    under `--escape=html`. Every other value is still escaped.
  - A block tag alone on its line removes that whole line from the output.
  - Unbalanced blocks, a second `${else}` or an unknown `|filter` fail with
    `E_TEMPLATE` at the tag.

### HTML fragments: `LET name: html` and `IMPORT … AS html` (see `examples/blog/`)
Name a piece of HTML once and use it in any RENDER template: a header, a nav, a
card. Every other LET is evaluated where it is declared, before any file
exists; a fragment is a **template that is evaluated where it is used**.

```
ARG SITE = "My blog"
IMPORT posts FROM ./posts AS markdown
LET nav: html = <nav>${#each posts as p}<a href="/${p.slug}/">${p.title}</a>${/each}</nav>
LET header: html <<HTML
<header><h1>${SITE}</h1>${nav}</header>
HTML
IMPORT footer FROM ./partials/footer.html AS html

RENDER --escape=html <<HTML TO /${item.slug}/index.html FOR item IN posts
${header}<main><h2>${item.title}</h2>${item.content|raw}</main>${footer}
HTML
```

- **Forms.** `LET name: html = <markup>` takes the rest of the line (trailing
  whitespace dropped) as the template; it cannot continue with `\` onto the next
  line (`E_SYNTAX`; write a heredoc), so error positions are exact.
  `LET name: html <<HTML` (or `= <<HTML`) takes a heredoc body with the usual
  rules (`<<-` strips tabs, the body is raw, a missing closing line is
  `E_SYNTAX`), except that the template is the body **without its final
  newline**, since a fragment is inserted inline (`${header}<main>`). The type
  is ASCII case-insensitive (`: HTML`). `IMPORT name FROM ./partials/x.html AS html` makes a UTF-8 file a
  fragment with the same rules (its final newline, `\n` or `\r\n`, is dropped
  too; a directory is `E_TYPE`, invalid UTF-8 `E_ENCODING`); it may also be remote or from Git, pinned like any import.
- **A type, not a convention.** A fragment's markup is inserted as written,
  also under `--escape=html`; the values substituted *inside* it (`${SITE}`,
  `${p.title}`) are escaped by the escape mode of the RENDER that uses it. A
  plain string LET stays escaped as before, so `--escape=html` keeps its
  guarantee (like Go's `template.HTML`). `${name|raw}` on a fragment is the
  same as `${name}`.
- **Evaluated where it is used.** `${name}` in a template renders the
  fragment in place, against that RENDER: its `FOR` loop variable (`${item…}`),
  the `${#each}` variables around the use, its escape mode. A fragment may use
  other fragments. With `FOR … BY` or `PAGE` the loop variable is the group or
  page (`${page.items}`, `${tag.key}`), as in the RENDER's own template. In a
  Sitefile imported `AS sitefile`, its fragments and `AS html` imports resolve
  in that Sitefile (its ARGs, its paths); the importer's fragments of the same
  name are not visible there.
- **Checked where it is declared** (`validate` reports these at the LET):
  its template syntax (`E_TEMPLATE`, at the exact line and column), and every
  name it reads that is declared before it (a dotted `${T.x}` on an ARG/LET is
  `E_TYPE`; a stage, PLUGIN or TOOL is `E_UNDEFINED`). A name declared only
  *after* the LET is `E_UNDEFINED` (define before use: `'B' is declared on line
  9, after LET a: html; a fragment can only use names declared before it`), and
  a fragment that uses itself is `E_TEMPLATE`. A name declared nowhere is left
  to the use site (usually a FOR loop variable). An imported partial is checked
  where it is used (its text is read with the imports).
- **Errors at the use site** keep the fragment's own position and name the use:
  `Sitefile:3:21: 'item' is not an ARG, LET or IMPORT (in html fragment 'card'
  used at Sitefile:9:5)`. A partial that reaches itself through other fragments
  is `E_TEMPLATE` naming the chain (`html fragment 'b' uses itself (b → a → b)`).
- **Templates only.** A fragment in any other operand (a COPY path, an EXPORT,
  OPTIONS, ARGS, …) is `E_TYPE` (`'header' is an html fragment; html values can
  only be used in RENDER templates`), as is `FOR x IN header` or `FEED header`.
  Inside a template, a fragment is not a value: a dotted `${header.x}`,
  `${#each header …}` or `${#if header}` is `E_TYPE`.
- **Namespace.** Fragments share the ARG/LET/IMPORT namespace and the
  define-before-use rule, and are not overridable by `--build-arg`. They are
  not in `validate --json`'s `vars`.
- **Incremental rebuilds** treat a fragment's text like an ARG/LET value, and
  a partial like any import a template reads.

### File helpers: `${file "/path" | img alt="…"}` (see `examples/image/`)
A template can write the tag for a file: `<img>` with its size, `<link
rel="stylesheet">` with its integrity, and so on. Tags need the URL a file is
served at, which an IMPORT does not know (it has not been placed), so a helper
reads a file **already in the current stage's tree, by its output path**, when
the RENDER runs:

```
COPY ./media/ /img/
COPY ./css/ /css/
RENDER --escape=html <<HTML TO /index.html
${file "/img/hero.jpg" | img alt="Our team"}
${file "/img/intro.mp4" | video controls poster="/img/intro.jpg"}
${file "/css/site.css" | stylesheet}
HTML
```
```html
<img src="/img/hero.jpg" width="1600" height="900" alt="Our team" loading="lazy" decoding="async">
<video src="/img/intro.mp4" controls poster="/img/intro.jpg"></video>
<link rel="stylesheet" href="/css/site.css" integrity="sha384-…">
```

- **Syntax.** `${file <path> [| <helper> [<name>[=<value>]]…]}`. `file` is the
  helper keyword only before a space or tab (`${file}` is still a variable named
  `file`). The path and attribute values are a `"double-quoted"` string (`\`
  escapes, `${name}` / `${name.key}` references, which may contain `}`), a
  `'single-quoted'` one (literal) or a bare word (up to whitespace, `|` or `}`).
  Attribute names are `[A-Za-z_:][A-Za-z0-9_:.-]*`, each at most once (ASCII
  case-insensitive); a bare name is a boolean attribute (`controls`). Anything
  else is `E_TEMPLATE` at the `${`: an unknown helper, attributes on a value
  helper, an unterminated string.
- **The file** is an absolute path in the tree at this RENDER (`E_PATH` for a
  relative one; a directory is `E_TYPE`), normalized like any destination. A
  path nothing has written yet is `E_NOT_FOUND`: `'/img/hero.jpg' is not in the
  tree at this RENDER (write it with COPY, RENDER or TRANSFORM on an earlier
  line)`. Its URL is the path with each segment percent-encoded like
  `encodeURIComponent` (`/img/my%20photo.jpg`).
- **Value helpers** produce a value, escaped like any `${…}` (so under
  `--escape=html` only): none or `url` (the URL), `width` and `height` (of an
  image, see below), `type` (the media type, by extension), `size` (bytes),
  `integrity` (`sha384-…` of the bytes), `datauri` (`data:<type>;base64,…`, at
  most 32768 bytes, `E_TOO_LARGE` otherwise, so a photo is not inlined by
  accident), `srcset` (from image plugin metadata, see below).
- **Tag helpers** write markup, inserted as-is also under `--escape=html`;
  every attribute value in it (the path, `alt`, your attributes) is
  HTML-escaped. The helper's own attributes come first, then yours in order,
  then its defaults; one of yours named like an own or default attribute
  replaces its value in place (`loading=eager`).

  | Helper | Writes |
  | --- | --- |
  | `img` | `<img src width height … loading="lazy" decoding="async">` (no width/height for an SVG); from image plugin metadata, `<img src srcset width height …>`, in a `<picture>` for several formats |
  | `video`, `audio` | `<video src …></video>`, `<audio src …></audio>` |
  | `source` | `<source srcset type …>` for an image (in a `<picture>`), `<source src type …>` otherwise |
  | `script` | `<script src integrity …></script>` |
  | `stylesheet` | `<link rel="stylesheet" href integrity …>` |
  | `preload` | `<link rel="preload" href as …>`: `as` from the extension (style, script, font, image, fetch) or your `as=`; `integrity` for script and style, `crossorigin` for font and fetch |
  | `modulepreload` | `<link rel="modulepreload" href integrity …>` |
  | `pre` | `<pre …>` with the file's UTF-8 text, escaped |
  | `svg` | the SVG file's markup, without its XML declaration, comments before the root and doctype; your attributes go on the root `<svg` |

- **Integrity** is added only where browsers check it: scripts, stylesheets and
  their preloads. They ignore it on images and media, where `width`/`height`
  (no layout shift while loading) are what helps. Files are same-origin, so no
  `crossorigin` is added for integrity; pass `crossorigin=anonymous` when the
  site is served from elsewhere.
- **Image sizes** are read from the file header, not by decoding: PNG (IHDR),
  GIF (screen descriptor), JPEG (the first SOF; EXIF orientation 5-8 swaps
  width and height, as browsers display it), WebP (VP8, VP8L, VP8X) and AVIF
  (the primary item's `ispe`; `irot` of a quarter turn swaps them). Reads are
  bounded by the file and never allocate by the declared size. A file whose
  bytes are none of these, or whose header is truncated, is `E_DATA` (`'/a.png'
  is not a valid PNG image: its header is truncated or malformed`); a non-image
  extension is `E_TYPE`. Both implementations are checked against the same
  fuzzed headers (`rust/tests/fixtures/media`).
- **SVG is a script risk**: an SVG file can hold `<script>` and event handlers.
  `img` of an SVG is safe (browsers do not run scripts in images). The `svg`
  helper pastes the markup into the page, so it needs `RENDER --inline-svg`;
  without it the build fails with `E_UNSUPPORTED`. Nothing is sanitized: inline
  only SVG you trust.
- **Image plugin metadata** (`examples/image/`). The image plugin writes
  `<name>.json` next to the files it makes (docs/PLUGINS.md, "The metadata
  file"). The image helpers (none or `url`, `type`, `width`, `height`,
  `srcset`, `img`) read it **whenever it is there**: for `${file
  "/img/sky.jpg" | …}`, the file `/img/sky.json` in the tree, when it is an
  object with a number `version` and a `variants` list (any other JSON there is
  ignored); or the `.json` itself, given to `width`, `height`, `srcset` or `img`
  (`E_DATA` if it is not metadata). So `/img/sky.jpg` itself need not exist.
  From it:
  - `url`, `type`, `width`, `height`: `src`, `type`, `width`, `height` (the
    largest variant of the fallback format); `srcset`: its `srcset` (`E_TYPE`
    for a file without metadata).
  - `img`: `<img src srcset width height … loading="lazy" decoding="async">`;
    with several formats, inside `<picture>` after one `<source type srcset>`
    per other format, modern first (your `sizes` is copied to each source):
    `${file "/img/sky.jpg" | img alt="Sky" sizes="(min-width: 40em) 50vw, 100vw"}`.
  - Values are used as the plugin wrote them. The metadata must be version 1
    (`'/img/sky.json' is image metadata version 2; this sitefile reads version
    1 (upgrade sitefile)`, `E_DATA`), with string `src`, `type`, `srcset`,
    positive whole `width`/`height` and a `sources` list of `{type, srcset}`
    (`E_DATA` naming the field otherwise). Other helpers (`size`, `integrity`,
    `datauri`, …) read the file itself.
  - After a FINGERPRINT that renamed the variants (and the `.json`), the old
    paths still work and each absolute URL in `src` and `srcset` that names a
    renamed file becomes its hashed URL; other URLs (another host, a file it
    did not rename) stay as written.
- **Fragments and loops.** Helpers work in html fragments (evaluated at the
  use, against that RENDER's tree and flags) and in `RENDER … FOR`, with
  `${item.key}` in their strings: `${file "/img/${item.file}" | img alt="${item.alt}"}`.
  A reference that is an html fragment is `E_TYPE`.
- **Incremental rebuilds**: a RENDER whose template (or a fragment it uses) has
  a `${file …}` is keyed by its input tree too, so a changed file is seen.
- **With FINGERPRINT**: a `${file …}` *after* a FINGERPRINT in the stage (or
  in a stage `FROM` it) may name the file's old path: `${file "/style.css" |
  stylesheet}` writes the hashed URL (`/style.3f2a…9c.css`) and an `integrity`
  of the hashed file's bytes, which are what is served. This is the order to
  use for SRI. A helper *before* a FINGERPRINT writes the path the file has at
  the RENDER, and the FINGERPRINT rewrites that reference (`src`, `href`,
  `srcset`) like any other, but `integrity` stays that of the bytes at the
  RENDER: right for scripts and for stylesheets FINGERPRINT does not change,
  wrong for a stylesheet whose own `url(…)` references it rewrites.

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
  magic bytes, like tar and zip; a gzip-wrapped bundle, as
  `@johnhenry/fileable`'s `encode="wbn"`, `@johnhenry/packfile`'s `toArchive()`
  and `EXPORT … AS wbn COMPRESSION gzip` write it, is unpacked the same way,
  with the limits applied to the decompressed bundle): URL pathnames become paths (`…/` →
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

### Tag pages, pagination and drafts (see `examples/blog/`)
Three blog staples, built on `IMPORT` and `RENDER … FOR`:

```
ARG DRAFTS: boolean = false
ARG PER_PAGE: number = 10
IMPORT posts FROM ./posts AS markdown SORT BY date DESC DRAFTS ${DRAFTS}
RENDER --escape=html ./index.html TO /page/${page.number}/index.html FOR page IN posts PAGE ${PER_PAGE} FIRST /index.html
RENDER --escape=html ./tag.html TO /tags/${tag.key}/index.html FOR tag IN posts BY tags
RENDER --escape=html ./tags.html TO /tags/index.html
FEED tag BASE ${SITE_URL} LINK /posts/${item.slug}/ TO /tags/${tag.key}/feed.xml TITLE "Tagged ${tag.key}" FOR tag IN posts BY tags
```

- **Drafts: `IMPORT … DRAFTS <true|false>`.** With `false`, the elements whose
  `draft` field is `true` (or the text `true`, as in a CSV cell) are left out of
  the import, so of everything that reads it: `RENDER … FOR` pages, `#each`
  lists, groups and pages, FEED entries, and therefore the SITEMAP (which lists
  the pages written). With `true` they are kept. The value is an operand that
  must be `true` or `false` once interpolated (`E_VALIDATION` otherwise), so
  the usual pattern is a boolean ARG: drafts stay out unless the build says
  `--build-arg DRAFTS=true` (a preview). Without the clause nothing is left out
  (importing never filters implicitly). `DRAFTS` applies to `AS markdown`,
  `json`, `yaml` and `csv` (`E_VALIDATION`), at most once, and needs a list
  (`E_TYPE` for a single document). Drafts are left out first, then the
  `SCHEMA` checks the rest, then `SORT BY` orders it.
- **Tag pages: `RENDER … FOR tag IN list BY key`.** Instead of once per
  element, the template renders once per distinct value of the element field
  `key`: `tag` is `{ key, count, items }`, where `key` is the value as text
  (`${tag.key}`), `items` the elements that have it, in list order, and `count`
  their number. A field that is a list (`tags: [a, b]`) puts the element in a
  group per value (a value repeated in one element counts once); elements
  without the field, or with `null` or `''`, are in no group. Values must be
  text, numbers or booleans (`E_TYPE`: `FOR tag IN posts BY tags: 'tags' of
  document '/a.md' is not text or a list of text`), and an element that is not
  an object is `E_TYPE`. Groups come in `SORT BY` order of their keys (numeric
  ones numerically, then text by UTF-16 code units); two keys that only differ
  in case (`Rust`, `rust`) are two groups whose paths collide
  (`E_PATH_COLLISION`). If no element has the field, nothing is rendered and
  the build warns.
- **A page listing every tag: `${#each posts by tags as tag}`.** The groups of
  `BY` are only the loop variable inside that RENDER; any template can group a
  list itself with `${#each list by key as g}` (see Markdown and template
  blocks above), so `tags.html` above is
  `${#each posts by tags as tag}<a href="/tags/${tag.key}/">${tag.key}</a> (${tag.count})${/each}`.
- **A feed per tag: `FEED tag … FOR tag IN list BY key`.** FEED's `FOR <var>
  IN <import> BY <key>` clause (after TITLE) writes one Atom feed per group:
  the subject of the FEED is the loop variable (`FEED tag …`; another name is
  `E_SYNTAX`), each feed's entries are its group's `items` (in list order), and
  `TO` (required with FOR, `E_SYNTAX`) and `TITLE` are evaluated per group with
  `${tag.key}` / `${tag.count}`, so `TO /tags/${tag.key}/feed.xml`. `LINK` still
  uses `${item…}` (the loop variable cannot be named `item`, `E_DUPLICATE`).
  Groups are formed as for `RENDER … BY` (same order, same `E_TYPE` errors);
  feeds whose destinations collide are `E_CONFLICT` (unless identical) or
  `E_PATH_COLLISION`, a destination ending in `/` is `E_PATH`, and no group at
  all writes nothing and warns. Without TITLE every feed is titled with the BASE URL.
- **Pagination: `RENDER … FOR page IN list PAGE <n> [FIRST <dest>]`.** The
  list is cut into pages of `n` items (`n` is an operand, a whole number from 1
  to 999999999, `E_VALIDATION` otherwise; `${PER_PAGE}` works) and the template
  renders once per page. `page` is `{ count, items, number, total, url, prev,
  next }`: the page's `items`, its `number` (from 1) of `total` pages, `count`
  items in the whole list, and the URLs of this, the previous and the next page
  (`''` when there is none, so `${#if page.next}` works). A URL is the page's
  destination with `…/index.html` written as `…/` and each segment
  percent-encoded, as SITEMAP writes paths. The destination is evaluated per
  page (it usually uses `${page.number}`); `FIRST <dest>` gives page 1 its own
  destination (`/index.html` rather than `/page/1/index.html`). Destinations
  are computed before the URLs, so they cannot use `url`, `prev` or `next`
  (`E_UNDEFINED`). An empty list gives one page with no items (so an index
  page always exists) and no warning.
- **Both: `FOR tag IN list BY key PAGE <n> [FIRST <dest>]`** paginates each
  group: `tag` is a page with the group's `key` too (`{ key, count, items,
  number, total, url, prev, next }`, `count` being the group's size), and
  `prev`/`next` stay within the group.
  `FOR t IN posts BY tags PAGE 10 FIRST /tags/${t.key}/index.html` with
  destination `/tags/${t.key}/page/${t.number}/index.html` gives every tag an
  index and numbered older pages.
- The clauses come in this order after `FOR item IN list`: `BY`, then `PAGE`,
  then `FIRST` (only after `PAGE`). `FIRST` may use the loop variable like the
  destination; `PAGE`'s size may use ARG/LET values only. They apply to
  `RENDER`, not `TRANSFORM … FOR`.
- **Elsewhere**: FEED reads the import itself (drafts left out, every element,
  not pages). Incremental rebuilds memoize each group or page as one element:
  its key is the element value (items, numbers and URLs included), so a page
  re-renders exactly when what it shows changes, and editing one post re-renders
  its own page, its tag pages and the index pages that list it. `graph` labels
  the operation `RENDER ./tag.html FOR tag IN posts BY tags PAGE 10`; lint's
  `render-escape` checks the destination as written.

### Schemas: `IMPORT … SCHEMA <file>` (see `examples/blog/`)
Like Astro's content collections, an import of documents can declare what their
fields must look like, so a typo in a post's frontmatter fails the build instead
of rendering an empty title:

```
IMPORT posts FROM ./posts AS markdown SORT BY date DESC SCHEMA ./post.schema.yaml
```

```yaml
# post.schema.yaml
required: [title, date]
properties:
  title: string
  date: date
  draft: boolean
  tags: {type: list, items: string, pattern: '^[a-z0-9-]+$'}
  status: {enum: [draft, published]}
```

- **Where**: `SCHEMA <file>` is a clause of `IMPORT … AS markdown|json|yaml|csv`
  (`E_VALIDATION` for other kinds), given at most once, in any order with the
  other clauses. `<file>` is an operand (`${NAME}` works), a context-relative
  `.yaml`, `.yml` or `.json` file (`E_VALIDATION` otherwise; a URL is
  `E_PROTOCOL`; the usual local sandbox applies). YAML schemas use the YAML
  subset of `AS yaml`; JSON ones are parsed as JSON.
- **The schema** is a small, deterministic subset of JSON Schema, the same in
  both implementations: a mapping with
  - `required`: a list of keys every document must have. A key whose value is
    `null` counts as missing, and so does an empty CSV cell.
  - `properties`: a mapping from a key to a rule, either a type name
    (`title: string`) or a mapping with any of
    - `type`: `string`, `number`, `boolean`, `date` (a string that is
      `YYYY-MM-DD` or an RFC 3339 date-time, exactly what FEED accepts, so an
      impossible date such as `2026-02-30` fails) or `list`;
    - `items` (with `type: list`): the type of every element (`string`,
      `number`, `boolean` or `date`);
    - `enum`: a non-empty list of allowed strings, numbers or booleans; for a
      `list` value, the allowed elements.
    - `pattern`: a regular expression the value (for a `list`, each element)
      must match somewhere, like JSON Schema's `pattern` and JavaScript's
      `RegExp.test`; anchor it (`'^[a-z0-9-]+$'`) to match the whole value.
      Only strings can match (`it should be a string matching the pattern …`),
      so in a CSV import every cell is checked as text. Checked after `type`
      and `enum`. Write it single-quoted in YAML so backslashes stay as typed.
  Keys not listed are not checked, and a listed key that is missing (or `null`)
  is only an error when it is `required`. There are no other keywords (no
  nested objects); an unknown key, type or malformed rule is `E_SCHEMA` at the
  `SCHEMA` clause, naming the schema file and the property.
- **Patterns** are a subset of JavaScript regular expressions (with the `u`
  flag) that both implementations run with one small engine of their own
  (`src/pattern.ts`, `rust/src/pattern.rs`; no regex dependency), so they
  accept the same patterns and match the same strings. Every pattern they
  accept is a valid JavaScript `u` regex with the same meaning:
  - literals: any character other than `^ $ \ . * + ? ( ) [ ] { } |`;
  - `.`: any character except `\n`, `\r`, U+2028 and U+2029;
  - classes `[abc]`, `[a-z]`, `[^…]` (`-` is literal first or last; `[]`
    matches nothing, `[^]` anything);
  - `\d` (`[0-9]`), `\w` (`[A-Za-z0-9_]`), `\s` (JavaScript whitespace) and
    their complements `\D \W \S`; `\n \r \t`; a `\` before any of
    `^ $ \ . * + ? ( ) [ ] { } | /` (and `-` in a class) is that character;
  - repetition `* + ? {n} {n,} {n,m}` (counts up to 1000; a trailing `?`
    makes it lazy, which changes nothing here), groups `( … )` and
    `(?: … )`, alternation `|`, and the anchors `^` and `$` (start and end of
    the value; there is no multiline mode).
  Characters are Unicode code points, and matching is case-sensitive. Anything
  else (`\b`, back-references, lookaround `(?=`, named groups, `\u…`, flags,
  a lone `{`, `}` or `]`) is `E_SCHEMA` naming the pattern, the position (in
  characters) and the reason:
  `IMPORT posts: schema './post.schema.yaml' property 'slug': pattern '[a-' is invalid at character 1: unterminated character class`.
  Matching runs a Thompson NFA, so it takes time linear in the value's length
  whatever the pattern (no catastrophic backtracking). The two engines are
  checked against shared vectors (`rust/tests/fixtures/pattern/vectors.json`),
  random patterns (`fuzz.json`), and JavaScript's own `RegExp`.
- **What is checked**: each element of a list import (every Markdown document,
  each element of a JSON/YAML array or directory collection, each CSV row), or
  the single document of a one-file import. An element that is not an object is
  `E_TYPE`. Markdown documents are checked as templates see them, so a `date`
  taken from the file name satisfies `required: [date]`. CSV cells are always
  strings, so in a CSV import `number` also accepts a numeric string (as
  `SORT BY` reads them), `boolean` the strings `true` and `false`, and `enum`
  entries match their text.
- **Errors**: the first mismatch fails the build with `E_SCHEMA` at the IMPORT
  line, naming the document (its `path` inside the import, or `#n` for the n-th
  element of an array or CSV file), the key, what was expected and what was
  found:
  `IMPORT posts: document '/2026-01-05-hello.md' has no 'title' (required by schema './post.schema.yaml')`,
  `IMPORT posts: document '/a.md' has 'date' string 'soon'; it should be a date (YYYY-MM-DD or an RFC 3339 date-time)`,
  `IMPORT posts: document '/a.md' has 'tags' item 2 number 3; it should be a string`,
  `IMPORT items: document '#2' has 'size' string 'xl'; it should be one of s, m, l`,
  `IMPORT posts: document '/a.md' has 'handle' string 'Hello World'; it should match the pattern '^[a-z0-9-]+$'`.
  Imports are checked in Sitefile order and documents in path order (before
  `SORT BY`), after every import has been read.
- Checking happens whenever the imports are read (`build`, `serve`, `inspect`,
  `why`), not in `validate`, which never reads files. The schema file is
  read on every build (also in watch mode, where a change to it triggers a
  rebuild like any other file); `graph` shows it as a `local:` input of the
  import.

### Plugins: PLUGIN and TRANSFORM (see `docs/PLUGINS.md`, `examples/motion/`)
- `PLUGIN name FROM ./plugins/x.mjs|x.wasm [INTEGRITY sha256:…]` declares a renderer:
  a local `.mjs` module in its own folder (not the context root), or a
  WebAssembly module with no imports (anywhere; see docs/PLUGINS.md). URLs fail
  with `E_PROTOCOL`. Plugin names share the ARG/LET/IMPORT/stage namespace.
- `TRANSFORM [--overwrite] [--from=stage] [--include=<pattern>…] [--exclude=<pattern>…]
  [<source>] WITH name TO <dest> [OPTIONS key=value …] [FOR item IN list]` sends
  the source (a file, an inline body, or a value import) and the options to the
  plugin and writes what it returns: one file at `<dest>`, or several under
  `<dest>/`. Without a source, the plugin gets the `FOR` element, or only its
  options. `<dest>` and option values may use `${item.key}`.
- **A directory source** (a context directory, `name:` / `name:/dir` of a tree
  import, or `--from=stage /dir`) calls the plugin **once per file**, in path
  order (UTF-8 bytes), each call with its own `input` (`input.path` is the
  source label plus the file's relative path, e.g. `./photos/trip/a.jpg`):
  `TRANSFORM ./photos/ WITH image TO /img/ OPTIONS widths=400,800`. Local
  directories honour `.sitefileignore`, like COPY.
  - **Destinations.** By default each file gets the directory
    `<dest>/<relative dir>/` (`./photos/trip/a.jpg` with `TO /img/` gets
    `/img/trip`; a trailing `/` on `<dest>` is optional) and the plugin names
    the files, as for one file with a directory destination. A last segment
    `*` writes one file per input under its own relative path (`TO /css/*`);
    `*.ext` replaces its extension (`TO /thumbs/*.png` writes
    `/thumbs/trip/a.png`; `a.tar.gz` becomes `a.tar.png`, a name without an
    extension gets one), so the plugin sees a file destination and picks the
    format from it. A `*` destination with a source that is not a directory is
    `E_TYPE`.
  - **`--include=<pattern>`** keeps only the files some pattern matches and
    **`--exclude=<pattern>`** leaves files out (both repeatable,
    `.sitefileignore` lines relative to the source directory, as for COPY's
    `--exclude`): `TRANSFORM --include=*.jpg --exclude=drafts/ ./photos/ …`.
    With a file source or a value import they are `E_TYPE`; without a source,
    with an inline body or with `FOR` they are `E_SYNTAX`. A directory with no
    files left is a warning.
  - A directory source with `FOR` is `E_TYPE` (each file is already one call).
    A `TOOL` writes one file per input, so it needs `TO <dir>/*` or
    `TO <dir>/*.ext` (`E_PLUGIN` otherwise).
  - Calls run up to 4 at a time (wasm plugins and tools; a JavaScript plugin's
    one process takes one call at a time); the files, warnings, `--profile`
    events (one per file, with an `input` arg) and errors come in path order
    either way. A failing file fails the build with its error prefixed by the
    file (`./photos/notes.txt: PLUGIN image: not a PNG, JPEG, GIF or WebP
    image`); the first failing file in path order is the one reported.
  - Each file is cached and memoized on its own: the cache key is the one a
    single-file TRANSFORM of that file to the same destination has (so they
    share results), and a `--watch` rebuild re-runs only the files that
    changed. Each output's origin names its input (`Sitefile:9 TRANSFORM
    ./photos/a.jpg`), which `sitefile why` shows.
  - Two files may write the same path only with identical content
    (`./photos/a.jpg` and `./photos/a.png` both writing `/img/a.json` is
    `E_CONFLICT` naming both, `--overwrite` or not); conflicts with files
    already in the tree follow the usual rules.
- Plugins run in a separate Node process with `--permission`: read access to
  their own folder only; no writes, processes, workers, network or
  environment; a per-call time limit (`--plugin-timeout`). Failures are
  `E_PLUGIN`. This protects against accidents, not deliberately malicious code:
  pin plugins with `INTEGRITY` (for a `.mjs` plugin, the digest of its whole
  folder, which it can read; for a `.wasm` plugin, of the module; see
  docs/PLUGINS.md, "What `INTEGRITY` pins").
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
- Images: the first-party wasm plugin `plugins/image/image.wasm` resizes and
  converts PNG, JPEG, GIF and WebP (`TRANSFORM ./a.jpg WITH image TO /img/
  OPTIONS widths=400,800`) and writes a metadata file with each variant's
  width, height and a `srcset`. See `docs/PLUGINS.md` ("The image plugin")
  and `examples/image/`.

### Site files: SITEMAP, FEED, ROBOTS, SECURITY_TXT, REDIRECT and HEADER (see `examples/blog/`, `examples/robots/`)
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
  Above the [protocol's limits](https://www.sitemaps.org/protocol.html) of
  50,000 URLs or 50 MiB (52,428,800 bytes) the destination becomes a
  **sitemap index** (`<sitemapindex>`, one `<sitemap><loc>` per part) and the
  URLs go to numbered parts next to it, `/sitemap-1.xml`, `/sitemap-2.xml`, …
  (`-N` before the last extension), in path order; a part is closed when the
  next URL would pass either limit. Search engines and `ROBOTS … SITEMAP`
  then use the index.
- `CANONICAL BASE <url> [PRETTY]` adds `<link rel="canonical" href="…">`
  right before the first `</head>` of every `.html`/`.htm` page of the
  **current stage's tree at that point** that is below the EXPORT path (other
  than `/404.html` there), is valid UTF-8 and has no `<link>` whose `rel`
  has the token `canonical` before that `</head>` (yours win). The URL is
  `<base>` and the page's path below the EXPORT path, encoded like a sitemap
  entry (`…/index.html` becomes `…/`); `PRETTY` also drops `.html` (`/about.html`
  → `<base>/about`), for hosts that serve pages without the extension. A page
  with no `</head>` is left alone, and one warning names those pages. Comments
  and the contents of `<script>` and the other raw-text elements are skipped,
  as when references are scanned. The rewritten pages get the origin
  `Sitefile:<line> CANONICAL`. With LANGUAGES (below) it also writes
  `hreflang` alternates.
- `LANGUAGES <code>… [DEFAULT <code>]` (see `examples/hreflang/`) says which
  pages are translations of each other, so that CANONICAL and SITEMAP write
  [hreflang alternates](https://developers.google.com/search/docs/specialty/international/localized-versions).
  It writes nothing itself; without CANONICAL or SITEMAP it has no effect.
  - Each code (an operand) must be a basic BCP 47 tag: 2-3 letters, then
    `-` subtags of 1-8 letters or digits (`en`, `fr`, `pt-BR`, `zh-Hant-TW`);
    anything else, a code listed twice (ASCII case-insensitive), a `DEFAULT`
    that is not one of the codes, or a second LANGUAGES line in the Sitefile
    is `E_VALIDATION`. It applies to every CANONICAL and SITEMAP of the
    Sitefile, also those before it.
  - **Folders.** The codes are folder names: `/en/about.html` and
    `/fr/about.html` are the `en` and `fr` pages of `/about.html` (the first
    path segment must equal the code exactly). A page in no language folder
    is the `DEFAULT` language's page of its own path, so with `DEFAULT en`
    `/about.html` and `/fr/about.html` translate each other; without
    DEFAULT it has no language. The path is the one each instruction uses for
    the page: below the EXPORT path for CANONICAL, the tree path for SITEMAP.
  - **Documents.** Pages written by a `RENDER … FOR item IN list` (without BY
    or PAGE) earlier in the same stage, whose element has a `translationKey`
    (text or a number) and a text `lang`, are grouped by that key in that
    language instead, whatever their folders, so translations can have
    different slugs (`/blog/hello.html` and `/blog/bonjour.html`). A `lang`
    that is not a language code is `E_VALIDATION`; an element with only one
    of the two fields is ignored (the folder rule applies to its page).
  - A group of two or more pages gives each of them one alternate per page
    of the group (itself included), ordered by language (ASCII
    case-insensitive), then `x-default` for the DEFAULT language's page when
    the group has one. A page alone in its group gets none. Two pages of
    one language in a group (`/about.html` and `/en/about.html` with
    `DEFAULT en`, or two documents with one key and one `lang`) are
    `E_VALIDATION`, naming both.
  - CANONICAL writes `<link rel="alternate" hreflang="<code>" href="…">`
    elements right after its canonical link (the URLs as for the canonical
    link, so `PRETTY` applies), into every page of a group that has no
    `<link>` with the `rel` token `alternate` and an `hreflang` attribute
    before its `</head>` (yours win). A page that keeps its own canonical
    link still gets the alternates.
  - SITEMAP adds `<xhtml:link rel="alternate" hreflang="<code>" href="…"/>`
    elements to the entries of those pages (the URLs as for `<loc>`), and
    `xmlns:xhtml="http://www.w3.org/1999/xhtml"` to every `<urlset>` when any
    entry has one. The split into a sitemap index counts the bytes of these
    elements too.
- `BASE` (SITEMAP and FEED) must be an absolute `http://` or `https://` URL
  with a host and no credentials, query, fragment, whitespace or control
  characters (`E_VALIDATION` otherwise); trailing slashes are dropped, so
  `https://example.com/blog/` and `https://example.com/blog` are the same.
- `FEED [--overwrite] <import> BASE <url> LINK <path> [TO <dest>] [TITLE <text>] [FOR <var> IN <import> BY <key>]`
  (clauses in this order; `FOR`: a feed per group, see Tag pages above) writes an [Atom 1.0](https://www.rfc-editor.org/rfc/rfc4287)
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
- `ROBOTS [--overwrite] [ALLOW <path> | DISALLOW <path>]… [SITEMAP] [TO <dest>]`
  (clauses in this order; ALLOW and DISALLOW repeat, mixed) writes a
  [robots.txt](https://www.rfc-editor.org/rfc/rfc9309) (default `/robots.txt`)
  with one group for every crawler: `User-agent: *`, then an `Allow:` or
  `Disallow:` line per rule in source order, or an empty `Disallow:` (allow
  everything) when there are none. Paths are absolute, without spaces or
  control characters (`E_VALIDATION`); `*` and `$` pass through as robots.txt
  wildcards. `SITEMAP` adds, after a blank line, a `Sitemap: <url>` line for
  every SITEMAP instruction of the Sitefile, in source order and also those
  after the ROBOTS line (`<base><dest>`, as the sitemap writes its own
  locations; duplicates once); with no SITEMAP it is `E_VALIDATION`. Like
  SITEMAP it writes the file at its place in the stage:

  ```text
  User-agent: *
  Disallow: /drafts/

  Sitemap: https://example.com/sitemap.xml
  ```
- `SECURITY_TXT [--overwrite] CONTACT <uri>… EXPIRES <date> [ENCRYPTION <uri>]… [POLICY <uri>]… [TO <dest>]`
  (clauses in any order; CONTACT, ENCRYPTION and POLICY repeat) writes an
  [RFC 9116](https://www.rfc-editor.org/rfc/rfc9116) file (default
  `/.well-known/security.txt`): the `Contact:` lines, `Expires:`, then the
  `Encryption:` and `Policy:` lines, in source order. CONTACT and EXPIRES are
  required (`E_VALIDATION`, as RFC 9116 requires both). A CONTACT is a
  `mailto:`, `tel:` or `https://` URI, an ENCRYPTION an `https://`,
  `openpgp4fpr:` or `dns:` URI, and a POLICY an `https://` URL, none with
  spaces (`E_VALIDATION`). EXPIRES is a date (`YYYY-MM-DD`, written as
  `…T00:00:00Z`) or an RFC 3339 date-time, as FEED reads dates; a duration
  (`P1Y`, `365d`) is not accepted, since it would make the file depend on the
  day of the build. A date in the past, or less than 30 days away, prints a
  warning (validate and build). The file is not signed.
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
    `/blog/:year/:slug`, `/blog/*` and `:splat` is passed through as written;
    `sitefile serve` understands placeholders and a trailing `/*`
    (docs/CLI.md), and `validate` warns about a line it would ignore
    (`serve-ignored`). `EXPORT … AS
    wbn` also bakes exact-path REDIRECTs into 3xx responses and HEADER values
    into each response (see **wbn** under Export formats).
  - `HEADER /assets/* Cache-Control: public, max-age=3600`: the name is the
    word ending in `:` (`E_SYNTAX` if there is none), the value is the rest of
    the line, its words joined with one space and trimmed. The name must be an
    HTTP token (`E_VALIDATION`), and the value must be non-empty and contain no
    control characters other than tab, so CR/LF cannot inject headers. The
    pattern is an absolute path like `from`. `/_headers` has one block per
    pattern, in order of first use: the pattern, then `  Name: value` lines in
    source order.
  - `MIME <.ext> <type/subtype>` (see `examples/mime/`) sets the `Content-Type` of the files with
    that extension, for types a host doesn't know (`.usdz`, game data) or
    gets wrong (`.ts` as MPEG-TS): it adds the rule `/*<.ext>` with
    `Content-Type: <type/subtype>` to `/_headers`, in source order with the
    HEADER lines (so `sitefile serve`, Netlify and Cloudflare Pages use it), and
    `EXPORT … AS wbn` gives those files that content-type, since a Web Bundle
    takes `content-type` from `/_headers`. The extension is a dot and
    letters, digits, `+`, `-` or `_`, the type an RFC 6838 `type/subtype`
    without parameters (`E_VALIDATION` otherwise); both are written in
    lowercase. An extension given twice is `E_VALIDATION`. With only MIME
    lines, `/_headers` has the origin `Sitefile:<line> MIME`.
  - If the tree already has `/_redirects` or `/_headers` (from a COPY, say),
    the normal conflict rules apply: identical bytes are fine, different
    content is `E_CONFLICT` naming that COPY, unless one of the REDIRECT (or
    HEADER) lines has `--overwrite`, which replaces the file.

### Fingerprinting: FINGERPRINT (see `examples/blog/`)
`FINGERPRINT [--overwrite] [--no-cache-headers] <pattern>… [MANIFEST <dest>] [INTEGRITY]`
renames assets to content-hashed names (`/style.css` becomes
`/style.3f2a9c1b0d.css`) and rewrites the references to them, so a host can
serve them with `Cache-Control: public, max-age=31536000, immutable` (EXPORT
writes those headers to `/_headers`, see below). Like SITEMAP it runs
**at its place in the stage** and reads the stage's tree at that point, so put
it after the pages and assets are written. It is an instruction rather than an
EXPORT clause because the result is an ordinary tree: later instructions (a
SITEMAP, a COPY into another stage, `--target`) see the renamed files, and
`why` can show the rename.

- **Patterns** (operands, so `${VAR}` works; at least one) are absolute paths
  matched against the tree's paths segment by segment: `*` matches any run of
  characters within a segment, `?` one character, and a `**` segment any number
  of segments (also none); everything else is literal and case-sensitive. A
  trailing `/` means everything below: `/assets/` is `/assets/**`. A pattern
  that does not start with `/` is `E_VALIDATION`. `FINGERPRINT /assets/ /**/*.css`.
- **Never renamed**: HTML pages (`.html`, `.htm`), files named `_redirects` or
  `_headers`, and the MANIFEST file. Patterns that match nothing else print a
  warning (`Sitefile:9 FINGERPRINT: no files match /assets/**`).
- **Names**: `name.<hash>.ext`, with the hash before the last extension
  (`app.min.js` → `app.min.<hash>.js`), or after the name when there is none
  (`LICENSE` → `LICENSE.<hash>`, `.env` → `.env.<hash>`). The hash is the first
  **10** lowercase hex digits of the SHA-256 of the file's final content, so the
  same content always gets the same name, in both implementations.
- **CSS first**: a matching CSS file has its own references to other matching
  files rewritten *before* it is hashed, so its name changes when an image or
  font it uses changes. Files are therefore hashed in dependency order; a cycle
  of such references (`a.css` imports `b.css`, which imports `a.css`) is
  `E_VALIDATION` naming the cycle, since no name could include the hash of the
  others. Other files are hashed as they are (JavaScript is not scanned).
- **References**: every `.html`, `.htm` and `.css` file of the tree (renamed or
  not, valid UTF-8) has each reference to a renamed file rewritten. The
  scanner (`src/links.ts`, ported as `rust/src/links.rs`, identical results;
  a differential fixture checks it) finds:
  - in HTML: `href` (every element but `<base>`), `src`, `srcset` and
    `imagesrcset` (each candidate URL), `poster`, `xlink:href` and `data` on
    `<object>`; the CSS of `<style>` elements and `style` attributes. Comments,
    `<!…>`/`<?…>`, end tags and the contents of `<script>`, `<textarea>`,
    `<title>` and `<xmp>` are skipped. Attribute values have `&amp;` `&lt;`
    `&gt;` `&quot;` `&apos;` and numeric character references decoded (others
    stay as written) and are trimmed of ASCII whitespace. `<base href>` is not
    honoured.
  - in CSS: `url(…)` (quoted or not) and the string after `@import`; comments
    and other strings are skipped, CSS escapes are decoded.
  - Ignored: empty URLs, `#fragment`s and `?query`s of the same document,
    `http(s):` and `//host` URLs, and every other scheme (`mailto:`, `data:`,
    `tel:`, `javascript:`).
  - A URL resolves against the file that contains it (percent-decoded, NFC,
    `.`/`..` collapsed); an absolute path (`/img/a.png`) is a URL path of the
    exported site, so it resolves under the **EXPORT path** (`EXPORT /www`:
    `/www/img/a.png`), whatever the stage, and `..` never climbs above it
    (as in a browser). A URL ending in `/` names a directory and is never
    rewritten. Matching is exact: `IMG/LOGO.PNG` does not name `/img/logo.png`.
  - **Rewriting** inserts `.<hash>` into the name *as written*, before its
    extension's dot, and keeps everything else (directories, `?query`,
    `#fragment`, quotes, escapes, `&#…;` references, percent-encoding):
    `href="css/site.css?v=1"` → `href="css/site.3f2a9c1b0d.css?v=1"`,
    `srcset="img/logo%402x.png 2x"` → `img/logo%402x.<hash>.png 2x`. A name
    whose extension's dot is percent-encoded (`a%2Ecss`) becomes a relative
    URL to the new file instead.
- **Moves and conflicts**: each renamed file leaves its old path and keeps its
  mode; renamed and rewritten files get the origin `Sitefile:<line> FINGERPRINT`.
  A new name that already holds different content is `E_CONFLICT` unless
  `--overwrite` (identical content is fine).
- **`MANIFEST <dest>`** (a file path) writes a JSON object from each old path
  to its new path, sorted by path (UTF-8 bytes), two-space indented, with a
  final newline (`{}` when nothing was renamed), under the same conflict rules:

  ```json
  {
    "/css/site.css": "/css/site.3f2a9c1b0d.css"
  }
  ```

  It is the hook for later steps that need the names.
- **`INTEGRITY`** adds Subresource Integrity to the HTML: every `<script>`
  with a `src` and every `<link>` with an `href` whose `rel` has the token
  `stylesheet` or `modulepreload` (ASCII case-insensitive), in every `.html`
  and `.htm` file of the tree, that loads a file this FINGERPRINT renamed gets
  ` integrity="sha384-<base64>"` (the SHA-384 of the renamed file's final
  bytes) inserted right after the tag name: `<script src="app.js">` →
  `<script integrity="sha384-…" src="app.<hash>.js">`. A tag that already has
  an `integrity` attribute is left alone, and so are other elements
  (`<img>`, `<link rel=icon>`, `rel=preload`). No `crossorigin` is added:
  fingerprinted files are files of the site, so they are same-origin, where
  SRI needs no CORS. Tags are found by the same scanner as references
  (comments and raw text are skipped).
- **Cache headers**: when the exported stage holds files a FINGERPRINT renamed,
  EXPORT adds `Cache-Control` lines to `/_headers` after the HEADER lines:
  `public, max-age=31536000, immutable` for the renamed files, and `no-cache`
  for the pages, so a deploy never serves a stale page that names old assets.
  Each renamed file is covered by a `/dir/*` rule for the topmost directory
  below the root that holds only renamed files (`/assets/*`), or else by its
  own URL; pages by `/` (when `/index.html` exists), `/*/` (when a deeper
  `index.html` exists) and `/*.html`. Pages a host serves without `.html`
  (pretty URLs such as `/about`) are not matched; add a `HEADER` for them.
  URLs with spaces or control characters are left out. The lines are not
  written when any FINGERPRINT of the Sitefile has `--no-cache-headers`, when
  a `HEADER` sets `Cache-Control` itself (yours win; nothing is mixed), or
  with `--target`. When there are no HEADER lines and the tree already has a
  `/_headers` (COPY'd in), it is kept as it is and a warning says the lines
  were not added. With only these lines, `/_headers` has the origin
  `Sitefile:<line> FINGERPRINT` (the first FINGERPRINT), and `why /_headers`
  lists that FINGERPRINT among its inputs.

  ```text
  /assets/*
    Cache-Control: public, max-age=31536000, immutable
  /
    Cache-Control: no-cache
  /*.html
    Cache-Control: no-cache
  ```
- **Site files**: a `HEADER` pattern or a `REDIRECT` target that is exactly the
  URL of a file renamed in the exported stage (or the stages it is built
  `FROM`) is written to `/_headers` / `/_redirects` with the new URL
  (`HEADER /css/site.css Cache-Control: …` → `/css/site.<hash>.css`); patterns
  with wildcards are left as written, and a file renamed twice gets its last
  name. A `/_redirects` or `/_headers` COPY'd into the tree is not rewritten.
  A SITEMAP lists only HTML pages, which are never renamed, so it stays valid
  wherever it runs.
- **Elsewhere**: `graph` labels the operation `FINGERPRINT /assets/** …` (it
  reads the stage's tree, so it has no input edges); `why` on a renamed file
  shows the history of its old path, then `FINGERPRINT …  wrote <new path>`
  from `<old path> (renamed)`; a page whose references were rewritten shows
  `overwrote` from `references to renamed files`. In watch mode an unchanged
  FINGERPRINT step is reused only when the whole tree before it is unchanged
  (like SITEMAP), and its warnings are printed again. `--target` stops before
  a later stage's FINGERPRINT like any other operation.

### Precompressed sidecars: PRECOMPRESS (see `examples/precompress/`)
`PRECOMPRESS [--overwrite] [gzip] [br] <pattern>… [MIN <bytes>]` writes a gzip
sidecar `<file>.gz` and/or a Brotli sidecar `<file>.br` next to each matching
file, for servers that send it in place of the file to clients that accept it (nginx `gzip_static`, Caddy
`file_server precompressed`, sirv, http-server `--gzip`). Like FINGERPRINT it
runs **at its place in the stage** on the tree at that point, so put it after
the files it compresses (and after a FINGERPRINT, so the hashed names get
sidecars).

- **Patterns** work as FINGERPRINT's (absolute, `*`, `?`, `**`, a trailing `/`
  for everything below; `PRECOMPRESS /` is the whole tree). Patterns that
  match nothing print a warning.
- **Which files**: only compressible types, by extension (ASCII
  case-insensitive): `html htm css js mjs cjs json map xml svg txt md csv tsv
  webmanifest rss atom ics vtt yaml yml toml wasm ttf otf eot ico`. Already
  compressed formats (images other than SVG, `woff2`, video, archives) are left
  alone. A file needs at least `MIN` bytes (an operand, a whole number;
  default **1024**), and its sidecar is written only when it is smaller than
  the file.
- **Formats**: `gzip` (the default) and `br`, in any order, each at most once
  (`PRECOMPRESS br gzip /` writes both). Each sidecar is written only when it is
  smaller than the file. `zstd` is `E_UNSUPPORTED`: sitefile has no Zstandard
  encoder that writes the same bytes in both implementations.
- **Deterministic**: the sidecar is a gzip member with a fixed header (no
  modification time, no file name) around the shared deflate encoder
  (`src/deflate.ts`, `rust/src/deflate.rs`) that zip, tar.gz and OCI layers use,
  so both implementations write the same bytes on every run. A `.br` sidecar is
  Brotli at quality 11 (the smallest output, the usual choice for static
  precompression; it runs at a few MB per second, so a build with many large files
  takes longer), window 22 (4 MiB) and generic mode, from libbrotli 1.2.0: Node's
  own in TypeScript, the same C code vendored in the Rust port (docs/RUST.md,
  "Compression").
- Sidecars keep the file's mode and get the origin `Sitefile:<line> PRECOMPRESS`;
  `why` shows `<file> (compressed)` as the input. A sidecar path that already
  holds different content is `E_CONFLICT` unless `--overwrite`. In watch mode an
  unchanged step is reused only when the whole tree before it is unchanged (like
  SITEMAP).

### Precache manifest: PRECACHE (see `examples/precache/`)
`PRECACHE [--overwrite] <pattern>… [TO <dest>] [INTO <service-worker>]`
(clauses in this order) lists files for a service worker to precache, in the
format Workbox's `precacheAndRoute` reads (`injectManifest`). It runs **at
its place in the stage** on the tree at that point, so put it after the files
and after any FINGERPRINT.

- **Patterns** work as FINGERPRINT's (absolute, `*`, `?`, `**`, a trailing `/`
  for everything below); patterns that match nothing print a warning.
- **Entries**: every matching file below the EXPORT path (other than the
  manifest and the service worker themselves), sorted by URL: `url` is its
  path below the EXPORT path, percent-encoded like a rewritten reference
  (`/img/a%20b.png`), and `revision` the first 10 hex digits of its SHA-256,
  or `null` for a file a FINGERPRINT of the stage (or the stages it is built
  `FROM`) renamed, since its name already changes with its content.
- **`TO <dest>`** (default `/sw-manifest.json`) writes the JSON array, one
  entry per line:

  ```json
  [
    {"url": "/index.html", "revision": "3f2a9c1b0d"},
    {"url": "/style.5e1b0c2a9f.css", "revision": null}
  ]
  ```
- **`INTO <service-worker>`** replaces the first `self.__WB_MANIFEST` in that
  file of the stage (UTF-8 text) with the same array, as `workbox
  injectManifest` does; a missing file or placeholder is `E_VALIDATION`. With
  only INTO, no manifest file is written. sitefile writes no service worker of
  its own.
- The manifest follows the conflict rules (`--overwrite`); the service worker
  is replaced. Both get the origin `Sitefile:<line> PRECACHE`.

### Content Security Policy: CSP (see `examples/csp/`)
`CSP [POLICY <policy>] [META]` gives every exported page a
[Content-Security-Policy](https://developer.mozilla.org/docs/Web/HTTP/CSP)
that allows its own inline `<script>` and `<style>` elements by hash, so the
policy needs no `'unsafe-inline'`. Like HEADER it may appear in any stage
(once: a second CSP is `E_VALIDATION`) and is applied **at EXPORT**, to the
final pages of the exported directory.

- **Base policy**: `POLICY <policy>` (one operand, so quote it; no control
  characters or commas, `E_VALIDATION`), default
  `default-src 'self'; base-uri 'self'; object-src 'none'`.
- **Hashes**: for each `.html`/`.htm` page (valid UTF-8), the SHA-256 (base64)
  of the exact text of every `<script>` without a `src` and every `<style>`,
  found by the reference scanner (comments and other raw text skipped). The
  `'sha256-…'` sources, deduplicated in page order, are appended to the base's
  `script-src` / `style-src` directive, or added as `script-src 'self' …` /
  `style-src 'self' …` when the base has none and the page has such
  elements. Directives are written joined with `; `. Inline event handlers
  (`onclick=`), `style=` attributes and `javascript:` URLs are not hashed (CSP
  would need `'unsafe-hashes'`), so a strict policy blocks them; so is the
  live-reload script `serve --watch` injects (johnhenry/sitefile#181).
- **Where**: by default as `Content-Security-Policy` lines in `/_headers`
  after the HEADER lines, one block per page URL: `/a/index.html` as `/a/` and
  `/a/index.html`, `/b.html` as `/b.html` and `/b` (pretty URLs), other pages
  by their path (URLs with spaces are left out). A HEADER that sets
  `Content-Security-Policy` too is `E_VALIDATION`. With no HEADER lines and a
  COPY'd `/_headers` already in the tree, that file is kept and a warning says
  the lines were not added. With **`META`**, each page instead gets
  `<meta http-equiv="Content-Security-Policy" content="…">` right after its
  `<head>` start tag (pages without one are left alone, with a warning); a
  `<meta>` policy cannot set `frame-ancestors` or reporting.
- `--target` writes no policies (like the other EXPORT site files). `why` on a
  page or on `/_headers` shows the CSP line.

### Link checking: CHECKLINKS (see `examples/blog/`)
`CHECKLINKS [--warn] [--external]` checks every reference in the HTML and CSS
files of the stage's tree **at its place in the stage** (like SITEMAP and
FINGERPRINT; put it last, or after a FINGERPRINT to check the rewritten
names) and fails the build if one points at nothing. It is an instruction,
not a `build`/`validate` flag, so the check is part of the Sitefile (the same
in `build`, `serve --watch` and CI) and can be placed per stage; `validate`
only plans and never sees the files.

- **What is checked**: the references the FINGERPRINT scanner finds (see
  Fingerprinting: `href`, `src`, `srcset`, `poster`, … in HTML, `<style>` and
  `style=` CSS, `url()` and `@import` in CSS) in every `.html`, `.htm` and
  `.css` file (valid UTF-8), files in path order (UTF-8 bytes) and references
  in file order. Fragments, queries of the same page, `mailto:`, `data:` and
  other non-http schemes are skipped; `#fragment` targets are not checked.
- **Found** means served the way `sitefile serve` and static hosts serve it:
  the file itself, `<path>.html` (clean URLs: `/about` finds `/about.html`), or
  `<path>/index.html`; a URL ending in `/` needs its `index.html`. Absolute
  paths resolve under the EXPORT path, as for FINGERPRINT. A link a `REDIRECT`
  of the Sitefile covers (its source exactly, or a `/*` prefix: `/blog/*`) is
  not broken, wherever the REDIRECT line is.
- **Broken links** fail the build with `E_BROKEN_LINK`, listing up to 20 (then
  `… and N more`) as `<file>:<line>: <url> (<reason>)`:

  ```
  error[E_BROKEN_LINK]: Sitefile:12:1: CHECKLINKS: 2 broken links:
    /index.html:4: css/site.css (not found: /css/site.css)
    /docs/index.html:2: guide/ (not found: /docs/guide/)
  ```

  `<file>` is the tree path, `<line>` the line of the reference, `<url>` the
  URL as written (character references and escapes decoded).
- **`--warn`** prints one warning per broken link instead
  (`warning: Sitefile:12 CHECKLINKS: broken link /index.html:4: css/site.css (not found: /css/site.css)`)
  and the build goes on.
- **`--external`** also requests every `http(s)://` and `//host` link
  (`https:` is assumed), each URL once, in order of first use, one at a time,
  through the same client and **network policy** as a remote IMPORT: https
  only unless `--allow-http`, public addresses only unless
  `--allow-private-network`, every redirect hop checked, the fetch timeout.
  A link is broken when the request fails (`could not connect`, `no answer
  within 30000 ms`, `too many or invalid redirects`) or answers 400 or more
  (`HTTP 404`); a URL the policy refuses is reported as broken with the flag
  that would allow it (`plain http is checked only with --allow-http`). The
  response body is not read. Without `--external` nothing is requested, so a
  build stays offline and reproducible.
- **Elsewhere**: the step writes nothing (`why` never lists it); `graph`
  labels it `CHECKLINKS [--warn] [--external]`. In watch mode an unchanged step
  (the same tree before it) is not run again (no requests), and its warnings
  are printed again.

### Lint rules (`sitefile validate`)
Like `docker build --check`, `validate` (and the language server) warns about likely mistakes in a Sitefile that is
otherwise valid. Each warning names a stable rule id:
`warning: Sitefile:4: [render-escape] RENDER writes /index.html without --escape=html, so substituted values are not HTML-escaped (add --escape=html, or --escape=none if that is intended)`.
The line is the instruction's first line. Rules read only the Sitefile (no files, network or lockfile), and lint
warnings never fail `validate`; `build`, `inspect`, `graph` and `why` do not lint.

| Rule | Warns about |
| --- | --- |
| `unused-name` | an `ARG`, `LET`, `IMPORT`, `PLUGIN` or `TOOL` nothing refers to (`${NAME}` in any operand, `name:` sources, `FOR … IN`, `FEED`, `WITH`, names in inline `RENDER` bodies and in html fragments (an html fragment nothing uses is reported like any LET), `name+<stage>` for an `IMPORT … AS sitefile`). A template *file* (or one from an import or stage) can read any ARG, LET and value import, so with one in the Sitefile only `file`/`tree` imports, plugins and tools are reported. |
| `unpinned-import` | a remote `IMPORT` without `INTEGRITY` (`Sitefile.lock` pins it only after the first fetch). |
| `unpinned-image` | a registry `FROM image <ref>` (not a local-store image) or the `BASE` of `EXPORT … AS oci` without an `@sha256:` digest. |
| `render-escape` | a `RENDER` to a `.html`/`.htm` destination (a `FOR` destination as written) without `--escape=…`; an explicit `--escape=none` is taken as intended. |
| `sitemap-order` | a `SITEMAP` followed, in the same stage, by an instruction writing a `.html` file it therefore does not list. |
| `shadowed-write` | a `RENDER` (without `FOR`) or inline `COPY` whose file a later `RENDER`/inline `COPY` in the same stage replaces with `--overwrite`, so the first write has no effect. |
| `serve-ignored` | a `REDIRECT` or `HEADER` (without `${…}` in it) whose `_redirects`/`_headers` line `sitefile serve` would ignore, with the reason (e.g. `*` in the middle of a path, or a header the server sets itself such as `Content-Length`). |

The unnamed non-final stage warning (above) is a planner warning, printed by `build` too; it is not a lint rule.

**Disabling.** A comment `# sitefile-lint: disable=<rule>[,<rule>…]` anywhere turns the rules off for the whole
file; `# sitefile-lint: disable-next-line=<rule>[,…]` turns them off for the next instruction only (blank lines and
other comments in between are skipped). `all` names every rule. The name `sitefile-lint` is ASCII
case-insensitive, with spaces or tabs allowed around it, the `:` and the `=` (it shares the `sitefile` prefix with the
`# sitefile=1` version directive, and like any comment it is body text inside a heredoc). A `# sitefile-lint:`
comment that is neither form, or names an unknown rule, is itself a warning (`[lint-comment]`, which cannot be
disabled). On the command line, `validate --lint-disable unused-name,render-escape` (repeatable) does the same for
one run.

```
# sitefile-lint: disable=unpinned-import
IMPORT data FROM https://example.com/data.json AS json
# sitefile-lint: disable-next-line=render-escape
RENDER ./templates/trusted.html TO /index.html
```

### Portable paths
Names that work where you build but break elsewhere are reported when the
build exports, as warnings (`EXPORT /: …`), or with `build --strict-paths` as
one `E_PATH_PORTABLE` error that lists them all. The exported files are checked
(paths below the EXPORT path), in path order:

- **Windows**: a path segment with `<`, `>`, `:`, `"`, `|`, `?`, `*`, `\` or a
  control character, one that ends with a dot or a space, or a reserved device
  name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, also with
  `¹²³`, in any case and with any extension: `aux.html`, `NUL.txt`). Such files
  cannot be extracted from a zip or written by a Windows checkout:
  `'/a:b.txt' cannot be extracted on Windows: ':' is not allowed in a Windows file name`.
- **GitHub Pages**: when a file or folder name starts with `_` (other than
  `/_headers` and `/_redirects`, which other hosts read) and there is no
  `/.nojekyll`, one message names up to three of them: Jekyll, which GitHub
  Pages runs by default, does not publish them.
- **Case**: two paths that differ only in case are already `E_PATH_COLLISION`
  (see below). A link whose target differs from a file only in case is a
  broken link for CHECKLINKS (matching is exact, as on a Linux host), and its
  message names the file: `not found: /Images/Logo.png; '/images/logo.png'
  differs only in case, which works on macOS and Windows but not on Linux hosts`.

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
  against its directory listing (macOS does this itself; on Linux sitefile
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
  URL (a Git import by a commit in its URL or its lockfile entry); a registry
  `FROM image` and the `BASE` image of an `oci` export (or `--base`) by
  `@sha256:` or a lockfile `images` entry. With `--no-lockfile` only the Sitefile's own pins
  count. Local imports never need one. `--strict-pins` conflicts with
  `--update-lock` (a usage error), since that would accept changed content.
  `inspect` and non-oci exports never resolve a `BASE`, so it is not checked
  there. `graph --strict-pins` checks like `validate`; `why` and `--target`
  builds check every `IMPORT` of the Sitefile (a target still resolves them all).
  `--cache-from`/`--cache-to` references are caches, not inputs, and are never
  checked. With `--provenance`, a strict build sets
  `externalParameters.strictPins: true` and gives each remote dependency an
  annotation `dev.sitefile.pin` (`integrity`, `lockfile`, or `digest` for an
  image); without `--strict-pins` the provenance is unchanged.
  There is no Sitefile-level switch yet (the flag needs no new syntax; the
  `# sitefile=N` directive only pins the language version). Typical CI use:
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
- **Protocol.** sitefile speaks Git's smart HTTP **protocol v2** itself; no `git` program runs, so
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
- **Verification.** The packfile is parsed by sitefile (zlib objects, `OFS_DELTA` and `REF_DELTA`,
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
- Private repositories need credentials, which URLs may not carry: use a build secret
  (`IMPORT … FROM git+https://… SECRET token AS BEARER`, or `AS HEADER Authorization` with a
  `Basic …` value); it is sent with every Git request to the repository's origin.

### Sitefile imports (`IMPORT … AS sitefile`)
A Sitefile can use the named stages of another Sitefile, local or in a Git repository, like Earthly targets
(issue #97), so a theme or a pipeline is written once and shared. See `examples/shared-stages/`.

```
ARG ACCENT = "rebeccapurple"
IMPORT theme FROM ./theme AS sitefile ARG ACCENT=${ACCENT}
IMPORT tools FROM git+https://github.com/me/site-tools.git#v1.2:stages AS sitefile

STAGE site FROM theme+assets
RENDER --escape=html --from=theme+layout /page.html TO /index.html
COPY --from=tools+icons / /icons/
EXPORT /
```

- **The import.** `IMPORT <name> FROM <dir> AS sitefile [ARG NAME=value]…` names another build: `<dir>` is its
  **context directory**, whose `Sitefile` is read. It is a context-relative directory (`./theme`; a file is
  `E_TYPE`, a directory without a `Sitefile` is `E_NOT_FOUND`) or a Git directory,
  `git+https://host/repo.git#ref:dir` (see Git sources above; the selection must be a directory). A plain
  `https://` URL is `E_PROTOCOL`: a published file tree is an image (`FROM image`), not a Sitefile. `INTEGRITY` is
  `E_VALIDATION` (a Git import is pinned by its commit); `SECRET` works as for any Git import; `SORT BY` and the
  other kinds' clauses do not apply. Like an `ARG`, an `IMPORT … AS sitefile` may come before the first `FROM`
  without starting the implicit `FROM scratch` stage, so the first stage can start from one of its stages.
- **Naming its stages.** `<name>+<stage>` is the final tree of the imported Sitefile's named stage `<stage>`,
  wherever a stage of this file can be read: `FROM theme+assets`, `STAGE site FROM theme+assets`,
  `COPY|RENDER|TRANSFORM --from=theme+assets`. `<name>` must be an `IMPORT … AS sitefile` on an earlier line
  (`E_UNDEFINED` for `--from=`; for a FROM base, `x+y` without such an import is still a context directory, as
  before). Whether `<stage>` exists is checked when the imported Sitefile is read:
  `FROM theme+nope: theme/Sitefile has no stage 'nope' (stages: assets, layout)` (`E_UNDEFINED`, at the
  instruction). Only the imported file's **named** stages are visible; its unnamed stages, its EXPORT and the
  stages of the Sitefiles it imports are not (re-export one with `STAGE x FROM inner+y`).
- **Namespacing.** The import's name is one name in this file's namespace (shared with ARG, LET, IMPORT and
  stages). Nothing else of the imported file (its ARGs, LETs, imports, stages) enters it, so names never clash.
  `COPY theme:` and `${theme}` do not read a Sitefile import (`E_TYPE`: read one of its stages with
  `--from=theme+<stage>`), nor do `FOR … IN theme` and `FEED theme` (`E_TYPE`).
- **ARGs.** The imported Sitefile's ARGs keep their defaults, except those set by the IMPORT's `ARG NAME=value`
  clauses (repeatable, any order among the other clauses; the value is an operand, so `${VAR}` of this file
  works). This build's `--build-arg`s never reach it. A clause naming no ARG of the imported file is
  `E_UNDEFINED` (`IMPORT theme: theme/Sitefile has no ARG NAME`); an ARG without a default and without a clause
  is `E_MISSING_ARG` (`IMPORT theme: ARG N of theme/Sitefile has no default; set it with ARG N=<value>`); the
  ARG's type applies (`IMPORT theme ARG N expects a number, got 'x'`). `ARG X` twice is `E_SYNTAX`; `ARG` on
  another kind of IMPORT is `E_VALIDATION`.
- **Paths and the sandbox.** The imported Sitefile's own paths resolve against **its** directory, which is its
  build context: `COPY ./assets/ /` in `theme/Sitefile` reads `theme/assets/`, with the sandbox rules of a context
  (`..` or a symlink leaving the directory is `E_PATH_ESCAPE`, even when the target is inside this build's
  context), its own `.sitefileignore`, and this build's export, vendor and `--profile` paths skipped. A Git
  import's directory is checked out to a temporary directory for the build (its symlinks resolved inside the
  tree, as for any Git import) and removed afterwards.
- **Messages and origins** name the imported file: `theme/Sitefile:3` for a local one (the path as this build's
  own Sitefile is named, plus the directory), `git+https://host/repo.git#<commit>:stages/Sitefile:3` for a Git
  one (the commit it was read at). Its files keep origins such as `theme/Sitefile:9 COPY`, which `inspect` and
  `why` show. Its planner warnings (unnamed stages) are printed too.
- **What may be imported.** Any valid Sitefile without `PLUGIN` or `TOOL` (`E_UNSUPPORTED` at that line: an
  imported Sitefile, possibly another repository's, cannot run code here). It may use stages, `FROM <dir>`,
  `FROM image`, local, https and Git imports, and other Sitefile imports. Its EXPORT, REDIRECT and HEADER lines
  are not used (they belong to its own export). `FINGERPRINT` and `CHECKLINKS` are stage operations, so they run
  in its stages like any other: a stage that FINGERPRINTs exposes the renamed files, a stage of the importer that
  starts `FROM theme+site` inherits its renames (so a HEADER or REDIRECT of the importer naming the old URL follows
  the rename, as for `FROM <stage>`), and `why` shows the rename in the imported Sitefile. `CHECKLINKS` checks the
  imported stage's tree against the imported file's own EXPORT path, with this build's network policy for
  `--external`; its warnings and `E_BROKEN_LINK`s name the imported file (`theme/Sitefile:6 CHECKLINKS`).
- **What is built.** Its IMPORTs are all resolved when it is read (as in a `--target` build), and its stages up to
  the last one this build reads are built, in order, once: a stage that only a later stage than `--target` reads
  is not built. Stages of different imports, and of the same Sitefile imported twice (with other ARGs, say), are
  built separately.
- **Nesting.** An imported Sitefile may import others; at most 8 levels (`E_VALIDATION`:
  `IMPORT x: Sitefile imports nest more than 8 levels deep`). Importing a Sitefile that is already being imported
  (itself with `./`, or the same repository commit and directory) is a cycle: `E_VALIDATION`,
  `IMPORT me: Sitefile import cycle: Sitefile -> Sitefile`.
- **Pinning.** There is one lockfile, this build's `Sitefile.lock`: a Git Sitefile import is a `git` entry (its URL
  as written and the commit), and the remote inputs of every imported Sitefile (its https and Git imports and
  registry `FROM image`s) are recorded there by URL and reference, like this file's own. An imported Sitefile's
  own `Sitefile.lock` is not read (like Cargo, only the top-level lockfile counts). `--frozen-lockfile`,
  `--update-lock` and `--no-lockfile` apply to all of them. `--strict-pins` checks the Git Sitefile import itself
  before anything is fetched (a commit in the URL or a lockfile entry), and each imported Sitefile's inputs when
  it is read, before they are fetched; `validate --strict-pins` and `graph --strict-pins` check only this file's.
- **Elsewhere.** `sitefile vendor` vendors a Git Sitefile import like any Git import (its tree) and, recursively,
  the remote inputs of every imported Sitefile, so an offline `--frozen-lockfile` build needs no network.
  `sitefile outdated` lists a Git Sitefile import like a Git import and then the remote inputs of the imported
  Sitefile (read at the pinned commit, or the ref's latest when unpinned), named `theme+data`, `theme+<stage>` for
  its FROM images; a Sitefile that cannot be read is an `error` row. `--provenance` lists the import (`import:theme`,
  digest: the imported Sitefile's sha256, or the checkout's tree digest with `gitCommit` for Git) and the imported
  Sitefiles' imports as `import:theme+data`, their FROM images as `from:theme:<line>`. `graph` shows a
  `stage:theme+assets` node (kind `stage`, label `stage assets of IMPORT theme`) fed by `import:theme`, without
  reading the imported Sitefile; `why` follows a file into the imported Sitefile's stages (stage
  `theme+assets`, locations in `theme/Sitefile`). In watch mode the imported Sitefile's steps are memoized like
  this file's (keyed by its label and ARG values), and a local imported directory inside the context is watched
  with it. `validate` and `graph` never read the imported Sitefile; `build`, `inspect`, `why`, `vendor` and
  `outdated` do.
- **Lint and editors.** `unused-name` reports an `IMPORT … AS sitefile` none of whose stages is named. The language
  server completes `AS sitefile` and `ARG`, documents `<name>+<stage>` on hover, and goes to the stage's
  definition in a local imported Sitefile (`<dir>/Sitefile` next to the document); the name part goes to the
  IMPORT.

### Build secrets (`--secret`, `IMPORT … SECRET`)
Remote IMPORTs from private APIs and repositories need credentials, which URLs may not carry.
Like Docker's `--mount=type=secret`, a secret is given on the command line and named in the
Sitefile; its value is never part of the Sitefile, the output or any record of the build.
- **CLI**: `--secret id=<id>,env=<VAR>` (an environment variable) or `--secret id=<id>,src=<file>`
  (a file, relative to the working directory; one trailing newline is dropped), repeatable, on
  `build`, `inspect`, `why`, `vendor` and `outdated` (`validate` and `graph` accept and ignore it).
  `<id>` is an identifier. A malformed flag is a usage error that does not echo the flag (it might be
  the secret); an unset variable, an unreadable or empty file, or a value with characters other than
  printable ASCII, spaces and tabs is `E_SECRET`. An id no IMPORT names is a warning.
- **Sitefile**: `SECRET <id> AS HEADER <Name>` sends the value verbatim as the request header `<Name>`
  (an HTTP token); `SECRET <id> AS BEARER` sends `Authorization: Bearer <value>`. Clauses may be
  repeated and mixed with `AS`, `INTEGRITY` and `SORT BY` in any order, but a header (compared
  case-insensitively; `BEARER` is `Authorization`) only once. Headers the client sets itself (`Host`,
  `Content-Length`, `Content-Type`, `Accept`, `Accept-Encoding`, `User-Agent`, `Git-Protocol`,
  `Connection`, …) are `E_SYNTAX`. `SECRET` on a local import is `E_VALIDATION`.
- **Missing**: an IMPORT whose secret was not given fails with `E_SECRET` when it would be fetched. A
  vendored copy needs no secret (nothing is fetched).
- **Where the value goes**: only into the request headers of that IMPORT, and only to the origin
  (scheme, host and port) of its URL: a redirect to another origin is followed without it (a
  same-origin redirect keeps it). For Git imports it goes with every Git request to the repository.
- **Where it never goes**: the output, cache and memo keys (they hold the secret's *id* and header
  name, never the value), `Sitefile.lock` and the vendor manifest (keyed by URL, as before),
  provenance and SBOM, `--profile` traces, `validate --json` (ids and header names only), warnings and
  error messages. As a last line of defence, every warning and error of a build, `vendor` or
  `outdated` run with secrets has each secret value replaced by `***` (for example when a server puts
  the credential it received into a redirect URL).
- The content fetched with a secret is ordinary content: it is pinned, locked and vendored like any
  other import, so commit a vendor directory of private data only where that data may live.

### Vendoring (`sitefile vendor`)
`sitefile vendor [context]` (like `cargo vendor`) downloads every remote input so
later builds work offline and without the network flags:
- It fetches each remote `IMPORT`, each registry `FROM image` and the `BASE`
  image (of an `oci` export, or `--base`) **exactly as a build would**: same network policy and limits, same
  `INTEGRITY` and `Sitefile.lock` checks (`--frozen-lockfile`, `--update-lock`),
  and `Sitefile.lock` is updated the same way. It always fetches; an existing
  vendor directory is not read.
- It then **replaces** the vendor directory (`--vendor-dir`, default
  `vendor/sitefile`, a path inside the context) as a whole, written to a
  temporary sibling and swapped in:
  - `blobs/sha256/<hex>`: content-addressed bytes: each import body (an archive
    imported `AS tree` is stored as the archive), and for a `FROM image` or `BASE` image the blobs
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
  (`... is not vendored in vendor/sitefile; fetching it`). For a `FROM image` or `BASE`, a tag
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
- **Images**: each registry `FROM image` (kind `image`, named by its stage, or
  `#<n>` for an unnamed one; local-store images have no upstream and are not
  listed), then the `BASE` (the `oci` export's `BASE`, or `--base`): a tag is resolved again
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
  `--target` build still resolves them all), every registry `FROM image` and the same `BASE`. It always asks
  upstream, never a vendor directory (`sitefile vendor` copies are what is
  pinned, not what is current), and `--cache-from`/`--cache-to` references are
  caches, not inputs.
- `--json` prints `{"inputs": [{kind, name, source, pin, pinnedBy, latest, status,
  newerTag, error}], "summary": {current, changed, unpinned, digest-only, error}}`;
  `pinnedBy` is `integrity`, `lockfile`, `digest`, `commit` or `null`, `error` is
  `{code, message}` or `null`.

### Archives (`IMPORT … AS tree` on a file)
- Formats are detected from magic bytes, not names: tar (ustar or v7), tar.gz /
  tgz (gzip; a gzip stream that holds a Web Bundle is read as one), tar.zst / tzst
  (Zstandard), and zip (stored and deflate, including ZIP64 records: the zip64
  end record and locator and the 0x0001 extra field, as streaming writers add
  them even to small archives; no encryption). Anything
  else fails with `E_TYPE`; xz and bzip2 streams and 7z and RAR
  archives are named in the message, with what to convert them to. Supported tar extras: POSIX ustar prefix (old GNU
  headers keep timestamps there and are read without it), GNU long names, pax
  `path`/`size` (a malformed record is `E_ARCHIVE`). Other pax keys are ignored.
  Zip directories are recognized by a trailing `/` or by a directory mode.
  Zip entry names are UTF-8 when general-purpose bit 11 is set. Without it,
  the Info-ZIP Unicode Path field (0x7075) is used when its CRC-32 matches the
  raw name; otherwise a name that is valid UTF-8 is read as UTF-8 (macOS, 7-Zip
  and many tools write UTF-8 without the flag), and any other name as IBM437
  (CP437), as older Windows Explorer, Info-ZIP and Java tools write it.
  A gzip stream may hold several members; zero bytes after the last one are
  ignored, any other trailing data must be another valid member.
  A zstd stream may hold several frames and skippable frames, as libzstd reads
  them: a frame's checksum is checked when present, data after a frame that
  does not start another frame is ignored, and a frame that needs a window
  over 128 MiB or a dictionary is `E_ARCHIVE` (as is a truncated stream).
  Decompression is asynchronous, so other imports keep making progress.
- `INTEGRITY` and the lockfile apply to the archive bytes exactly as for file
  imports, and the import's provenance digest is the archive's digest.
- Directories are implicit. Modes are reduced to `0644`/`0755`. A single
  top-level directory is **not** stripped: `COPY name:/top/ /` does that.
- OS metadata is dropped, with a warning naming the import and the count:
  `__MACOSX/` anywhere in a path, AppleDouble `._*` files, `.DS_Store`,
  Finder's `Icon\r`, and `Thumbs.db` and `desktop.ini` in any case. A Finder
  zip of `site/` is then just `site/`. `--keep-os-metadata`
  (`BuildOptions.keepOsMetadata`) keeps them. Unsafe or conflicting entries
  are still errors, junk or not.
- Entry names are normalized to NFC, and names that differ only in case are
  `E_PATH_COLLISION` (see "Writes and conflicts").
- Unsafe entries are rejected, never skipped: absolute paths (including `C:`),
  `..` segments, backslashes, NUL (`E_ARCHIVE_UNSAFE`), and symlink, hard link,
  device and fifo entries (`E_ARCHIVE_UNSAFE`). A zip whose local header
  disagrees with its central-directory record (name, compression method, or
  sizes when there is no data descriptor), or whose entries overlap, is
  `E_ARCHIVE_UNSAFE` too: other tools would extract different files from it. Duplicate paths, or a file and
  directory at one path, fail with `E_CONFLICT`. Malformed or truncated
  archives fail with `E_ARCHIVE`, as does a zip entry whose CRC-32 does not
  match its central-directory record (a corrupt stored entry inflate cannot
  notice).
- Limits (`E_ARCHIVE_LIMIT`), configurable through `BuildOptions.archiveLimits`:
  `maxEntries` 10 000, `maxTotalBytes` 500 MiB of uncompressed content, and
  `maxRatio` 100 (uncompressed:compressed, measured on the gzip or zstd stream or the
  whole zip; not enforced below 1 MiB of output, since tar padding compresses
  extremely well). Inflation is bounded with zlib's `maxOutputLength` (zstd too), so a
  bomb is stopped at the limit, never fully inflated; zip entries are never
  inflated beyond their declared size. The download cap still applies first.

### Host files: EXPORT … FOR <host>

`EXPORT <path> … FOR netlify|cloudflare|vercel|firebase|azure|github-pages|s3` (the last clause; the host
name is case-insensitive, any other word is `E_SYNTAX`) writes the REDIRECT and HEADER rules,
FINGERPRINT's `Cache-Control` lines and CSP's `Content-Security-Policy` lines as that host reads them
(a CSP line the host cannot carry is an error at the CSP line, like a HEADER). It works with `AS directory`, `zip`, `tar`
and `tgz` (`E_VALIDATION` otherwise), and not with `--target`, which exports the stage's raw tree. It is
the reverse of `sitefile import-config` (CLI.md).

**A rule the host cannot express as Netlify (and `sitefile serve`) apply it is an error**: `E_EXPORT` at
the rule's line, for example `EXPORT … FOR firebase: REDIRECT /old /new 303: Firebase Hosting redirects
are 301 or 302 (build with --lossy to leave it out)`. With `build --lossy`, the rule is left out with a
warning instead. FINGERPRINT's own cache headers are never an error: when the host cannot carry them,
they are left out with a warning.

Netlify applies a redirect only when no file answers the path, a file at the path itself, its
`/index.html` or its `.html` clean URL (`REDIRECT` has no `!`). Cloudflare Pages, Firebase, Azure and S3
apply redirects first. So on those hosts a redirect that a file in the export answers (for `/blog/*`,
any file below `/blog/`) is refused: the site would behave differently there.

| Host | Writes | Refused (or left out with `--lossy`) |
| --- | --- | --- |
| `netlify` | `_redirects` and `_headers`, as without `FOR` | nothing |
| `cloudflare` | `_redirects` and `_headers` | redirects a file answers first; more than 2,000 static or 100 dynamic redirects, more than 100 header patterns |
| `vercel` | the files under `.vercel/output/static/` and `.vercel/output/config.json` (Build Output API v3, for `vercel deploy --prebuilt`). In `routes`, every header pattern comes first as `{src, headers, continue: true}` (`*` becomes `.*`, `:name` becomes `[^/]+`), then `{handle: "filesystem"}`, then each redirect as `{src, headers: {Location}, status}` (an exact path also matches with a trailing slash; `/*` becomes `(?<splat>.*)` and `:name` becomes `(?<name>[^/]+)`, used as `$splat` and `$name` in the target), then `/404.html` for misses when the export has one | `*` other than a trailing `/*`, a `:name` in the target that the source does not have |
| `firebase` | the files under `public/` and `firebase.json` with `hosting.public`, `redirects` (`/*` becomes `/:splat*`) and `headers` (`*` becomes `**`) | statuses other than 301 and 302, header patterns with `:placeholders`, redirects a file answers first |
| `azure` | `staticwebapp.config.json` next to the files: `routes` (redirects, then header routes), `globalHeaders` (the `/*` pattern) and `responseOverrides.404` for `/404.html` | statuses other than 301 and 302; `:placeholders`; `:splat` in a target; patterns that are not exact or a trailing `/*`; two routes for the same paths (only the first applies there); a route header that is also in `/*` (Azure replaces rather than adds); redirects a file answers first |
| `github-pages` | `.nojekyll`, and `CNAME` with the host of `BASE` when it is a custom domain (not `*.github.io`; it must have no path). `/404.html` is used as it is | every REDIRECT and HEADER |
| `s3` | `_s3/website.json`, the bucket website configuration (`IndexDocument`, `ErrorDocument` for `/404.html`, `RoutingRules` for `/prefix/*` redirects: `KeyPrefixEquals`, `ReplaceKeyPrefixWith` for a target ending in `/:splat`, else `ReplaceKeyWith`, `HostName`/`Protocol` for absolute targets). `_s3/objects.json` lists per-object metadata (`Key`, then `CacheControl`, `ContentType`, …, from HEADER, and `WebsiteRedirectLocation` for exact-path redirects: create an empty object with it). Leave `_s3/` out of the upload (`aws s3 sync … --exclude '_s3/*'`) | exact redirects that are not 301; redirecting `/` or every key; `:placeholders`; a target that keeps the path anywhere but at the end; headers other than Cache-Control, Content-Type, Content-Disposition, Content-Encoding, Content-Language and Expires; redirects a file answers first |

A file the host step writes (`CNAME`, `firebase.json`, …) that the tree already has with different
content is `E_CONFLICT`, as for any other write.

### Export formats
`--out` is a directory for `directory` and a **file** for every other format.
Default `--out`: `<context>/dist`, `dist.zip`, `dist.tar`, `dist.tar.gz`,
`dist.wbn` (`dist.wbn.gz` for `AS wbn COMPRESSION gzip`), `dist.oci.tar`, `dist.car`, `dist.html`, `dist.mhtml`, `dist.warc` (`dist.warc.gz` for `AS warc COMPRESSION gzip`), `dist.swbn`, `dist.wacz`, `dist.epub`. The format is chosen by, in order: `--format`; the `--out`
extension (`.zip`, `.tar`, `.tar.gz`/`.tgz`, `.wbn`, `.wbn.gz`, `.car`, `.mhtml`/`.mht`, `.warc`, `.warc.gz`, `.swbn`, `.wacz`, `.epub`; not `.html`, which stays a directory name), so `-o site.zip` writes a
zip; then the Sitefile's `EXPORT ... AS`; then `directory`. `.oci.tar` selects `oci`.
A wbn written to `*.wbn.gz` is gzipped unless `--compression none` says otherwise.

All archive writers are deterministic: the same tree gives byte-identical
output, whatever the order files were added, and the TypeScript and Rust
implementations write the same bytes. Compression uses sitefile's own
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

- **zip**: raw deflate (stored when deflate does not shrink the entry, and always for already-compressed media by
  extension: png, jpg/jpeg, gif, webp, avif, heic, jxl, woff/woff2, mp3, m4a, aac, ogg/oga, opus, flac, mp4, m4v,
  webm, mov, mkv, zip, gz, tgz, br, zst, xz, bz2, 7z, rar, wbn; tar.gz and OCI layers are unchanged), CRC-32, central directory, UTF-8 names (flag bit 11), an
  extended-timestamp extra field (0x5455, mtime only: `SOURCE_DATE_EPOCH`, else 1980-01-01, in UTC, so unzip shows the
  same time in every time zone; the DOS time fields hold the same instant in UTC), unix modes in
  the external attributes (version made by = 3 << 8). No zip64 is written: more than
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
  `--oci-layers <n>` (build only, 1 to 64, default 1) splits the site's files over `n` layers instead
  of one: a file goes to layer `u32be(sha256(path)[0..4]) mod n` (its path in the exported tree), each
  layer is a deterministic tar of its files under the root (with the root's parent directories), and the
  config carries `dev.sitefile.layers=<n>`. A push after changing a few files then uploads only the
  layers holding them (with 8 layers, a one-file change re-uploads about an eighth of the site). The
  store, `save`, `serve`, `push`, `pull` and `FROM image` read the files from all `n` site layers. The
  default stays one layer, so existing image IDs do not change.
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
    index entries point at the same response unless their headers differ
    (see `_headers` below).
  - Primary URL: `<base>`, in a `primary` section, if `/index.html` exists;
    otherwise there is no `primary` section.
  - Each file's response has `:status` `200` and `content-type`.
    `content-type` comes from the extension: html/htm, css, js/mjs, json, svg,
    txt, xml, webmanifest (all with `; charset=utf-8`), png, jpg/jpeg, gif,
    webp, avif, ico (`image/x-icon`), wasm, woff, woff2, pdf, mp4, webm, mp3;
    anything else is `application/octet-stream`.
  - **`_headers` (HEADER) is baked in** (johnhenry/sitefile#144). When the
    exported tree has `/_headers`, each response also gets the headers of
    every block whose pattern matches its URL path, exactly as
    `sitefile serve` matches them: `*` matches any run of characters, values
    of a name set by several blocks are joined with `, `, and a directory URL
    matches with its trailing slash (so `HEADER / …` applies to `<base>` but
    not to `<base>index.html`, which then get separate responses). A
    `Content-Type` there replaces the inferred one; the names `serve` owns
    (`content-length`, `etag`, `location`, …) are ignored, as in `serve`.
    Header names are lowercase and, like every CBOR map here, in bytewise
    order of their encoding. `serve`'s own defaults (`cache-control:
    no-cache`, `x-content-type-options`) are not added.
  - **Exact-path `_redirects` (REDIRECT) rules become redirect responses.**
    A rule without a splat and with a 301, 302, 303, 307 or 308 status is a
    response with that `:status`, a `location` of the target as written (a
    path stays relative to the host, as in `/_redirects`) and an empty body,
    at `<base><from>` — and nothing else: no `_headers` headers, as in
    `serve`. It follows `serve`'s order: a forced rule (`301!`, only possible
    in a hand-written `_redirects`) replaces the file at its path; any other
    rule applies only when `serve` would find nothing there (no file, no
    `<from>.html`, no directory), so `REDIRECT /about …` next to
    `/about/index.html` is not baked in. The first rule matching a path
    decides; when that is a splat or a `200`/`404` rewrite, the path is left
    to `/_redirects`. Identical redirects share one response.
    Not baked in, so only a reader of `/_redirects` applies them: splat rules
    (`/blog/* /posts/:splat`, whose targets depend on the request), `200`
    rewrites and `404` rules, and the trailing-slash spelling `<from>/` that
    `serve` also redirects. A `.wbn` imported with `AS tree` keeps only the
    `200` responses, so redirects do not become files.
  - `/_headers` and `/_redirects` stay in the bundle as files, so a bundle
    unpacked with `IMPORT … AS tree` (or by packfile's `fromWebBundle`) still
    deploys to Netlify or Cloudflare Pages, and serves with `sitefile serve`,
    with every rule, including the splats.
  - Index entries are `[offset, length]`, where offset is counted from the
    first byte of the responses section.
  - Not signed, no integrity block, no variants. Layout follows the b2 draft as
    understood when written; validate with a Web Bundle tool before relying on
    it in a browser.
  - `COMPRESSION gzip` (or `--compression gzip`, or `-o *.wbn.gz`): the same
    bundle bytes inside one gzip member written by the shared deterministic
    encoder (header mtime 0, no file name, OS byte 3; see `src/deflate.ts`), so
    both implementations write identical `.wbn.gz` files. With
    `BASE https://packfile.invalid/` it is exactly what packfile's
    `fromArchive(buffer)` reads with no options.

- **car**: an [IPFS CARv1](https://ipld.io/specs/transport/car/carv1/) file of the tree as a UnixFS DAG,
  built the way kubo's `ipfs add -r --hidden --cid-version=1 <dir>` builds it, so the root CID is the one
  kubo prints for the exported directory (tested against kubo 0.43, whose `ipfs dag export` of that root
  gives the same bytes). Files are cut into 262144-byte chunks, each a raw block (CIDv1, codec `raw`); a
  file of one chunk is that block; longer files are a balanced tree of dag-pb UnixFS File nodes of at most
  174 links. Directories are dag-pb UnixFS Directory nodes with links sorted by name; a directory big
  enough for IPFS to shard (HAMT, over 256 KiB of names and CIDs, roughly 5000 entries) fails with
  `E_EXPORT`. Every CID is v1 with a SHA-256 multihash. The header's one root is the tree's `/`; blocks
  follow once each, root first, then depth first in link order. No modes or timestamps are recorded (kubo
  adds neither by default), so `SOURCE_DATE_EPOCH` does not change the file. `build` and `save` print the
  root CID (`root bafy…`). Upload the file to an IPFS pinning service (`ipfs dag import`, web3.storage,
  Pinata). Tools that wrap the directory in one more directory (as `ipfs-car pack` can) report a different
  root; compare with kubo's unwrapped `ipfs add -r`.

- **html**: one page as a single self-contained file. The page is `index.html` of the exported tree
  (`EXPORT /docs AS html` exports `/docs/index.html`); a tree without one fails with `E_EXPORT`. Every
  asset the page references is inlined as a `data:<type>;base64,…` URI (the type from the Web Bundle MIME
  table): URLs in `src`, `srcset`/`imagesrcset`, `poster`, `xlink:href`, `<object data>` and `<link
  href>`, and in its CSS (`<style>`, `style=""`), found with the FINGERPRINT reference scanner. A
  stylesheet's own `url()` and `@import` references are inlined into it first (an import cycle is
  `E_EXPORT`). Kept as written: `<a href>` and `<area href>` (navigation), references to other `.html`
  files (pages, such as a canonical `<link>`), external URLs (`https:`, `//host/`; the page still loads
  them when online) and non-file URLs (`data:`, `mailto:`, `#top`). A reference to a file that is not in
  the tree, or to a directory, fails with `E_EXPORT` naming the page and line. A fragment is kept after
  the data URI (`icons.svg#home`). Scripts are inlined byte for byte: what they fetch or import at run
  time is not followed. Text must be UTF-8.
- **mhtml**: the same page and the same assets (each once, in the order first referenced, through CSS
  too) as one MHTML file (RFC 2557 `multipart/related`, what Chrome writes for "Webpage, Single File"),
  references unchanged. Each part is base64 with its `Content-Type` and an absolute `Content-Location`:
  the file's path under `BASE` (or `--base-url`; an https URL ending in `/`, default
  `https://site.invalid/`), the page first. Root-relative references (`/css/a.css`) resolve to parts only
  when the base is an origin (`https://example.com/`). The boundary is fixed and the file has no date, so
  the output depends only on the tree.

- **warc**: a [WARC/1.1](https://iipc.github.io/warc-specifications/specifications/warc-format/warc-1.1/)
  web archive (ISO 28500) of the site at `BASE` (required, from here or `--base-url`; an https URL
  ending in `/`). A `warcinfo` record comes first, then, in path order, one `response` record per file
  at `<BASE><path>` (percent-encoded): `HTTP/1.1 200 OK` with the Content-Type of the Web Bundle MIME
  table (or the site's `_headers`), `Content-Length`, and every `_headers` rule that matches the path, in
  file order. `_headers` and `_redirects` themselves are not recorded. An `index.html` is recorded again
  at its directory's URL (`<BASE>`, `<BASE>docs/`), which replay tools request. `WARC-Payload-Digest` and
  `WARC-Block-Digest` are `sha256:<hex>`; record IDs are version-8 UUIDs derived from the record, and
  every `WARC-Date` is `SOURCE_DATE_EPOCH` (default `1970-01-01T00:00:00Z`), so the same build gives the
  same bytes. `COMPRESSION gzip` (or `-o site.warc.gz`) writes every record as its own gzip member, the
  usual `.warc.gz` layout. WARCs are also read: `IMPORT x FROM site.warc AS tree` (plain or gzipped) and
  `sitefile serve site.warc` take the `200` responses of the first response's host, a URL ending in `/`
  as that directory's `index.html`, the first record of each path; paths are checked like archive entries
  (`E_ARCHIVE_UNSAFE`) and the archive limits apply.
- **wacz**: a [WACZ 1.1.1](https://specs.webrecorder.net/wacz/1.1.1/) package, the format
  ReplayWeb.page opens; needs `BASE` like `warc`. A zip (written like `AS zip`) of
  `archive/data.warc.gz` (the same bytes as `AS warc COMPRESSION gzip`), `indexes/index.cdx` (one
  CDXJ line per response record, `<SURT> <YYYYMMDDhhmmss> {url, mime, status, digest, length, offset,
  filename}`, sorted, the offset and length addressing the record's gzip member), `pages/pages.jsonl`
  (every HTML page, a directory index by its directory URL, with its `<title>`), `datapackage.json`
  (every file's SHA-256 and size) and `datapackage-digest.json`. Dates are `SOURCE_DATE_EPOCH`
  (default `1970-01-01T00:00:00Z`). The package is not signed and is not read back (import a `.warc`).

- **swbn**: a [Signed Web Bundle](https://github.com/WICG/webpackage/blob/main/explainers/integrity-signature.md)
  for an [Isolated Web App](https://github.com/WICG/isolated-web-apps). `EXPORT / AS swbn KEY iwa` with
  `--secret id=iwa,src=key.pem`, where `key.pem` is an unencrypted Ed25519 private key in PEM, PKCS #8
  (`openssl genpkey -algorithm ed25519 -out key.pem`); anything else is `E_SECRET`, and a missing secret
  is `E_SECRET` too. The key is only read, never written to the output, the lockfile, caches or traces.
  The Web Bundle ID is the lower-case, unpadded base32 of the public key and the suffix `00 01 02`;
  `build` prints it (`web bundle id …`), and the app's origin is `isolated-app://<id>/`. The bundle is
  the tree's Web Bundle (b2, as `AS wbn`) at that origin, preceded by the v2 integrity block
  `[magic, "2b\0\0", {webBundleId}, [[{ed25519PublicKey}, signature]]]`, signed over the length-prefixed
  SHA-512 of the bundle, the block without signatures and the signature attributes. Ed25519 is
  deterministic, so the same tree and key give the same bytes; the output matches `wbn-sign` 0.3.1
  byte for byte. The tree should hold `/.well-known/manifest.webmanifest` for Chrome to install it.
  The Rust port signs with `ring`'s Ed25519 (already in the build through rustls). A `--secret` value may contain line
  breaks (a PEM key); one used as an HTTP header (`IMPORT … SECRET`) may not (`E_SECRET`).
- **epub**: an [EPUB 3](https://www.w3.org/TR/epub-33/) publication. The zip starts with `mimetype`
  (`application/epub+zip`, stored, without an extra field, as the OCF container requires), then
  `META-INF/container.xml`, the package document `EPUB/package.opf` and the site's files under `EPUB/`.
  The spine is every `.xhtml`/`.html` page in path order with `/index.xhtml` or `/index.html` first;
  they are declared `application/xhtml+xml`, so they must be well-formed XHTML (write them with
  `.xhtml` templates). A site file `/nav.xhtml` is the navigation document; otherwise one is generated
  that lists the spine pages by `<title>`. Metadata comes from the first page: its `<title>` (default
  `Untitled`) and `<html lang>` (default `en`); the identifier is a UUID derived from the tree digest
  and `dcterms:modified` is `SOURCE_DATE_EPOCH` (default the Unix epoch), so the same tree gives the
  same file. `_redirects` and `_headers` are left out; a site file `/package.opf` is `E_CONFLICT`; a
  tree without a page is `E_EXPORT`. See `examples/book`.

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
