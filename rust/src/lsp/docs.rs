//! Short hover texts, condensed from docs/LANGUAGE.md. Keep them in step with it.

/// (keyword, signature, summary)
const INSTRUCTIONS: &[(&str, &str, &str)] = &[
    (
        "FROM",
        "FROM scratch|<base>|image <ref> [AT <dir>] [AS name]",
        "Starts a stage. `scratch` is an empty tree; a bare name of an earlier stage starts from a copy of its final tree; `<import>+<stage>` starts from a stage of a Sitefile imported `AS sitefile`; `image <ref>` starts from the files of an OCI site image (a registry reference is pinned in Sitefile.lock; a name without a registry host is a local-store image); anything else is a context directory (`./name` forces the directory). Without any FROM, an implicit unnamed `FROM scratch` starts at the first instruction other than ARG and `IMPORT … AS sitefile`.",
    ),
    (
        "STAGE",
        "STAGE name [FROM scratch|<base>|image <ref> [AT <dir>]]",
        "Starts a named stage: exactly `FROM base AS name`; `STAGE name` alone is `FROM scratch AS name`.",
    ),
    (
        "ARG",
        "ARG NAME[: string|number|boolean] [= value]",
        "A build parameter. Without a default it is required: pass `--build-arg NAME=value` (or `--build-arg-file`). Usable as `${NAME}` on later lines.",
    ),
    (
        "LET",
        "LET NAME[: type] = value | LET name: html = <markup> | LET name: html <<HTML",
        "A computed variable, usable as `${NAME}` on later lines. ARG and LET share one namespace; rebinding fails. `LET name: html` is an html fragment: a template evaluated where a RENDER template uses it (`${name}`) and inserted as markup (values inside it are still escaped); templates only.",
    ),
    (
        "IMPORT",
        "IMPORT name FROM <uri> [AS kind] [INTEGRITY sha256:…] [SORT BY key [ASC|DESC]] [SCHEMA <file>] [DRAFTS true|false] [SECRET id AS HEADER <Name> | AS BEARER]… [ARG NAME=value]…",
        "Names a resource: a context-relative path, an `https://` URL, or a Git repository (`git+https://host/repo.git#ref:path`; pin a commit for reproducibility). Kinds: file, text, json, yaml, csv, markdown, wbn, tree (default: tree for a directory, else file), and sitefile (another Sitefile, whose stages are `name+<stage>`). COPY places files (`COPY name: /dest/`); text and data kinds are values for RENDER templates and FOR.",
    ),
    (
        "COPY",
        "COPY [--overwrite] [--from=stage] [--exclude=<pattern>…] [--chmod=644|755] [--parents] <source> <dest>",
        "Places a file or directory into the stage's tree. `<source>` is a context path, `name:` / `name:/inner` for an import, a path in an earlier stage with `--from`, or an inline `<<NAME` body. `<dest>` is absolute; a trailing `/` means a directory.",
    ),
    (
        "RENDER",
        "RENDER [--escape=none|html] [--overwrite] [--from=stage] <template> TO <dest> [FOR item IN list [BY key] [PAGE n [FIRST <dest>]]]",
        "Renders a template (a file or an inline `<<NAME` body) to a file. Templates use `${NAME}`, `${name.key}`, `${#each list as item}…${/each}`, `${#if x}…${else}…${/if}` and `${value|raw}`.",
    ),
    (
        "TRANSFORM",
        "TRANSFORM [--overwrite] [--from=stage] [--include=<pattern>…] [--exclude=<pattern>…] [<source>] WITH name TO <dest> [OPTIONS key=value …] [FOR item IN list] [ARGS …]",
        "Runs a PLUGIN (or an allowed TOOL) on the source and writes what it returns at `<dest>`. A directory source runs it once per file, in path order (`TO /out/` gives each file the directory `/out/<dir>/`; `TO /out/*` or `/out/*.ext` one file named after it). A TOOL needs `ARGS` with `{output}` (and `{input}` when there is a source).",
    ),
    (
        "PLUGIN",
        "PLUGIN name FROM ./plugins/x.mjs|x.wasm [INTEGRITY sha256:…]",
        "Declares a sandboxed renderer: a local ES module or an import-free WebAssembly module. URLs are not allowed. A JavaScript plugin runs only when the CLI allows it with `--allow-plugin name` (or `--allow-plugins`); a wasm plugin needs no flag. See docs/PLUGINS.md.",
    ),
    (
        "TOOL",
        "TOOL name",
        "Declares an external program for TRANSFORM … WITH name … ARGS. It runs only when the CLI allows it with `--allow-exec name`.",
    ),
    (
        "SITEMAP",
        "SITEMAP [--overwrite] BASE <url> [TO <dest>]",
        "Writes a sitemaps.org file (default `/sitemap.xml`) listing every `.html` file in the stage's tree at this point, sorted by path, except `/404.html`. `/a/index.html` is listed as `<base>/a/`.",
    ),
    (
        "FEED",
        "FEED [--overwrite] <import> BASE <url> LINK <path> [TO <dest>] [TITLE <text>]",
        "Writes an Atom feed (default `/feed.xml`) from a list import such as `AS markdown`, in list order. Each element needs `title` and `date`; `LINK` is evaluated per element with `${item.key}`.",
    ),
    (
        "REDIRECT",
        "REDIRECT [--overwrite] <from> <to> [STATUS 301|302|303|307|308]",
        "Adds a line to `/_redirects` (Netlify / Cloudflare Pages format), written at EXPORT in source order. `<from>` is an absolute path (`/*` and `:splat` work on hosts and in `serve`); `<to>` a path or http(s) URL. Default status 301.",
    ),
    (
        "HEADER",
        "HEADER [--overwrite] <path-pattern> <Name>: <value>",
        "Adds a response header for paths matching the pattern (`*` matches anything) to `/_headers`, written at EXPORT; one block per pattern, in source order.",
    ),
    (
        "MIME",
        "MIME <.ext> <type/subtype>",
        "Sets the `Content-Type` of the files with that extension: adds `/*<.ext>` with `Content-Type: <type/subtype>` to `/_headers` (in source order with HEADER), which `serve`, Netlify, Cloudflare Pages and a Web Bundle export use. No parameters; written in lowercase.",
    ),
    (
        "FINGERPRINT",
        "FINGERPRINT [--overwrite] [--no-cache-headers] <pattern>… [MANIFEST <dest>] [INTEGRITY]",
        "Renames the files matching the patterns (`*`, `?`, `**`; `/assets/` means everything below it) to `name.<hash>.ext` (10 hex digits of the SHA-256 of the content) and rewrites the references to them in every HTML and CSS file of the stage. HTML pages keep their names. A CSS file is hashed after its own references are rewritten. EXPORT adds `Cache-Control: public, max-age=31536000, immutable` for the renamed files and `no-cache` for the pages to `/_headers`.",
    ),
    (
        "CANONICAL",
        "CANONICAL BASE <url> [PRETTY]",
        "Adds `<link rel=\"canonical\" href=\"<base><path>\">` before `</head>` of every page of the stage below the EXPORT path that has no canonical link (`…/index.html` becomes `…/`). `PRETTY` drops `.html`. Pages without `</head>` are left alone, with a warning.",
    ),
    (
        "PRECOMPRESS",
        "PRECOMPRESS [--overwrite] [gzip] <pattern>… [MIN <bytes>]",
        "Writes a gzip sidecar `<file>.gz` next to each matching compressible file (text, scripts, data, SVG, fonts) of at least MIN bytes (default 1024), when it is smaller, for servers that send it in place of the file (nginx `gzip_static`, Caddy `precompressed`). Deterministic: the shared deflate encoder, a fixed header. `br` and `zstd` are not supported yet.",
    ),
    (
        "ROBOTS",
        "ROBOTS [--overwrite] [ALLOW <path> | DISALLOW <path>]… [SITEMAP] [TO <dest>]",
        "Writes `/robots.txt` for every user agent (`User-agent: *`) with the ALLOW and DISALLOW rules in source order (an empty `Disallow:` allows everything). `SITEMAP` adds a `Sitemap:` line for each SITEMAP of the Sitefile (its BASE and destination).",
    ),
    (
        "SECURITY_TXT",
        "SECURITY_TXT [--overwrite] CONTACT <uri>… EXPIRES <date> [ENCRYPTION <uri>]… [POLICY <uri>]… [TO <dest>]",
        "Writes an RFC 9116 `/.well-known/security.txt`. CONTACT (`mailto:`, `tel:` or `https:`) and EXPIRES (a date or RFC 3339 date-time, not a duration) are required; a date in the past or less than 30 days away warns.",
    ),
    (
        "PRECACHE",
        "PRECACHE [--overwrite] <pattern>… [TO <dest>] [INTO <service-worker>]",
        "Writes a Workbox-style precache manifest, `[{url, revision}]`, of the matching files below the EXPORT path (default `/sw-manifest.json`); `revision` is 10 hex digits of the SHA-256, or null for a fingerprinted file. `INTO` replaces `self.__WB_MANIFEST` in a service worker of the stage with it.",
    ),
    (
        "CHECKLINKS",
        "CHECKLINKS [--warn] [--external]",
        "Checks every reference in the stage's HTML and CSS files at this point: a link to a file that is not there (and that no REDIRECT covers) fails the build with `E_BROKEN_LINK`, listing each with its file and line. Clean URLs (`/about` → `/about.html`) and directory indexes count as found.",
    ),
    (
        "EXPORT",
        "EXPORT <path> [AS directory|zip|tar|tgz|wbn|oci|car|html|mhtml|warc] [BASE <url>] [AT <root>] [COMPRESSION gzip|none]",
        "Exports `<path>` of the last stage. Exactly one, and the last instruction. The CLI's `--out`, `--format` and `--base-url` override it.",
    ),
];

/// (instruction or "*", clause, text)
const CLAUSES: &[(&str, &str, &str)] = &[
    ("FROM", "AS", "`AS name` names this stage, so a later `FROM name`, `STAGE x FROM name` or `--from=name` can use it."),
    ("FROM", "SCRATCH", "`scratch` (the bare word) is an empty tree. `\"scratch\"` is a path."),
    ("FROM", "IMAGE", "`image <ref>`: the files of an OCI site image. With a registry host (`ghcr.io/me/theme:1`) the tag is resolved to a digest once and recorded in Sitefile.lock `images` (or pin it with `@sha256:`); without one (`theme:1`, made by `build -t`) it is an image in the local store."),
    ("FROM", "AT", "`AT <dir>`: the directory inside the image to start from (default: the image's `dev.sitefile.root` label, `/` without one)."),
    ("STAGE", "FROM", "`FROM base`: `scratch`, an earlier stage, `image <ref>`, or a context directory. Omitted: `scratch`."),
    ("STAGE", "IMAGE", "`image <ref>`: the files of an OCI site image (a registry reference is locked in Sitefile.lock; no registry host: the local store)."),
    ("STAGE", "AT", "`AT <dir>`: the directory inside the image to start from (default: its `dev.sitefile.root` label)."),
    ("STAGE", "SCRATCH", "`scratch` (the bare word) is an empty tree."),
    ("IMPORT", "FROM", "`FROM <uri>`: a context-relative path, an `https://` URL (`http://` only with `--allow-http`), or `git+https://host/repo.git#ref[:path]` (ref: a branch, tag or 40-hex commit)."),
    ("IMPORT", "AS", "`AS kind`: file, text, json, yaml, csv, markdown, wbn, html, tree or sitefile. `AS tree` on a file extracts an archive; `AS html` makes a file an html fragment."),
    (
        "IMPORT",
        "ARG",
        "`ARG NAME=value` (repeatable, `AS sitefile` only): sets an ARG of the imported Sitefile; its other ARGs keep their defaults, and this build's `--build-arg`s never reach it.",
    ),
    ("IMPORT", "INTEGRITY", "`INTEGRITY sha256:<64 lowercase hex>`: the content must match; remote pins are recorded in Sitefile.lock."),
    ("IMPORT", "SORT", "`SORT BY key [ASC|DESC]` orders a list import (markdown, json, yaml, csv) by a field; numbers and numeric strings compare numerically."),
    ("IMPORT", "BY", "`SORT BY key`: the field to sort by."),
    (
        "IMPORT",
        "SCHEMA",
        "`SCHEMA <file>`: a local `.yaml`/`.json` schema every document (markdown frontmatter, or each element of a json/yaml/csv list) must match: `required: [title, date]` and `properties:` with a type (string, number, boolean, date, list; `items` for list elements) or an `enum`. A mismatch fails the build with `E_SCHEMA`, naming the document, the key, what was expected and what was found.",
    ),
    (
        "IMPORT",
        "DRAFTS",
        "`DRAFTS true|false`: with `false`, elements whose `draft` is `true` are left out of the import (and so of FOR pages, groups, FEED and SITEMAP). Usually `DRAFTS ${DRAFTS}` with `ARG DRAFTS: boolean = false`, so `--build-arg DRAFTS=true` previews them.",
    ),
    (
        "IMPORT",
        "SECRET",
        "`SECRET <id> AS HEADER <Name>` or `SECRET <id> AS BEARER`: send the build secret given with `--secret id=<id>,env=<VAR>` (or `src=<file>`) as a request header, only to this URL's origin. The value is never recorded or shown. Repeatable, one clause per header.",
    ),
    ("IMPORT", "HEADER", "`SECRET <id> AS HEADER <Name>`: the secret is sent verbatim as the request header `<Name>`."),
    ("IMPORT", "BEARER", "`SECRET <id> AS BEARER`: the secret is sent as `Authorization: Bearer <secret>`."),
    ("IMPORT", "ASC", "Ascending order (the default)."),
    ("IMPORT", "DESC", "Descending order."),
    ("PLUGIN", "FROM", "`FROM ./path`: a local `.mjs` module (in its own folder) or `.wasm` module."),
    ("PLUGIN", "INTEGRITY", "`INTEGRITY sha256:<64 lowercase hex>`: the plugin file must match."),
    ("RENDER", "TO", "`TO <dest>`: the output file (absolute, no trailing `/`)."),
    ("RENDER", "FOR", "`FOR item IN list` renders once per element of a list import; `<dest>` may use `${item.key}`."),
    ("RENDER", "IN", "`IN list`: an IMPORT that is a list (markdown documents, a JSON/YAML array, CSV rows)."),
    ("RENDER", "BY", "`BY key` (after FOR): render once per distinct value of the field (a list field counts each value): the loop variable is `{key, count, items}`, groups in SORT BY order. Tag pages: `TO /tags/${tag.key}/index.html FOR tag IN posts BY tags`."),
    ("RENDER", "PAGE", "`PAGE n` (after FOR [BY]): render once per page of n items: `{count, items, number, total, url, prev, next}` (`prev`/`next` are '' at the ends; with BY, also `key`, per group). An empty list is one empty page."),
    ("RENDER", "FIRST", "`FIRST <dest>` (after PAGE n): the first page's destination, e.g. `/index.html` while later pages go to `/page/${page.number}/index.html`."),
    ("TRANSFORM", "WITH", "`WITH name`: a PLUGIN or TOOL declared on an earlier line."),
    ("TRANSFORM", "TO", "`TO <dest>`: where the result is written (a file, or a directory for several files). For a directory source, `*` (each file's name) or `*.ext` (its name with a new extension) as the last segment writes one file per input."),
    ("TRANSFORM", "OPTIONS", "`OPTIONS key=value …`: passed to the plugin; TOOL ARGS can use `{option.key}`. Values may use `${item.key}`."),
    ("TRANSFORM", "FOR", "`FOR item IN list` runs once per element of a list import."),
    ("TRANSFORM", "IN", "`IN list`: an IMPORT that is a list."),
    ("TRANSFORM", "ARGS", "`ARGS …` (rest of the line): the TOOL's argv, no shell. `{input}`, `{output}` (required) and `{option.key}` are replaced."),
    ("SITEMAP", "BASE", "`BASE <url>`: the absolute http(s) URL the site is published at (no query or fragment)."),
    ("SITEMAP", "TO", "`TO <dest>`: the sitemap file (default `/sitemap.xml`)."),
    ("FEED", "BASE", "`BASE <url>`: the absolute http(s) URL the site is published at; entry links are made absolute with it."),
    ("FEED", "LINK", "`LINK <path>`: each entry's page, evaluated per element (`/posts/${item.slug}/index.html`); `…/index.html` becomes `…/`."),
    ("FEED", "TO", "`TO <dest>`: the feed file (default `/feed.xml`)."),
    ("FEED", "TITLE", "`TITLE <text>`: the feed's title (default: the BASE URL)."),
    ("FINGERPRINT", "MANIFEST", "`MANIFEST <dest>`: a JSON file mapping each old path to its fingerprinted path (sorted by old path)."),
    ("FINGERPRINT", "INTEGRITY", "`INTEGRITY`: adds `integrity=\"sha384-…\"` (Subresource Integrity) to each `<script src>` and `<link rel=stylesheet|modulepreload>` that loads a renamed file, unless it has one."),
    ("CANONICAL", "BASE", "`BASE <url>`: the absolute http(s) URL the site is published at."),
    ("CANONICAL", "PRETTY", "`PRETTY`: canonical URLs without `.html` (`/about.html` → `<base>/about`)."),
    ("PRECOMPRESS", "MIN", "`MIN <bytes>`: the smallest file compressed (a whole number; default 1024)."),
    ("PRECOMPRESS", "gzip", "`gzip`: write `<file>.gz` (the default, and the only format so far)."),
    ("ROBOTS", "ALLOW", "`ALLOW <path>`: an `Allow:` rule (an absolute path; `*` and `$` work as robots.txt wildcards)."),
    ("ROBOTS", "DISALLOW", "`DISALLOW <path>`: a `Disallow:` rule (an absolute path; `/` keeps crawlers out of everything)."),
    ("ROBOTS", "SITEMAP", "`SITEMAP`: a `Sitemap:` line for each SITEMAP of the Sitefile (an error when there is none)."),
    ("ROBOTS", "TO", "`TO <dest>`: the file (default `/robots.txt`)."),
    ("SECURITY_TXT", "CONTACT", "`CONTACT <uri>` (required, repeatable): how to report a vulnerability: `mailto:`, `tel:` or `https:`."),
    ("SECURITY_TXT", "EXPIRES", "`EXPIRES <date>` (required): when the file is stale: `YYYY-MM-DD` or an RFC 3339 date-time."),
    ("SECURITY_TXT", "ENCRYPTION", "`ENCRYPTION <uri>` (repeatable): a key for encrypted reports: `https:`, `openpgp4fpr:` or `dns:`."),
    ("SECURITY_TXT", "POLICY", "`POLICY <uri>` (repeatable): the disclosure policy, an `https:` URL."),
    ("SECURITY_TXT", "TO", "`TO <dest>`: the file (default `/.well-known/security.txt`)."),
    ("PRECACHE", "TO", "`TO <dest>`: the manifest file (default `/sw-manifest.json`; with only INTO, none is written)."),
    ("PRECACHE", "INTO", "`INTO <service-worker>`: a file of the stage whose first `self.__WB_MANIFEST` becomes the manifest (Workbox `injectManifest`)."),
    ("REDIRECT", "STATUS", "`STATUS 301|302|303|307|308`: the redirect's status (default 301)."),
    ("EXPORT", "AS", "`AS format`: directory (default), zip, tar, tgz (alias tar.gz), wbn, oci, car (an IPFS CARv1 file; its root CID is the one `ipfs add -r --hidden --cid-version=1` prints), html (index.html with its assets inlined as data: URIs) mhtml (index.html and its assets as one MHTML file) or warc (a web archive of every file at BASE)."),
    ("EXPORT", "BASE", "`BASE <url>`: for wbn, the https URL the files are mounted at; for oci, a container image to build on."),
    ("EXPORT", "AT", "`AT <root>` (oci only): the directory the files occupy in the image (default /srv/www)."),
    ("EXPORT", "COMPRESSION", "`COMPRESSION gzip|none` (oci, wbn and warc only): how the oci site layer is stored, whether the wbn is gzipped, or whether each warc record is."),
];

const FLAGS: &[(&str, &str)] = &[
    ("overwrite", "`--overwrite`: replace files that already exist at the destination (otherwise a different file there is `E_CONFLICT`)."),
    ("from", "`--from=stage`: read the source from the final tree of an earlier, completed stage (an absolute path inside it); `--from=<import>+<stage>` reads a stage of a Sitefile imported `AS sitefile`."),
    ("warn", "`--warn` (CHECKLINKS): print each broken link as a warning and keep building."),
    ("external", "`--external` (CHECKLINKS): also request every http(s) link once, under the network policy (`--allow-http`, `--allow-private-network`); a failure or a status of 400 or more is a broken link."),
    ("escape", "`--escape=none|html`: with `html`, `& < > \" '` are escaped in every substituted value (`${x|raw}` opts out)."),
    ("exclude", "`--exclude=<pattern>` (repeatable): skip files of a directory source matching a `.sitefileignore` pattern, relative to the source (`*.md`, `drafts/`, `!keep.md`)."),
    ("include", "`--include=<pattern>` (repeatable, TRANSFORM): keep only the files of a directory source matching a `.sitefileignore` pattern, relative to the source (`*.jpg`, `gallery/`); `--exclude` then leaves files out."),
    ("chmod", "`--chmod=644|755`: the mode of every copied file (Sitefile trees hold only 0644 and 0755 files)."),
    ("parents", "`--parents`: keep the source's path under the destination (`COPY --parents ./src/a.css /out/` writes `/out/src/a.css`)."),
    ("no-cache-headers", "`--no-cache-headers` (FINGERPRINT): EXPORT writes no `Cache-Control` lines to `/_headers` for the fingerprinted files and the pages."),
    ("inline-svg", "`--inline-svg`: let the template's `${file \"/x.svg\" | svg}` paste SVG markup into the page. SVG can run scripts, so this is an explicit opt-in; `img` needs no flag."),
];

/// The `${file …}` template helpers (src/media.ts).
const FILE_HELPERS: &[(&str, &str)] = &[
    ("url", "The file's URL (its tree path, each segment percent-encoded); the same as `${file \"/x\"}`."),
    ("width", "The image's width in pixels, from its header (PNG, JPEG with EXIF orientation, GIF, WebP, AVIF)."),
    ("height", "The image's height in pixels, from its header."),
    ("type", "The file's media type, by extension (`image/png`, `text/css`, …)."),
    ("size", "The file's size in bytes."),
    ("integrity", "`sha384-…`: the subresource integrity of the file's bytes."),
    ("datauri", "A `data:` URL of the file (at most 32768 bytes)."),
    ("srcset", "The `srcset` from the image plugin's metadata (`<name>.json`, read for `/img/name.jpg` or given directly)."),
    ("img", "`<img src width height … loading=\"lazy\" decoding=\"async\">`: width and height from the image header, or `src srcset width height` (and a `<picture>` for several formats) from the image plugin's `<name>.json` when it is there. Attributes pass through (`alt=\"…\"`); one named like a default replaces it."),
    ("video", "`<video src …></video>`."),
    ("audio", "`<audio src …></audio>`."),
    ("source", "`<source srcset|src type …>` (srcset for an image, for `<picture>`)."),
    ("script", "`<script src integrity …></script>` with the file's sha384 integrity."),
    ("stylesheet", "`<link rel=\"stylesheet\" href integrity …>` with the file's sha384 integrity."),
    ("preload", "`<link rel=\"preload\" href as …>`: `as` from the extension (or `as=…`); integrity for scripts and styles, `crossorigin` for fonts and fetches."),
    ("modulepreload", "`<link rel=\"modulepreload\" href integrity …>`."),
    ("pre", "`<pre …>` with the file's text, escaped."),
    ("svg", "The SVG file's markup pasted into the page (attributes go on its root). Needs `RENDER --inline-svg`: SVG can run scripts."),
];

/// Hover text of `file` in `${file …}` and of a helper after its `|`.
pub fn template_file() -> &'static str {
    "```sitefile
${file \"/path\" [| helper [name=\"value\" | name]…]}
```
A file of the stage's tree at this RENDER (an absolute path; `${name}` works inside the strings). Alone, its URL; with a helper, a value (`url`, `width`, `height`, `type`, `size`, `integrity`, `datauri`, escaped like any value) or a tag (`img`, `video`, `audio`, `source`, `script`, `stylesheet`, `preload`, `modulepreload`, `pre`, `svg`), inserted as markup with every attribute value escaped."
}

pub fn file_helper(name: &str) -> Option<&'static str> {
    lookup(FILE_HELPERS, name)
}

const IMPORT_KINDS: &[(&str, &str)] = &[
    ("file", "One file, placed with COPY (the default for a file)."),
    ("text", "A UTF-8 text value for templates (`${name}`); cannot be COPY'd."),
    ("json", "A parsed JSON value for templates and FOR (a directory: a list of its .json files)."),
    ("yaml", "A parsed YAML value (safe subset; a directory: a list of its .yaml/.yml files)."),
    ("csv", "CSV rows as a list of objects of strings (the header row gives the keys)."),
    ("markdown", "Markdown documents: frontmatter fields plus slug, date, path, content (HTML) and excerpt."),
    ("wbn", "A Web Bundle as data: version, primaryUrl and entries."),
    ("html", "An html fragment (a template partial): evaluated where a RENDER template uses it (`${name}`) and inserted as markup; values inside it are escaped by that RENDER."),
    ("tree", "A directory tree; on a file (zip, tar, tar.gz, wbn), extract the archive."),
    (
        "sitefile",
        "Another Sitefile (a context directory, or `git+https://…#ref:dir`): its named stages are `name+<stage>` in `FROM` and `--from`; its paths resolve inside its own directory; `ARG NAME=value` sets its ARGs.",
    ),
];

/// Hover text of `<import>+<stage>`, a stage of a Sitefile imported AS sitefile.
pub fn imported_stage(import: &str, stage: &str) -> String {
    format!("```sitefile\n{import}+{stage}\n```\nThe final tree of stage `{stage}` of the Sitefile imported as `{import}` (`IMPORT {import} FROM … AS sitefile`). It is built with that Sitefile's own ARGs and context, up to this stage.")
}

const EXPORT_FORMATS: &[(&str, &str)] = &[
    ("directory", "A directory of files (the default)."),
    ("zip", "A zip archive (deterministic: fixed timestamps and modes)."),
    ("tar", "A tar archive."),
    ("tgz", "A gzip-compressed tar archive (alias `tar.gz`)."),
    ("wbn", "A Web Bundle; needs `BASE <https url>` or `--base-url`; `COMPRESSION gzip` gzips it."),
    ("oci", "An OCI image (archive for `-o *.tar`, else a layout directory); `BASE`, `AT` and `COMPRESSION` apply."),
    ("car", "An IPFS CARv1 file (UnixFS, CIDv1); its root CID is the one `ipfs add -r --hidden --cid-version=1` prints."),
    ("html", "`index.html` as one file, every asset it references inlined as a `data:` URI."),
    ("mhtml", "`index.html` and its assets as one MHTML (multipart/related) file; `BASE` sets the parts' URLs."),
    ("warc", "A WARC/1.1 web archive of every file at `BASE`; `COMPRESSION gzip` gzips each record."),
];

const COMPRESSIONS: &[(&str, &str)] = &[
    (
        "gzip",
        "oci: a tar+gzip site layer (the default); wbn: the bundle gzipped (`.wbn.gz`).",
    ),
    (
        "none",
        "oci: a plain tar site layer (digest = diff_id); wbn: the plain bundle (the default).",
    ),
];

const ESCAPES: &[(&str, &str)] = &[
    ("none", "Substitute values as they are (the default)."),
    ("html", "Escape `& < > \" '` in substituted values."),
];

const TYPES: &[(&str, &str)] = &[
    ("string", "Any text (the default)."),
    ("number", "A number; the value must parse as one."),
    ("boolean", "`true` or `false`."),
    ("html", "LET only: an html fragment, a template evaluated where a RENDER template uses it and inserted as markup (`LET name: html = …` or a `<<HTML` heredoc)."),
];

fn lookup<'a>(table: &'a [(&str, &str)], key: &str) -> Option<&'a str> {
    table
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| *v)
}

/// Markdown for an instruction keyword.
pub fn instruction(kw: &str) -> Option<String> {
    INSTRUCTIONS
        .iter()
        .find(|(k, _, _)| k.eq_ignore_ascii_case(kw))
        .map(|(_, sig, text)| format!("```sitefile\n{sig}\n```\n{text}"))
}

/// One-line signature of an instruction (completion detail).
pub fn signature(kw: &str) -> Option<&'static str> {
    INSTRUCTIONS
        .iter()
        .find(|(k, _, _)| k.eq_ignore_ascii_case(kw))
        .map(|(_, sig, _)| *sig)
}

pub fn clause(kw: &str, clause: &str) -> Option<&'static str> {
    CLAUSES
        .iter()
        .find(|(k, c, _)| k.eq_ignore_ascii_case(kw) && c.eq_ignore_ascii_case(clause))
        .map(|(_, _, t)| *t)
}

pub fn flag(name: &str) -> Option<&'static str> {
    lookup(FLAGS, name)
}

pub fn import_kind(name: &str) -> Option<&'static str> {
    lookup(IMPORT_KINDS, name)
}

pub fn export_format(name: &str) -> Option<&'static str> {
    if name.eq_ignore_ascii_case("tar.gz") {
        return lookup(EXPORT_FORMATS, "tgz");
    }
    lookup(EXPORT_FORMATS, name)
}

pub fn compression(name: &str) -> Option<&'static str> {
    lookup(COMPRESSIONS, name)
}

pub fn escape(name: &str) -> Option<&'static str> {
    lookup(ESCAPES, name)
}

pub fn value_type(name: &str) -> Option<&'static str> {
    lookup(TYPES, name)
}

/// Hover text of a `# sitefile-lint:` comment.
pub fn lint_comment() -> String {
    format!(
        "```sitefile\n# sitefile-lint: disable=<rule>[,<rule>…]\n# sitefile-lint: disable-next-line=<rule>[,<rule>…]\n```\nTurns lint rules off for the whole file, or for the next instruction. Rules: {}; `all` names every rule. `sitefile validate --lint-disable` does the same for one run.",
        crate::lint::LINT_RULES.map(|r| format!("`{r}`")).join(", ")
    )
}

/// Hover text of the `# sitefile=N` syntax version directive.
pub fn version_directive() -> String {
    format!(
        "```sitefile\n# sitefile={v}\n```\nThe syntax version this file is written for (this sitefile reads version {v}). A comment before the first instruction, at most once; a file without it is version {v}. A newer version fails with `E_UNSUPPORTED` (upgrade sitefile).",
        v = crate::language::scanner::SYNTAX_VERSION
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::ast::{EscapeMode, ExportFormat, ImportKind, OciCompression, ValueType};
    use crate::language::parser::{
        flags_of, EXPORT_CLAUSES, FEED_CLAUSES, FINGERPRINT_CLAUSES, IMPORT_CLAUSES,
        INSTRUCTIONS as KWS, REDIRECT_CLAUSES, SECRET_FORMS, SITEMAP_CLAUSES, SORT_ORDERS,
        TRANSFORM_CLAUSES,
    };

    #[test]
    fn every_keyword_has_docs() {
        for kw in KWS {
            assert!(instruction(kw).is_some(), "{kw}");
            for (f, _) in flags_of(kw) {
                assert!(flag(f).is_some(), "--{f}");
            }
        }
        for k in ImportKind::ALL {
            assert!(import_kind(k.as_str()).is_some());
        }
        for f in ExportFormat::ALL {
            assert!(export_format(f.as_str()).is_some());
        }
        for c in OciCompression::ALL {
            assert!(compression(c.as_str()).is_some());
        }
        for e in EscapeMode::ALL {
            assert!(escape(e.as_str()).is_some());
        }
        for t in ValueType::ALL {
            assert!(value_type(t.as_str()).is_some());
        }
        for c in IMPORT_CLAUSES
            .iter()
            .chain(&SORT_ORDERS)
            .chain(&SECRET_FORMS)
        {
            assert!(clause("IMPORT", c).is_some(), "{c}");
        }
        for c in EXPORT_CLAUSES {
            assert!(clause("EXPORT", c).is_some(), "{c}");
        }
        for c in TRANSFORM_CLAUSES {
            assert!(clause("TRANSFORM", c).is_some(), "{c}");
        }
        for c in crate::language::parser::RENDER_LOOP_CLAUSES {
            assert!(clause("RENDER", c).is_some(), "{c}");
        }
        for (kw, clauses) in [
            ("SITEMAP", &SITEMAP_CLAUSES[..]),
            ("FEED", &FEED_CLAUSES[..]),
            ("REDIRECT", &REDIRECT_CLAUSES[..]),
            ("FINGERPRINT", &FINGERPRINT_CLAUSES[..]),
            ("CANONICAL", &crate::language::parser::CANONICAL_CLAUSES[..]),
            (
                "PRECOMPRESS",
                &crate::language::parser::PRECOMPRESS_CLAUSES[..],
            ),
            ("ROBOTS", &crate::language::parser::ROBOTS_CLAUSES[..]),
            (
                "SECURITY_TXT",
                &crate::language::parser::SECURITY_TXT_CLAUSES[..],
            ),
            ("PRECACHE", &crate::language::parser::PRECACHE_CLAUSES[..]),
        ] {
            for c in clauses {
                assert!(clause(kw, c).is_some(), "{kw} {c}");
            }
        }
    }
}
