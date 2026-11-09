# Changelog

## Unreleased

- **The version comes from `package.json`** (johnhenry/sitefile#146): `sitefile --version`, the help
  text and the builder version in build attestations said `0.1.0` in both implementations. TypeScript
  now reads `package.json`; Rust reports `rust/Cargo.toml`'s version (now `0.0.5`), and tests on both
  sides (`test/version.test.ts`, `rust/tests/version.rs`) fail when it differs from `package.json`.
  New conformance case `version`. **Attestation bytes change**: `--provenance` statements record the
  real version (`runDetails.builder.version.sitefile`, as does the SBOM's tool component), so their
  digests and OCI referrer digests differ from earlier builds, and will change with every release.
  Image IDs are unchanged.
- **Release binaries** (johnhenry/sitefile#69): a new workflow,
  `.github/workflows/release-binaries.yml`, attaches `sitefile-<version>-<target>.tar.gz` for
  macOS (arm64, x86_64) and Linux (x86_64, arm64; glibc), the VS Code extension
  `sitefile-vscode-<version>.vsix` and `SHA256SUMS` to each GitHub Release. It runs after a
  successful npm publish (or on a hand-made release, or by dispatch with a `dry_run` input) and
  publishes nothing itself; publish.yml is unchanged. No Windows build yet (the Rust port uses Unix
  file APIs). README "Install" covers npm, the prebuilt binaries with checksum verification, and
  `cargo install`; docs/RUST.md and the editor READMEs point at the binaries. The MIT `LICENSE`
  was already present and in the npm package.
- **`serve -d --watch <context>`** (johnhenry/sitefile#306): a detached server can build a Sitefile project. The
  worker runs the in-memory build loop of `serve --watch` (rebuild on change, live reload, error overlay on a
  failed build, the last good build served; a failed first build serves an empty site with the error). It listens
  before the first build, so `serve -d` returns at once. The record keeps the build inputs in a `build` object
  (`sitefile`, `target`, `buildArgs`, `buildArgFiles`, `noCache`, `noLockfile`, `noRewriteBase`) and the page-script
  flags (`noCssInject`, `console`, `noOverlay`, `navigateToChanged`, `keepServiceWorkers`; now also for plain
  directories), so `server restart` keeps them. The `-d --watch` refusals (`… has a Sitefile` and `--<flag> is for
  serve --watch in the foreground, not -d`) are gone; build args are checked when the server is created. Both
  implementations; conformance case `watch/detached-build`; the two `err-serve-detach-watch-*` step cases now cover
  the up-front build-arg and `--rewrite-base` checks.
- **`serve --reload` without `--watch`** (johnhenry/sitefile#317): servers of images, archive files and
  directory snapshots (foreground or `-d`) inject the live-reload script and serve `/__sitefile/reload`, so pages
  left open reload after a restart (`server restart`, a restart on a new image), using the per-process instance ID
  from #305. With `-d` it is kept in the server record as `"reload": "morph"|"full"` (`servers --json`), and
  `-d --watch` now accepts `--reload` too. `--reload` takes its mode only when the next argument is `full` or
  `morph`, so a bare `--reload` works before the operand; `--reload <other>` is now a bare `--reload` followed by
  an operand (write `--reload=<mode>`). Detached live-reload servers now log the CSP warning. Both
  implementations, conformance cases `reload/detached-image` and `reload/foreground`.

## 0.0.5 (2026-11-09)

- **`PRECOMPRESS br`** (johnhenry/sitefile#203): Brotli sidecars `<file>.br`, alone or with gzip
  (`PRECOMPRESS br gzip /`), each written only when smaller. Byte-identical in both
  implementations because both run libbrotli 1.2.0 with the same settings (quality 11, window 22,
  generic mode): node:zlib in TypeScript, Google's C encoder vendored in `rust/vendor/brotli` and
  compiled by `rust/build.rs` in Rust (approved by the owner; building the Rust port now needs a C
  compiler). Checked over about 1,250 inputs (`rust/tests/fixtures/brotli/corpus.mjs`).
  `PRECOMPRESS zstd` is still `E_UNSUPPORTED`.

## 0.0.4 (2026-11-09)

- **`.tar.zst` / `.tzst` archives** (johnhenry/sitefile#221): `IMPORT … AS tree`, `serve`, `verify` and
  `diff` read Zstandard-compressed tars (detected by magic bytes, like gzip), with the same entry,
  size and ratio limits. TypeScript uses node:zlib's `zstdDecompress`; Rust uses the `ruzstd` crate
  (pure Rust, decode only; new dependency approved by the owner, rationale in rust/Cargo.toml).
  Both read what libzstd reads: several frames, skippable frames, checksums; a window over 128 MiB is
  refused. The E_TYPE message no longer calls zstd unsupported.
- **hreflang alternates: `LANGUAGES <code>… [DEFAULT <code>]`** (johnhenry/sitefile#303):
  names the site's language folders, so `/en/about.html` and `/fr/about.html` are the same page
  (pages outside the folders are the DEFAULT language's); documents with frontmatter `lang` and
  a shared `translationKey` (rendered by an earlier `RENDER … FOR`) are paired by key instead,
  for different slugs per language. CANONICAL then adds `<link rel="alternate" hreflang="…">`
  for every translation (plus `x-default` for the DEFAULT language's page), and SITEMAP adds
  `xhtml:link` alternates, also in a split sitemap. Unknown language codes and two pages of one
  language in a group are `E_VALIDATION`. Both implementations, with conformance cases, LSP
  hover and completion, grammars; example `examples/hreflang`.
- **bcrypt entries in `serve --auth-file`** (johnhenry/sitefile#278): `$2y$` (what `htpasswd -B` writes),
  `$2a$` and `$2b$` hashes at cost 4 to 31 are accepted next to `{SHA}`. Keys are the password's UTF-8 bytes
  up to a NUL, at most 72 bytes; digests are compared in constant time. TypeScript has its own
  EksBlowfish (src/bcrypt.ts, no npm dependency); Rust uses the `bcrypt` crate's raw function (new
  dependency approved by the owner, rationale in rust/Cargo.toml). Tested with OpenBSD's and
  htpasswd's vectors in both, and in the `serve-auth` conformance case.
- **Synced browsing: `serve --watch --sync[=scroll]`** (johnhenry/sitefile#309): opt-in, for
  `serve --watch` and `serve -d --watch` (the record keeps it). Each open page reports its path,
  fragment and title (and with `=scroll` its scroll position) to `POST /__sitefile/client`
  (same origin only, size- and rate-limited, no query strings, form contents, keys or cookies);
  a navigation in one page is sent to the others as `{"type":"navigate"}` (and scrolling as
  `{"type":"scroll"}`), which follow unless a form field has focus. Connected pages are listed
  at `GET /__sitefile/clients` (loopback, non-browser clients only) and logged as `client`
  lines (`{"type":"client",…}` with `--log-format json`), for Sitefile Desktop. Without
  `--sync` nothing changes. Both implementations, with conformance cases.
- **`serve -d --watch ./dir`, and pages reload after a server restart** (johnhenry/sitefile#304):
  a detached server can watch a plain directory (no Sitefile): it re-reads it on change and
  live-reloads open pages, CSS swaps included; `--poll`/`--poll-interval` apply. The record keeps
  `watch` and `pollMs`, so `server restart` watches again. Every live-reload stream now starts with
  `id: <instance>`, and a page that reconnects with another instance's `Last-Event-ID` (the server
  restarted) is told to reload. Both implementations, with conformance cases.
- **`IMPORT … AS tree` reads gzip-wrapped Web Bundles** (johnhenry/sitefile#141): after
  gunzip, a Web Bundle is unpacked like a plain `.wbn` instead of failing with `gzip
  content is not a tar archive`, so bundles written by `@johnhenry/fileable`
  (`encode="wbn"`), `@johnhenry/packfile`'s `toArchive()` and `EXPORT … AS wbn COMPRESSION
  gzip` import. Checked-in fixtures from both tools (`test/fixtures/gzip-wbn`) are read
  by both implementations and a conformance case.
- **`bake` names a gzip Web Bundle target's default output like `build`** (johnhenry/sitefile#151):
  a target with `format: wbn` (or `warc`) and no `output`, whose Sitefile exports in that format
  with `COMPRESSION gzip`, now writes `dist/<target>.wbn.gz` (`.warc.gz`) instead of
  `dist/<target>.wbn`. Both implementations, with a conformance case.

## 0.0.3 (2026-11-09)

- **`serve` keys under `--tls-cert`, and `u`'s network URLs** (johnhenry/sitefile#300): `o` and `u`
  use `https://` for an HTTPS server, and `u` lists every interface address (the start line's
  `Network:` list) instead of only the default route's. Both implementations.
- **`serve --mime ext=type` and `--mime-file mime.types`** (the CLI half of
  johnhenry/sitefile#220): custom content types before the built-in table,
  kept in the server record. Both servers.
- **`serve --throttle 3g|slow-3g|<kbit>[/<ms>]`** (johnhenry/sitefile#227):
  per-response latency and bandwidth limits, shown in the start message and kept
  in the server record. Both servers.
- **`serve` compression** (johnhenry/sitefile#167): precompressed `<path>.br`
  / `<path>.gz` variants by `Accept-Encoding` (br first), `--compress` for gzip
  on the fly (byte-identical in both servers), `Vary: Accept-Encoding`, an ETag
  per encoding; ranges stay identity.
- **`serve --tls-cert/--tls-key`** (johnhenry/sitefile#170): HTTPS with your
  certificate (mkcert works), `https://` URLs, paths kept in the server record,
  liveness probe over TLS. No CA is created. HTTP/1.1 only. Both servers (Rust
  through rustls, already in its build).
- **Sharing safety for `serve`** (johnhenry/sitefile#191): `--random-route`
  (a secret path prefix), `--allow-ip`/`--deny-ip` CIDR blocks, `--stop-after`
  (with `expires` in the record), `X-Robots-Tag: noindex` on non-loopback
  binds, and client address and device on access-log lines of non-loopback
  clients. Both servers.
- **`serve --max-connections-per-ip <n>`** (johnhenry/sitefile#178): a
  non-loopback client over the cap gets `503` with `Retry-After: 1`; kept in the
  server record. Both servers.
- **`serve --auth user:password`, `--auth-file`, `SITEFILE_SERVE_AUTH`**
  (johnhenry/sitefile#177): HTTP basic authentication on every request (401
  with `WWW-Authenticate` otherwise), constant-time checks, only PBKDF2
  verifiers in the server record, `auth: true` in `servers --json`, a warning on
  non-loopback binds. htpasswd `{SHA}` entries; bcrypt is a follow-up. Both
  servers.
- **`serve` prints network URLs on `0.0.0.0` / `::`** (johnhenry/sitefile#169):
  a `Network:` line per non-loopback interface address (IPv4 first, IPv6 in
  brackets), and `servers --json` gains `networkUrls`. Both implementations
  (Rust reads the interfaces with `getifaddrs`, no new dependency).
- **`serve --log-format json`, `--log-ip`, `--quiet`** (johnhenry/sitefile#173):
  one JSON object per log line (request, started, stopped, warning, error,
  message), a stable contract in docs/CLI.md, identical in both servers; kept in
  the server record. `server logs --json` converts text logs of older servers.
- **`serve --max-age`, `--immutable`, `--max-age-html`** (johnhenry/sitefile#175):
  preview production `Cache-Control` on files (HTML keeps `no-cache` unless
  `--max-age-html`); `_headers` still wins; kept in the server record. Both
  servers.
- **Portable paths** (johnhenry/sitefile#222): EXPORT warns about exported
  paths Windows cannot hold (reserved names such as `aux.html`, `:` and other
  forbidden characters, a trailing dot or space) and about `_` files and
  folders without `/.nojekyll` (GitHub Pages); `build --strict-paths` makes
  them one `E_PATH_PORTABLE` error. A CHECKLINKS broken link whose target
  differs from a file only in case names that file. Both implementations, with
  conformance cases.
- **MIME: custom content types** (johnhenry/sitefile#220): `MIME <.ext>
  <type/subtype>` adds a `/*.ext` `Content-Type` rule to `/_headers` (in
  source order with HEADER), which `serve`, the hosts and `EXPORT … AS wbn`
  use. Both implementations, with conformance cases. The CLI's `--mime` flag
  is not part of this change.
- **CSP: Content-Security-Policy from inline hashes** (johnhenry/sitefile#219):
  `CSP [POLICY <policy>] [META]` writes a per-page policy to `/_headers` (or a
  `<meta>` with META) that allows each page's inline scripts and styles by
  their SHA-256 instead of `'unsafe-inline'`. Both implementations, with
  conformance cases; example `examples/csp`.
- **Development 404s and what-answered log notes** (johnhenry/sitefile#201):
  a foreground or `--watch` server lists near misses on its 404 page
  (letter-case differences, `_redirects` rules that matched the path but not
  their query or conditions), and access-log lines note a clean URL, the SPA
  fallback or the 404.html that answered. Both implementations.
- **`serve --host-profile` and `--html-handling`** (johnhenry/sitefile#192):
  route like Netlify, Cloudflare Pages, Vercel, GitHub Pages or Firebase
  (canonical page URLs and their redirect status, whether `_redirects` and
  `_headers` are read, which 404 answers, Cloudflare's SPA fallback), or pick
  Cloudflare's `auto`, `force-slash`, `drop-slash` or `none` directly. Kept in
  the server record. Both implementations, with a conformance case per mode.
- **PRECACHE: a service-worker precache manifest** (johnhenry/sitefile#216):
  `PRECACHE <pattern>… [TO <dest>] [INTO <service-worker>]` writes a
  Workbox-style `[{url, revision}]` list (10 hex digits of the SHA-256;
  `null` for fingerprinted files) to `/sw-manifest.json` and/or injects it at
  `self.__WB_MANIFEST`. Both implementations, with conformance cases; example
  `examples/precache`.
- **`serve --rewrite-base <url>`** (johnhenry/sitefile#196): the production
  URL in served HTML, CSS and XML becomes the local origin (and `--base`), so
  a preview with absolute self-links stays local. On by default in
  `serve --watch` (from EXPORT, SITEMAP or FEED BASE; `--no-rewrite-base`
  turns it off); never applied to exports. Both implementations.
- **`serve -d` printed its `Started …` line twice** (a merge of
  johnhenry/sitefile#195): it is printed once, with the `--base` URL.
- **ROBOTS and SECURITY_TXT** (johnhenry/sitefile#217): `ROBOTS [ALLOW|DISALLOW
  <path>]… [SITEMAP]` writes `/robots.txt` (with a `Sitemap:` line for each
  SITEMAP of the Sitefile); `SECURITY_TXT CONTACT <uri>… EXPIRES <date>
  [ENCRYPTION <uri>]… [POLICY <uri>]…` writes an RFC 9116
  `/.well-known/security.txt`, rejects a missing Contact or Expires and warns
  when Expires is near or past. Both implementations, with conformance cases;
  example `examples/robots`.
- **`serve --isolate`** (johnhenry/sitefile#197): COOP `same-origin`, COEP
  `require-corp` (or `--coep credentialless`) and CORP `same-origin` on every
  response, for SharedArrayBuffer and wasm threads; `_headers` still wins; kept
  in the server record. Both servers.
- **`serve --cors`** (johnhenry/sitefile#171): `Access-Control-Allow-Origin: *`
  on every response (`_headers` still wins) and `OPTIONS` preflights answered
  with `204`; kept in the server record. Both servers.
- **`serve` proxies** (johnhenry/sitefile#172): a `_redirects` rule with status
  200 and an `http(s)://` target forwards the request there, as on Netlify,
  and `--proxy /api=http://localhost:3000` does the same for a dev backend.
  This is off unless allowed: a `_redirects` proxy rule only reaches hosts in
  `--allow-hosts`, and any other host gets a `502` naming the flag to add. The
  build fetch policy applies (https unless `--allow-http`, public addresses
  unless `--allow-private-network`, timeouts, every hop checked). Only GET and
  HEAD are proxied, without cookies or credentials, and redirects are passed
  on. `import-config` now also translates rewrites to URLs (Vercel,
  Firebase).

- **`serve` Host allow-list against DNS rebinding** (johnhenry/sitefile#190):
  both servers answer `403` unless `Host` is `localhost`, `*.localhost` or an
  IP address; `--allowed-host <name>` (repeatable; `.example.test` for
  subdomains, `*` for any) adds names and is kept in the server record.
- **PRECOMPRESS writes `.gz` sidecars** (johnhenry/sitefile#203):
  `PRECOMPRESS [--overwrite] [gzip] <pattern>… [MIN <bytes>]` writes
  `<file>.gz` next to matching text files of at least MIN bytes (default 1024)
  when smaller, with the shared deterministic deflate encoder, for nginx
  `gzip_static` and Caddy `precompressed`. `br` and `zstd` are rejected as not
  supported yet. Both implementations, with conformance cases; example
  `examples/precompress`.
- **`serve` in a terminal** (johnhenry/sitefile#224): single keys in a
  foreground `serve` or `serve --watch`:
  - `o` opens the browser;
  - `u` prints the local URL, plus the network URL and its QR code when
    bound to every interface;
  - `r` rebuilds, or re-reads a served directory;
  - `c` clears the screen;
  - `q` quits;
  - `h` lists the keys.

  `--open[=path]` opens the browser at start (`$BROWSER`, else the system
  default). `--qr` prints a QR code of the network URL. The QR encoder is
  shared by both implementations: they check the same golden fixtures and
  the standard's format and version bits.
- **WACZ export** (johnhenry/sitefile#206, WACZ part): `EXPORT / AS wacz BASE
  <url>` (or `-o site.wacz`) writes a deterministic WACZ 1.1.1 package that
  ReplayWeb.page opens: the site's WARC as `archive/data.warc.gz` (the same
  bytes as `AS warc COMPRESSION gzip`), a sorted CDXJ index with each record's
  offset and length, `pages/pages.jsonl` with every HTML page and its
  `<title>`, and `datapackage.json` with `datapackage-digest.json`. Both
  implementations; conformance case `ok-wacz`.
- **`serve --base /repo/`** (johnhenry/sitefile#195): serves the site under
  a path prefix (GitHub Pages project sites): `/` redirects to it, `/repo`
  gets its slash, everything else is 404, and the server's redirects and
  `_redirects` path targets stay under it. `serve --watch` defaults it to the
  path of the EXPORT BASE URL. Kept in the server record and shown in URLs.
  Both implementations.
- **`import-config` reads what serve now understands** (johnhenry/sitefile#204,
  after #193): `:placeholders` in sources and targets (Netlify, Vercel `:name`
  segments, Firebase `:name` and `:name*`), Netlify `query` and `conditions`
  (`Language`, `Country`, `Cookie`), and statuses 410 and 451 are now
  translated rather than listed as unsupported. A target `:name` that the
  source does not capture is still refused.
- **`EXPORT … AS epub`** (johnhenry/sitefile#209): an EPUB 3 book of the
  site's XHTML pages (index first), with a generated package document and
  navigation document (or the site's `/nav.xhtml`), deterministic metadata
  and `mimetype` stored first without an extra field. The zip writer takes a
  list of entries to write first that way, in both implementations. Example
  `examples/book`; conformance `example/book:epub-*`, `case/err-epub-no-page`.

- **`serve --spa` falls back by navigation** (johnhenry/sitefile#194): the
  app shell answers only page navigations (`Sec-Fetch-Mode`/`Sec-Fetch-Dest`,
  else `Accept: text/html`), never subresources, so `/users/jane.doe` works
  and a missing script stays 404. The shell is the nearest `200.html`
  (Surge) or `index.html` (nested apps such as `/admin/`); `--spa-exclude
  <glob>` skips paths and `--spa-fallback <path>` picks one file. Both
  implementations.
- **`EXPORT … FOR <host>`** (johnhenry/sitefile#205): writes the REDIRECT
  and HEADER rules (and FINGERPRINT's cache headers) as Netlify, Cloudflare
  Pages, Vercel (Build Output API v3), Firebase Hosting (`firebase.json`),
  Azure Static Web Apps (`staticwebapp.config.json`), GitHub Pages
  (`.nojekyll`, `CNAME` from `BASE`) or S3 (website configuration and
  per-object metadata) read them. A rule the host cannot express as Netlify
  applies it is `E_EXPORT` naming the rule; `build --lossy` leaves it out
  with a warning. This includes a redirect that a file in the export would
  answer first on Netlify, on hosts that apply redirects first. Both
  implementations, with a conformance case per host.

- **`_redirects`: placeholders, query matching, conditions, 410/451, nearest
  404 and warnings** (johnhenry/sitefile#193): `sitefile serve` understands
  `:placeholders` (`/blog/:year/:slug /posts/:slug`), query parameters
  (`/store id=:id /items/:id`), `Language=`, `Cookie=` and `Country=`
  conditions (`--country` sets the country), statuses 410 and 451, and a
  `404.html` in the nearest directory. Every `_redirects`/`_headers` line it
  ignores is a warning with its line and reason (`--strict-site-files` makes
  them an error), `validate` warns about REDIRECT/HEADER lines serve would
  ignore (lint rule `serve-ignored`), and the access log names the rule that
  answered (`→ /new (_redirects:3)`).
- **CANONICAL links and split sitemaps** (johnhenry/sitefile#218):
  `CANONICAL BASE <url> [PRETTY]` adds `<link rel="canonical">` before
  `</head>` of every page that has none (PRETTY drops `.html`), and SITEMAP
  writes a sitemap index with numbered parts above 50,000 URLs or 50 MiB.
  Both implementations, with conformance cases; example `examples/canonical`.
  hreflang alternates are not done yet.
- **`--oci-layers <n>`** (johnhenry/sitefile#214): split an oci image's files
  over `n` site layers (1-64, by a hash of each path) so pushing a new version
  uploads only the layers whose files changed; `dev.sitefile.layers` in the
  config marks them, and every reader (store, save, serve, push, pull,
  `FROM image`) merges them. Default unchanged (one layer, same image IDs).
  Both implementations; conformance `example/blog:oci-layers-*`,
  `case/steps-oci-layers`.
- **Identical `--help` in both CLIs**: the `--node` line now reads "default
  $SITEFILE_NODE, else the default Node, see docs/PLUGINS.md" in both. The
  default itself differs (the TypeScript CLI's own Node, `node` on `PATH` for
  the Rust binary), and that is now listed in docs/RUST.md "Known
  differences". `save --help` in the Rust binary now lists `car`, `html` and
  `mhtml` too. A `help-text` conformance case compares every command's help.
- **`serve`: `Last-Modified` and `103 Early Hints`** (johnhenry/sitefile#228):
  file responses carry `Last-Modified` (image: when it was stored; directory:
  newest file mtime; archive: its mtime; `--watch`: the build time), and
  `If-Modified-Since` without `If-None-Match` gives `304`. An HTML page whose
  `_headers` `Link` preloads or preconnects is preceded by a `103 Early Hints`
  response with those links (HTTP/1.1 `GET` only). Both servers.
- **`sitefile import-config`** (johnhenry/sitefile#204): translates
  netlify.toml, vercel.json, firebase.json, staticwebapp.config.json,
  wrangler.toml/.json (`[assets]`) and an `.htaccess` subset into the
  `_redirects` and `_headers` that `sitefile serve` understands today. Every
  rule written is checked against serve's parser. Placeholders, conditions,
  regular expressions, proxies and settings with no `_redirects` form are
  listed as unsupported, never half translated. `--json` lists both. It reads
  the TOML files with a small TOML subset shared by both implementations.
  Fixtures for every format are conformance cases.
- **`serve` answers byte ranges** (johnhenry/sitefile#166): `Accept-Ranges:
  bytes` on file responses; a single `Range` on `GET` gets `206` with
  `Content-Range`, an unsatisfiable one `416`; `If-Range` (strong ETag match)
  and `HEAD` are handled; multiple ranges are ignored (the whole file). Both
  servers; `_headers` cannot set `accept-ranges` or `content-range`.
- **`sitefile up`, `down` and `ps`** (johnhenry/sitefile#223): a servers file
  (`sitefile.servers.yaml`, `.yml` or `.json`) names several detached servers
  (`source`, `publish`, `spa`). `up` starts them all, idempotently: it leaves
  running ones alone, restarts exited ones and replaces changed ones. `down`
  stops and removes the ones it started, and `ps` lists them, including those
  not created yet. Sources are relative to the servers file. Server records
  gain an optional `project` field. Both implementations, with conformance
  cases.
- **`serve x.zip` serves in place** (#213): the zip is checked and indexed at
  start and each entry is read from the file per request, so memory stays
  flat for large zips; responses are unchanged, except that gzip clients get
  deflated entries passed through as `content-encoding: gzip` (no
  re-encoding). Library: `openZipFile`/`openZipArchive`/`loadServedSite` and
  `SiteServerOptions.content` (Rust: `open_zip_file`, `open_zip_archive`,
  `load_served_site`, `SiteServerOptions::content`). Both implementations.
- **E_TYPE names the format** (#221): an xz, zstd or bzip2 stream, a 7z or a
  RAR archive given to `IMPORT … AS tree` or `serve` is named in the error,
  with what to convert it to. (zstd decoding is not added: the Rust side has
  no zstd decoder without a new dependency; see #221.) Both implementations.
- **ZIP64 zips are read** (#185): the zip64 end record and locator and the
  0x0001 extra field (central and local) are read, so zips from Python's
  `force_zip64`, Java, Go and Node `archiver` with `forceZip64` import and
  serve; the archive limits still refuse large ones. Writing ZIP64 stays
  unsupported. Both implementations.
- **Zip local headers are cross-checked** (#184): a local header whose name,
  compression method or sizes (without a data descriptor) differ from the
  central directory, or entries whose bytes overlap, are `E_ARCHIVE_UNSAFE`,
  so sitefile never extracts different files than `unzip` or a browser would.
  Directory entries' local headers are now checked too. Both implementations.
- **Zip CRC-32 check** (#183): every extracted zip entry is checked against
  its central-directory CRC-32; a mismatch is `E_ARCHIVE` naming the entry
  (`IMPORT … AS tree`, `serve x.zip`). Both implementations.
- **Zip names without the UTF-8 flag** (#182): names without general-purpose
  bit 11 are read from the Info-ZIP Unicode Path field (0x7075) when its CRC
  matches, else as UTF-8 when valid, else as CP437 (they used to become
  U+FFFD, and two names could collide). Applies to `IMPORT … AS tree` and
  `serve x.zip`, in both implementations.
- **Zip export stores already-compressed media** (#203, writer half): png,
  jpg, webp, avif, woff2, mp4, zip, gz and similar extensions are stored
  without running deflate, so exports of media-heavy sites are faster. Zip
  bytes change only for such entries that deflate had shrunk slightly; tar.gz
  and OCI layers (image IDs) are unchanged. Both implementations, with a
  byte-for-byte conformance case (`zip-stored-media`).
- **Zip export writes the extended timestamp** (#187): every entry carries the
  0x5455 "UT" extra field (mtime only) in its local and central header, so
  `SOURCE_DATE_EPOCH` survives extraction in any time zone (unzippers read
  the DOS time as local time). **This changes zip export bytes**: every
  `-o x.zip` (and `EXPORT … AS zip`) output differs from 0.0.2's, still
  deterministic and identical across the TypeScript and Rust writers (their
  byte-for-byte fixtures are updated together). Image IDs are not affected.
- **Compression Dictionary Transport, experimental** (johnhenry/sitefile#215):
  `build --experimental-dictionary <previous>` adds `Use-As-Dictionary`
  headers for scripts, styles, JSON, wasm and HTML, and a `<path>.dcz`
  Zstandard delta against the previous build for each that changed;
  `serve` answers `Available-Dictionary` + `Accept-Encoding: dcz` with it.
  A small deterministic Zstandard encoder shared by both implementations
  (`src/zstd.ts`, `rust/src/zstd.rs`); frames decode with libzstd.
  Conformance cases `steps-dictionary`, `serve-dictionary`.
- **`EXPORT … AS swbn KEY <secret-id>`** (johnhenry/sitefile#207): a Signed
  Web Bundle for Isolated Web Apps, signed with an Ed25519 PEM key passed as
  `--secret id=<id>,src=key.pem` (never stored). The output is
  byte-identical to `wbn-sign` 0.3.1's; `build` prints the Web Bundle ID.
  The Rust port signs with `ring`'s Ed25519 (already in the build through rustls).
  `--secret` values may now contain line breaks; a value used as an HTTP
  header still may not. Conformance cases `ok-swbn`, `err-swbn-*`.
- **WARC export, import and serve** (johnhenry/sitefile#206, WARC part):
  `EXPORT / AS warc BASE <url>` writes a deterministic WARC/1.1 file (one
  `response` record per file with its `_headers`, directory indexes at their
  directory URL; `COMPRESSION gzip` or `-o x.warc.gz` gzips each record);
  `IMPORT … AS tree` and `sitefile serve` read `.warc` and `.warc.gz`. Both
  implementations; conformance cases `ok-warc`, `ok-warc-gzip`,
  `ok-warc-import`. WACZ follows once the zip writer work lands.
- **DOM-morphing reload with scroll restore** (johnhenry/sitefile#200): when a
  rebuild changed only existing HTML files, the reload event carries
  `"morph":true` and pages update in place (scroll, focus and form input
  stay), falling back to a full reload when their scripts changed. Full
  reloads keep the scroll position. `--reload full` turns morphing off.
- **`serve --watch --navigate-to-changed`** (johnhenry/sitefile#199): after a
  rebuild that changed exactly one page, the reload event carries its URL
  (`{"type":"reload","path":"/posts/hello/"}`) and open pages go there.
- **Build-error overlay and Rebuilding badge** (johnhenry/sitefile#198): the
  `serve --watch` stream sends `{"type":"building"}` when a rebuild starts,
  `{"type":"error",code,message,file,line,column}` when it fails (also to
  pages that connect while it is broken) and `{"type":"ok"}` for a good
  rebuild with identical output. Pages show a badge while rebuilding and a
  dismissible overlay (in a shadow root) on errors; `--no-overlay` turns
  both off.
- **Browser console errors in the `serve --watch` log** (johnhenry/sitefile#174):
  the live-reload client forwards uncaught errors, unhandled rejections,
  `console.error` and `console.warn` (`--console`: also `log` and `info`) to
  `POST /__sitefile/console`, and the server prints them as `browser error: ...`
  lines. Same-origin only (`Origin` must match `Host`), 16 KiB per message,
  50 messages per 10 s.
- **`serve --watch` for a plain directory, with CSS injection**
  (johnhenry/sitefile#168): `serve --watch ./dir` on a directory without a
  `Sitefile` (and without `-f`, `--target` or `--build-arg*`) serves it as it
  is and re-reads it on change. Events are now JSON:
  `{"type":"reload"}`, or `{"type":"css","paths":[...]}` when only existing
  stylesheets changed, in which case pages swap the `<link>` instead of
  reloading (`--no-css-inject` turns that off). A rebuild with identical
  output sends no event.
- **`--poll` for watch mode** (johnhenry/sitefile#225): `build --watch` and
  `serve --watch` take `--poll` (stat polling every 1000 ms instead of file
  system events, for network shares, Docker bind mounts and WSL) and
  `--poll-interval <ms>` (implies `--poll`).
- **Live reload through tunnels and proxies** (johnhenry/sitefile#226): the
  `/__sitefile/reload` stream is sent with `X-Accel-Buffering: no` and
  `Cache-Control: no-cache, no-transform`, and the TypeScript server now sends
  the same `: ping` comment every 15 s as the Rust one, so a buffering or
  idle-cutting proxy passes events through. The client finds the stream
  relative to its own URL and, when an answer is not an event stream (a proxy
  error page), reconnects with backoff (1 s doubling to 30 s).
- **Live reload gets past caching service workers** (johnhenry/sitefile#229):
  before reloading after a rebuild, the `serve --watch` client unregisters the
  page origin's service workers and empties its Cache Storage, so a site with
  a caching service worker shows the new build. `--keep-service-workers`
  turns it off.
- **`sitefile hosts`** (johnhenry/sitefile#176): lists the hosts a build would
  contact (remote IMPORTs, FROM image registries, the BASE registry) from the
  plan alone, with what needs each and its status under `--allow-hosts`,
  `--deny-hosts`, `--network strict` and `--allow-secret-host` (`--json` with
  `needs`). Nothing is fetched. Both implementations, with a conformance case.
- **`serve`: `/about/` no longer 404s when only `about.html` exists**
  (johnhenry/sitefile#188): it redirects (`301`, query kept) to the clean URL
  `/about`, in both servers. `/about.html` still answers 200; which spelling
  is canonical is johnhenry/sitefile#192.
- **`serve` accepts absolute-form request targets** (johnhenry/sitefile#189):
  `GET http://host/path?q HTTP/1.1` is answered like `GET /path?q` (the
  authority is ignored) instead of `400`, in both servers, as RFC 9112
  requires.
- **Extensionless well-known files get their type** (johnhenry/sitefile#180):
  `serve` and Web Bundle export send `/.well-known/apple-app-site-association`
  (and `/apple-app-site-association`), `openid-configuration`,
  `oauth-authorization-server` and `nodeinfo` as `application/json`,
  `webfinger` as `application/jrd+json` and `host-meta` as
  `application/xrd+xml`, instead of `application/octet-stream`. Extension
  matching is ASCII case-insensitive in both implementations.
- **`EXPORT … AS html` and `AS mhtml`** (johnhenry/sitefile#210): a single
  self-contained page. `html` inlines every asset `index.html` references
  (and what its stylesheets reference) as `data:` URIs; `mhtml` writes the
  page and the same assets as one `multipart/related` file under `BASE`
  (default `https://site.invalid/`). A missing asset fails with `E_EXPORT`;
  links, other pages and external URLs stay. Both implementations;
  conformance cases `ok-html`, `ok-mhtml`, `err-html-missing`. The reference
  scanner now records each attribute URL's element (`img src`).
- **`EXPORT … AS car`** (johnhenry/sitefile#208): an IPFS CARv1 file of the
  tree as a UnixFS DAG (raw leaves, 256 KiB chunks, balanced 174-link file
  nodes, CIDv1), the same DAG and root CID as kubo's `ipfs add -r --hidden
  --cid-version=1` (and the same bytes as its `ipfs dag export`). `-o x.car`
  selects it; `build` and `save` print the root CID. Both implementations;
  conformance cases `example/blog:car-bytes`, `case/ok-car`.
- **A fuller content-type table** (johnhenry/sitefile#179): `serve` and Web
  Bundle export type 75 extensions (WebVTT, XSLT, feeds, `.cjs`, source maps,
  fonts, more images, audio, video, HLS/DASH, glTF, archives, ...) instead of
  ~24, so files no longer arrive as `application/octet-stream` under
  `nosniff`. One generated table (`tools/gen-mime.mjs`) feeds both
  implementations. A `serve` conformance case kind (`"command": "serve"`)
  compares both servers' answers to raw HTTP requests.
- **Choose the Node that runs JavaScript plugins** (johnhenry/sitefile#159):
  `--node /abs/path`, `SITEFILE_NODE` or `BuildOptions.node` / `node` names
  the binary (relative paths are a usage error); the default stays the running
  Node (TypeScript) or `node` on PATH (Rust). Before the first plugin starts
  the binary is resolved, its `--version` must be at least 26.0.0 (Node 25
  added network permissions), and `plugins run on node <version> (<path>)` is
  printed. An older binary fails with `E_PLUGIN` at the `PLUGIN` line. `bake`
  passes `--node` through.
- **`sitefile diff`** (johnhenry/sitefile#212): files added, removed and
  changed (with sizes, digests and modes) between two images, directories or
  archives, plus the `_redirects`/`_headers` rules added and removed;
  `--json` and `--exit-code`. Conformance step case `steps-diff`.

- **OS metadata is filtered from archives** (#186): `IMPORT … AS tree` and
  `serve` of a zip, tar or Web Bundle (and `serve ./dir`) drop `__MACOSX/`,
  `._*`, `.DS_Store`, `Icon\r`, `Thumbs.db` and `desktop.ini`, with a warning
  giving the count; `--keep-os-metadata` (`keepOsMetadata`) keeps them. A
  Finder-made zip is now a single top-level folder. Changes the output (and
  image ID) of builds that imported such archives. Both implementations.
- **File manifests and `sitefile verify`** (johnhenry/sitefile#211):
  `build --manifest <file>` and `save --manifest <file>` write every exported
  file's path, size and SHA-256 with an RFC 6962 Merkle root; `sitefile verify
  <dir|archive|image> --manifest <file> [--json]` reports missing, changed and
  extra files and fails with `E_INTEGRITY`. Same bytes from both
  implementations; conformance step cases `steps-verify*`.
- **JavaScript plugins get a memory limit** (johnhenry/sitefile#157): the
  plugin process starts with `--max-old-space-size`, set by the new
  `--plugin-memory <MiB>` flag (default 1024, the same as the wasm linear
  memory cap, which the flag now sets too; `bake` passes it through). A plugin
  that passes it fails with `E_PLUGIN: … ran out of memory (more than the N MiB
  limit)` instead of V8's stack trace. Library: `PluginPolicy.maxMemoryMiB` /
  `max_memory_mib`.
- **Live reload works under a strict CSP and leaves fetch()ed HTML alone**
  (johnhenry/sitefile#181): `serve --watch` injects
  `<script src="/__sitefile/reload.js"></script>` (a same-origin script that
  `script-src 'self'` allows) instead of an inline script, and only into page
  navigations (`Sec-Fetch-Mode: navigate`, else `Sec-Fetch-Dest`, else an
  `Accept` naming `text/html`), so an HTML fragment loaded with `fetch()` is
  served unchanged. A `_headers` CSP that still blocks the script or the event
  stream gets a one-time warning. The client script has one source
  (`src/reload-client.ts`), which the Rust server compiles in. New watch-mode
  conformance cases (`conformance/watch.mjs`) compare both servers' responses
  and events.
- **FINGERPRINT writes cache headers and SRI** (johnhenry/sitefile#202):
  EXPORT adds `Cache-Control: public, max-age=31536000, immutable` for the
  fingerprinted files (a `/dir/*` rule when a directory holds only them) and
  `no-cache` for the pages to `/_headers`, unless `--no-cache-headers` or a
  HEADER sets `Cache-Control`. `FINGERPRINT … INTEGRITY` adds
  `integrity="sha384-…"` to the `<script src>` and
  `<link rel=stylesheet|modulepreload>` tags that load a renamed file. Both
  implementations, with conformance cases.
- **Network gating** (johnhenry/sitefile#176): a host refused because it
  matches no `--allow-hosts` rule now says which flag to add (`; add
  --allow-hosts <host>`) and carries `needs` (`kind: 'host'`,
  `NeedsKind::Host`), like a plugin that was not allowed. Opt-in strict mode,
  `--network strict` or `SITEFILE_NETWORK=strict`, allows only listed hosts
  (none when none are listed) and sends an `IMPORT … SECRET` only to a host
  that `--allow-secret-host <id>=<host>` names (`kind: 'secret-host'`). The
  default stays `open`. With `--json`, a failed command also prints
  `{"error": {code, message, span, needs}}` on stdout, and `outdated --json`
  error rows include `needs`. Both implementations, with conformance cases.
- **JavaScript plugins declare whether they may be cached**
  (johnhenry/sitefile#160): `export const cache = true` says a plugin is
  deterministic; `cache = false` opts out as before; any other value fails
  with `E_PLUGIN`. A JS plugin that exports neither is still cached, with one
  warning per build when results are cached or memoized. The examples' JS
  plugins export `cache = true`. docs/PLUGINS.md now says that module state
  carries over between calls and lists what is trusted as-is (`--cache-from`
  results, the local TRANSFORM cache, the Rust wasm module cache); the README
  security model links it.
- **`INTEGRITY` on a JavaScript plugin pins its folder** (johnhenry/sitefile#158):
  a JS plugin can read and import every file in its folder, so its
  `INTEGRITY` is now the folder digest (`sha256:` + the digest the TRANSFORM
  cache already uses) instead of the module file's. A changed helper, data
  file or vendored dependency fails with `E_INTEGRITY`, the error gives the
  folder's current digest, and a pinned folder may not contain symlinks.
  **Breaking:** an existing pin on a JS plugin (the module file's digest)
  fails until replaced with the digest from the error. Wasm pins are
  unchanged.

## 0.0.2 (2026-11-08)

- **Plugin console output keeps its order** (johnhenry/sitefile#164): a
  plugin's console lines are written before its result, in both the
  TypeScript and Rust runners, so they always become build warnings (a race
  dropped some). Fixed in 5689229 (#164).
- **Docs: the tool is "sitefile".** The product name is lowercase where it
  stands alone; "Sitefile" stays for the file (`Sitefile`, `Sitefile.lock`, "a
  Sitefile") and the language. The desktop app is Sitefile Desktop.

## 0.0.1 (2026-11-08)

- **`serve` serves archive files** (johnhenry/sitefile#155): `sitefile serve
  site.zip` (and `serve -d`) serves a zip, tar, tar.gz/tgz, Web Bundle or OCI
  image archive (`build -o x.oci.tar`, `save`) the way it serves a directory.
  The format comes from the bytes; a path-like argument that is a file is an
  archive, and a bare name is an archive file when no image has that name.
  Zip, tar and wbn are extracted with the default archive limits and served
  without hidden files (as a directory is); an OCI archive serves its first
  image as the store would, with every blob checked against its digest. The
  file is read again on every start and `server restart`. Server records gain
  `{"kind": "archive", "path": …}` (older records load unchanged); Rust adds
  `ServerSource::Archive`, `servers::load_archive_tree` and
  `load_archive_tree_with`, with every existing signature unchanged. A file
  that is not an archive is `E_TYPE` naming it. docs/CLI.md, "Site servers".
- **Explicit network settings: proxy, host rules, connect/read timeouts**
  (johnhenry/sitefile#156), for networks that need a proxy and for sitefile
  Desktop's Settings screen. `--proxy` / `SITEFILE_PROXY` (an `http://` proxy:
  a CONNECT tunnel for https, absolute form for http; `HTTP(S)_PROXY` is still
  never read), `--no-proxy` / `SITEFILE_NO_PROXY` (`NO_PROXY` syntax),
  `--allow-hosts` / `SITEFILE_ALLOW_HOSTS` and `--deny-hosts` /
  `SITEFILE_DENY_HOSTS` (checked on every hop, registries included; deny beats
  allow; the new code `E_HOST_BLOCKED` names the rule), and `--connect-timeout`
  / `--read-timeout` / `SITEFILE_FETCH_TIMEOUT`, on `build`, `validate`,
  `inspect`, `graph`, `why`, `pull`, `push`, `vendor` and `outdated`. A proxy
  never widens the fetch policy: the target is resolved and checked against
  the private-address rule before the proxy is asked. Embedders pass the same
  settings as `FetchPolicy.network` / `RegistryOptions.network`
  (`NetworkSettings`, additive in both implementations). docs/CLI.md,
  "Network"; `test/network.test.ts`, `rust/src/resources/network.rs`, and the
  `net-proxy*`, `net-no-proxy`, `net-allow-hosts` and `net-deny-hosts`
  conformance cases. Added in c131eb3 (#162).

- **Library API for embedders** (johnhenry/sitefile#153), found by embedding
  the crate in Sitefile Desktop. Rust: `servers::serve_detached`,
  `restart_detached`, `start_detached`, `stop_server`, `resolve_source`,
  `parse_publish` and `DetachedServe` (the CLI's `serve -d`, `server restart`
  and `server stop` now call them); `executor::store_build_as_image` with
  `ImageInfo` and `StoredImage` (what `build -t` calls);
  `BuildOptions::store_home`, so `FROM <local image>`, `BASE` and the wasm
  module cache use an embedder's store instead of `$SITEFILE_HOME`;
  `BuildOptions::cancel` with `CancelToken`, checked between imports, stages,
  operations and plugin or tool calls, failing with the new code
  `E_CANCELLED`; detached workers are reaped on a background thread, so a
  long-lived host can stop them without waiting out the grace period.
  Both implementations: TRANSFORM cache entries record their PLUGIN or TOOL
  (`"plugin"`, same bytes in both; older entries are still read, keys are
  unchanged), with `usage_by_plugin`/`usageByPlugin`,
  `prune_plugin`/`prunePlugin` and `sitefile cache prune --plugin <name>`; the
  `E_UNSUPPORTED` error of a PLUGIN or TOOL that is not allowed carries
  `needs` (kind, name, flag), with the message unchanged. CLI output is
  otherwise unchanged. docs/RUST.md, "Embedding the crate";
  `rust/tests/embed.rs`. Added in e0c555d (#154).

- **`EXPORT … AS wbn COMPRESSION gzip`** (johnhenry/sitefile#143): a Web Bundle
  export can now be gzip(`application/webbundle`), the `.wbn` archive format of
  `@johnhenry/packfile` (`toArchive`/`fromArchive`) and `@johnhenry/fileable`
  (`encode="wbn"`). `COMPRESSION` used to be `E_VALIDATION` on anything but
  `oci`; it now applies to `oci` and `wbn`, and on `wbn` defaults to `none`, so
  existing exports are byte-for-byte unchanged. The gzip layer only wraps the
  finished bundle and uses the shared deterministic encoder (`src/deflate.ts`,
  `rust/src/deflate.rs`), so both CLIs write identical bytes. On the CLI,
  `-o site.wbn.gz` selects a gzipped wbn (also for `sitefile save`), the
  default destination of such a Sitefile is `dist.wbn.gz`, and the new
  `--compression gzip|none` overrides `EXPORT … COMPRESSION` for the export's
  format (oci or wbn; any other format is a usage error); `--oci-compression`
  stays as the oci-only spelling. A Sitefile's `COMPRESSION` belongs to its own
  `AS` format: `--format wbn` on an `AS oci COMPRESSION …` Sitefile does not
  inherit it. `test/packfile-interop.test.ts` now reads a gzip export built with
  `BASE https://packfile.invalid/` through plain `fromArchive(buffer)`; new
  conformance cases compare the bytes of both implementations. Added in
  3c537de (#149).

- **Example: a fileable plugin for TRANSFORM** (`examples/fileable/`,
  johnhenry/sitefile#145). A JavaScript plugin renders a `@johnhenry/fileable`
  tree into the build with fileable's in-memory `toFiles()` and returns
  `{ files }`, so a JSX file tree needs no directory or zip on disk in
  between. fileable stays out of sitefile's dependencies: it is a
  devDependency, and `examples/fileable/vendor.mjs` copies the files its
  browser entry point loads into the plugin folder (gitignored; nothing
  generated is committed), the only folder a plugin can read. Its `cmd`
  attribute and other features that need files, processes or the environment
  fail with a clear `E_PLUGIN`. Checked by `npm run examples` and by
  conformance (both CLIs). docs/PLUGINS.md, "Example: rendering a fileable
  tree". Added in 0b5c4b3 (#147).

- **`EXPORT … AS wbn` bakes HEADER and exact REDIRECT rules into the bundle's
  responses** (johnhenry/sitefile#144, #150). Each response gets the headers of the
  `/_headers` blocks matching its URL, with `sitefile serve`'s matching
  (`serve-rules.ts`; a directory URL like `<base>docs/` matches with its
  slash and gets its own response when its headers differ). Exact-path
  `/_redirects` rules with a 3xx status become responses with that status, a
  `location` and an empty body, in `serve`'s order (a forced `301!` replaces
  the file; other rules apply only where `serve` would find nothing). Splats,
  `200` rewrites and `404` rules stay in `/_redirects`, which, like
  `/_headers`, is still in the bundle. Bundles of trees without those files
  are byte-identical to before. packfile's `createWebBundleRouter()` now
  answers with the HEADER values and redirects (`test/packfile-interop.test.ts`);
  conformance cases `ok-wbn-rules` and `ok-wbn-rules-files` compare both
  implementations' bytes. The TypeScript `headersFor` no longer drops a
  header named `__proto__` (the Rust port never did); this also
  changes the TypeScript `sitefile serve`, which now sends such a header as
  the Rust binary does. Added in 429ab24 (#150).

## 0.0.0 — first npm release (2026-11-06)

`@johnhenry/sitefile` is a new package: sitefile was never published to npm
under any name, and `0.0.0` is its first version. It joins the @johnhenry
family as its static build orchestrator, next to `@johnhenry/fileable` (file
trees in JSX) and `@johnhenry/packfile` (the Web Bundle reader and server).

- **Published as `@johnhenry/sitefile`** (was `sitefile`, `private: true`,
  `0.1.0` in `package.json` only). The tarball ships `lib/src/` (compiled by
  `prepack`, no source maps), the README, `LICENSE` and this file; it has no
  runtime dependencies. The Rust port stays at `0.1.0` in `rust/Cargo.toml`
  and is not published (johnhenry/sitefile#69).
- **MIT `LICENSE` file added.** `package.json` and `rust/Cargo.toml` already
  said MIT. See johnhenry/sitefile#69.
- **Node floor raised from 22 to 26** (`engines`, `.nvmrc`, CI), the family
  floor. The suite passed unchanged on Node 26.
- **CI on every push and pull request** (johnhenry/sitefile#67): typecheck,
  tests and the examples smoke test (`npm run examples`), the Rust
  fmt/build/test (clippy non-blocking until triaged), conformance between the
  two implementations with the release binary, the VS Code extension and
  tree-sitter grammar tests, actionlint, and the family's workflow lint.
  `publish.yml` publishes from `main` through npm trusted publishing.
- **Interop with `@johnhenry/packfile` is tested**
  (`test/packfile-interop.test.ts`, packfile as a devDependency only): packfile
  reads a Sitefile `.wbn` back into exactly the exported files and serves it,
  and its `fromArchive()` reads the same bundle once gzipped. The other
  direction, importing gzip-wrapped bundles from fileable or packfile with
  `AS tree`, is johnhenry/sitefile#141.
- **Fixed: a replaced archive destination went unnoticed on Linux.** Before
  renaming the new archive into place, `exportFile` (and the Rust
  `export_file`) checked that the existing destination was still the file
  they inspected by device and inode number only. Linux often gives a file
  created right after an unlink the same inode number, so a destination
  swapped mid-export passed the check and was replaced. The check now also
  compares size and nanosecond `ctime`. Found by the new CI's first Linux
  run; the existing test only ever passed on macOS. Fixed in 0e0a1a1 (#142).
- **Fixed: a case-only collision inside one directory had no source line.**
  Two names that differ only in case in a directory read by `COPY` (possible
  on a case-sensitive file system) failed with `E_PATH_COLLISION` but without
  the instruction's line; both implementations now report it. Its test only
  runs on case-sensitive file systems, so it had never run. Fixed in 0e0a1a1
  (#142).
- **Docs:** README badges, `## Contents`, `## Install`, `## Adding a new
  plugin`, `## Security model` (the former "Safety defaults", with what stays
  the user's responsibility), `## Family`; `examples/README.md`;
  `AGENTS.md` reshaped into the family playbook with `CLAUDE.md` as a symlink.
