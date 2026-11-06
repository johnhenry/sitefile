# CLI reference

`sitefile <command> --help` prints the options of each command. Exit codes: `0` success, `1` an error
(`error[E_CODE]: message`), `2` a usage error.

## Building

```
sitefile build [context] [-f Sitefile] [-o out] [--format fmt] [-t name[:tag]]... [options]
sitefile validate [context] [--json]
sitefile inspect  [context] [--json]
```

`build` accepts these image-related options on top of the ones in the README:

| Option | Meaning |
| --- | --- |
| `-t, --tag name[:tag]` | also store the build as an image in the local store (repeatable). **With `-t` and no `-o`, only the store is written and nothing is created in the context; with both, both happen.** |
| `--format oci` / `-o x.oci.tar` | write an OCI image: an OCI archive for a `.tar` destination, an OCI layout directory otherwise (`dist.oci.tar` by default) |
| `--base <image>` | build the oci image on `<image>` (runnable; overrides `EXPORT ... AS oci BASE`); a bare name means Docker Hub. Pin it: `nginx:alpine@sha256:...` |
| `--insecure-registry host[:port]` | allow plain http to that registry (repeatable) |
| `--oci-root /path` | directory of the files inside the image layer (default `/srv/www`; overrides `EXPORT ... AT`) |
| `--oci-compression gzip\|none` | site layer encoding: `gzip` (default, `tar+gzip`) or `none` (plain `tar`, digest = diff_id); overrides `EXPORT ... COMPRESSION` |
| `--platform os/arch` | image platform metadata (default `linux/amd64`) |
| `--label key=value` | add an OCI image config label (repeatable; overrides defaults such as `org.opencontainers.image.title`) |
| `--git-labels` | add `org.opencontainers.image.source`/`.revision` from the git checkout. Off by default: they change the image ID on every commit, and finding the checkout reads outside the context. Credentials in remote URLs are dropped. |

`build`, `validate` and `inspect` also accept:

| Option | Meaning |
| --- | --- |
| `--allow-exec name[=/abs/path]` | allow a `TOOL name` (an external program) to run; repeatable. A bare name is looked up on `PATH` when the build starts and the resolved real path is printed (`tool ffmpeg -> /opt/homebrew/bin/ffmpeg`). A `TOOL` that is not allowed fails with `E_UNSUPPORTED` naming the flag to add. The program runs **with your privileges**: allow only what you trust. |
| `--allow-plugin name` | allow the JavaScript `PLUGIN name` to run; repeatable. A JS `PLUGIN` that is not allowed fails with `E_UNSUPPORTED` at its line, naming the flag to add, before any import is fetched or any plugin process starts. A name that matches no `PLUGIN` is a warning. WebAssembly plugins need no flag. `validate` never runs plugins; `inspect` builds in memory, so it needs the flag like `build`. |
| `--allow-plugins` | allow every JavaScript `PLUGIN` in the Sitefile. Use it only for a Sitefile you trust as much as its plugin code. |
| `--no-cache` | do not read or write the TRANSFORM result cache for this build |
| `--plugin-timeout ms` | per-call time limit for TRANSFORM plugins and tools (default 60000) |

## Transform cache

TRANSFORM results (plugins and `TOOL`s) are cached in `$SITEFILE_HOME/cache/transform` (default `~/.sitefile/cache/transform`),
beside the image store but separate from it (`image prune` does not touch it). See [PLUGINS.md](PLUGINS.md#caching).

```
sitefile cache info [--json]              location, entries, blobs, size
sitefile cache prune [--older-than days]  delete every cached result, or only entries unused for N days
sitefile cache prune --max-size <size>    apply the size limit below now, with that size
sitefile cache du [--json]                size of every cache and the store, and what may be evicted (alias: cache ls)
```

### What lives under `$SITEFILE_HOME`

| Path | What | Safe to delete? |
| --- | --- | --- |
| `cache/transform/entries/<key>.json`, `cache/transform/blobs/<hex>` | TRANSFORM results (both implementations) | yes: a pure cache, rebuilt on the next miss. **Evicted automatically.** |
| `cache/wasm/<key>.cwasm` | compiled wasm plugins (Rust build only; [RUST.md](RUST.md#wasm-module-cache)) | yes: a pure cache, recompiled on the next miss. **Evicted automatically.** |
| `index.json`, `oci-layout`, `blobs/sha256/<hex>` of **tagged** images | the image store: your images | **no**. Never evicted automatically; `rmi` removes tags. |
| untagged images (`<none>`: a re-tagged build's old image, a BASE image pulled by digest) and blobs no image uses | the image store's garbage and pulled BASE images | yes: `image prune`, or **evicted automatically** (a BASE image is pulled again when needed). |
| `blobs/sha256/.tmp-*`, `*.tmp` | writes in progress (temp file + rename) | only when old (1 hour); a running pull or build may own them. |
| `servers/<id>.json`, `servers/<id>.log` | site server state and logs | stopped servers: `server rm`. Never evicted. |
| `lock` | held while the store is modified (stale locks are broken) | never by hand while sitefile runs. |

Remote `IMPORT`s (https URLs) are not cached on disk: they are fetched on every build and pinned by `Sitefile.lock`
(integrity), so there is no remote resource cache to clean. BASE images are pulled into the image store (above).

### Cache size limit

At the end of every `build` and `inspect`, Sitefile keeps the **evictable** data (TRANSFORM results, compiled wasm
modules, untagged images and unreferenced blobs) under a size limit by deleting the least recently used items first.
Tagged images, and every blob a tagged image uses, are never deleted and do not count against the limit.

- `SITEFILE_CACHE_MAX=<size>` sets the limit: bytes (`1073741824`) or a number with a binary unit, case-insensitive
  (`512K`, `500M`, `1.5G`, `2GiB`, `1T`). **Default `1G` (1 GiB). `0` turns automatic eviction off.** An invalid value
  is a warning and the default applies. `cache prune --max-size <size>` runs the same eviction on demand.
- **Recency is the file's mtime**, refreshed on every use: a TRANSFORM hit touches its entry, a wasm cache hit its
  `.cwasm` file, storing or pulling an image its manifest and reused blobs. atime is not used (it is often disabled
  with `noatime`/`relatime`), and an index file would be one more thing to keep consistent between concurrent builds.
- **Safe with concurrent builds:** every write is a temp file + rename, so a half-written file is never visible; temp
  files are only removed after 1 hour; nothing touched in the last **10 minutes** (TRANSFORM results, wasm modules)
  or **1 hour** (image store) is evicted, so a build never loses what it, or another build, just wrote or used; the
  image-store part runs under the store lock. The cache can therefore briefly exceed the limit by what was written
  within the grace period.
- **Cheap when under the limit:** one directory listing per cache, no file is read.
- **Never fails a build:** a problem (an unreadable store, a corrupt `index.json`) is a `warning: cache size limit
  not applied: ...` and the command's exit code is unchanged. When something is evicted, one line goes to stderr:
  `Evicted 3 cache items (412MB) to stay under SITEFILE_CACHE_MAX=1GiB`.

`cache du` prints one row per cache (`transform`, `wasm`, `store`, `servers`) with `ITEMS` (entries, modules, blobs,
files), `SIZE`, `EVICTABLE` and the path, a total, and the limit. `--json` gives
`{home, limit, caches: [{name, path, items, bytes, evictable}], bytes, evictable}` (sizes in bytes, `limit` 0 = none).

## Image store

The store lives in `$SITEFILE_HOME`, or `~/.sitefile`. Tests and scripts should always set `SITEFILE_HOME`.
It **is an [OCI image layout](https://github.com/opencontainers/image-spec/blob/main/image-layout.md)**
(image-spec v1.1):

```
$SITEFILE_HOME/
  oci-layout            {"imageLayoutVersion":"1.0.0"}
  index.json            image index; tags are entries annotated org.opencontainers.image.ref.name = "name:tag"
  blobs/sha256/<hex>    manifest, config and layer blobs (identical blobs are stored once)
  servers/<id>.json     site server state (see below)
  servers/<id>.log      site server log
  lock                  lock file held while the store is modified
```

- An **image** is one manifest, one config and one layer. The layer is a deterministic tar.gz of the site's files
  under a root directory (`/srv/www` by default). See *Export formats > oci* in [LANGUAGE.md](LANGUAGE.md#export-formats).
  The image holds **files only**: it has no entrypoint and is not runnable on its own.
- The **image ID is the manifest digest** (`sha256:<hex>`; shown as the first 12 hex digits). The same files, root,
  platform and labels give the same ID, so rebuilding identical output does not create a new image.
- An image without a tag is *dangling* (an `index.json` entry without a ref name); it shows as `<none>`. Building a
  tag again with different content leaves the old image dangling until `image prune` (or until the
  [cache size limit](#cache-size-limit) evicts it).
- `index.json` also records, per entry, `dev.sitefile.created`: when the image was first stored (bookkeeping only; it
  is not part of any digest, and is what `CREATED` shows).
- Writes are temp file + rename; modifications hold the lock file (stale locks of dead processes are broken), so
  concurrent `sitefile` calls do not corrupt the index. Every blob is digest-checked when read; a mismatch, a missing
  blob or a bad diff_id is `E_STORE`. Build-arg values are never stored.

### Reference grammar

```
[registry-host[:port]/]name[:tag][@sha256:<64 hex>]
name = component ('/' component)*      component = [a-z0-9]+([._-][a-z0-9]+)*
tag  = [A-Za-z0-9_][A-Za-z0-9_.-]{0,127}        (default: latest)
```

The first path component is a registry host when it contains `.` or `:` or is `localhost`. References are validated
everywhere (`E_USAGE` for a bad one). Locally a registry-qualified name is just a tag
(`sitefile tag blog:1 localhost:5000/blog:1`), stored as `registry/name:tag`. Docker Hub aliases (`docker.io`,
`index.docker.io`, `registry-1.docker.io`) are one registry (`docker.io`; its API host is `registry-1.docker.io`), and a
Hub name without a namespace lives in `library/`. **A name without a registry host defaults to Docker Hub only for
`pull` and `BASE`** (`nginx:alpine` is `docker.io/library/nginx:alpine`); everywhere else, and for `push`, it is a
local name. A **short image ID** (12 to 64 hex digits, optionally `sha256:`-prefixed) resolves when it is unique.

### Registries: `push` and `pull`

```
sitefile tag blog:1 localhost:5000/team/blog:1
sitefile push localhost:5000/team/blog:1
sitefile pull localhost:5000/team/blog:1          # or name@sha256:... ; nginx:alpine means Docker Hub
sitefile pull --platform linux/arm64 nginx:alpine
```

The client speaks the [OCI distribution spec](https://github.com/opencontainers/distribution-spec) v1.1: a `/v2/`
probe, then Bearer-token auth (the `WWW-Authenticate` realm/service/scope; an anonymous token first for pulls, credentials
when the registry rejects it, and straight away for pushes) or Basic auth.

- **push** HEADs every blob and uploads only the missing ones (monolithic `POST` + `PUT`); layers of a `BASE`
  image are *mounted* from the base's repository when it is on the same registry; then the manifest is `PUT` and the
  digest the registry returns is checked against ours. The pushed manifest digest equals the local image ID.
- **pull** accepts OCI and Docker v2 manifests and indexes/manifest lists (for an index the `--platform` variant,
  default `linux/amd64`), checks every blob's sha256 and size *while streaming* into a temp file (nothing unverified is
  ever stored), and a manifest fetched by digest must hash to it. The platform manifest is stored unchanged (its digest
  is the image ID) and tagged as written. Pulled third-party images are listed, pushed and used as `BASE`; only images
  built by sitefile can be `serve`d or `save`d as a site.
- **Limits**: `--max-download-bytes` per blob (default 2 GiB), 8 GiB per image, 4 MiB per manifest or token response,
  `--timeout` idle time per request (default 30 s), 30 minutes per blob transfer.
- **Transport**: https. Plain http only for loopback registries (`localhost`, `127.0.0.0/8`, `::1`) and hosts named with
  `--insecure-registry host[:port]` (repeatable; or `$SITEFILE_INSECURE_REGISTRIES`, comma separated); those try https
  first and fall back to http. The registry host itself may be a private address (you named it). Redirects (registries
  send blob downloads to a CDN) are followed manually, at most 5, each hop re-checked: a target on another origin must be
  https and have no credentials in its URL, and must resolve to public addresses (checked at connect time) unless
  `--allow-private-network`; the `Authorization` header is **never** sent to another origin. Token-endpoint realms
  obey the same rules. Compressed transfer encodings are refused.
- **Credentials**, first match wins: `--username` with `--password-stdin` (never a password on the command line);
  `$SITEFILE_REGISTRY_AUTH`, a JSON object such as `{"ghcr.io":"user:token","reg.example.com":{"username":"u","password":"p"}}`;
  then the Docker config (`$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`): `credHelpers[host]`, then
  `auths[host]` (`auth` base64 `user:pass`, `username`/`password`, or an `identitytoken` exchanged at the token
  endpoint), then `credsStore`. Docker Hub is found under `docker.io`, `index.docker.io` or `https://index.docker.io/v1/`.
  **Credential helpers execute a program**: only when the Docker config names one, as `docker-credential-<name> get`
  found on `PATH` (the name must be a plain word, no shell is involved), with the server address on stdin and nothing
  secret on argv. Credentials are fetched only when a registry asks for authentication, and tokens/passwords are kept in
  memory and never printed or put in an error message.
- Errors: `E_AUTH` (credentials missing/rejected), `E_NOT_FOUND` (404), `E_INTEGRITY` (digest mismatch), `E_TOO_LARGE`,
  `E_PROTOCOL`/`E_SSRF_BLOCKED` (transport policy), `E_REGISTRY` (anything else the registry says).

To run a pushed or pulled site image, use it as `COPY --from=` source or serve it with `sitefile serve`; for a runnable
container image, build on a base with `BASE` (see [LANGUAGE.md](LANGUAGE.md#export-formats)).
The default platform of images is `linux/amd64` (reproducible, metadata only for files-only images); on an arm64 host
Docker warns about the mismatch. Use `--platform linux/arm64` to label them for arm64.

### Commands

| Command | Description |
| --- | --- |
| `images [--json] [--digests] [-q]` | `REPOSITORY TAG IMAGE ID CREATED FILES SIZE`, one row per tag. `--digests` adds the full ID, `-q` prints short IDs only. Alias: `image ls`. |
| `image inspect <ref\|id> [--json]` | ID, tags, platform, root, layer/config digests, labels, and the files (the layer is fully verified). |
| `tag <src> <dst>` | add or move the local tag `<dst>` to image `<src>`. |
| `rmi [-f] <ref\|id>...` | remove tags. The image leaves the index when no tag is left. An ID with several tags needs `-f`. Alias: `image rm`. |
| `image prune` | drop untagged images, delete unreferenced blobs, print the space reclaimed. |
| `save <ref\|id> -o out [--format fmt] [--base-url url] [--force]` | export an image. Formats as in `build -o`: a directory, `.zip`, `.tar`, `.tar.gz`/`.tgz`, `.wbn`; **`.oci.tar` (or `--format oci` with a `.tar` destination) writes the stored image itself as an OCI archive, and `--format oci` with any other destination as an OCI layout directory** (same digests as the store). A non-empty destination needs `--force`. |
| `image diff <a> <b>` | `A`/`D`/`C` lines for added, removed and changed paths. |

## Language server

```
sitefile lsp [--stdio]
```

Runs the Sitefile language server (LSP 3.17, JSON-RPC over stdin/stdout) for editors: diagnostics from the real
parse/validate/plan pipeline, completion, hover, go to definition, references, rename and document symbols. `--stdio`
is accepted (clients pass it) and is the only transport. It exits 0 after `shutdown` + `exit`. See [LSP.md](LSP.md).

**Rust binary only** (a tooling exception, see [RUST.md](RUST.md#tooling-exception-the-language-server)): the
TypeScript CLI prints a usage error pointing to `rust/target/release/sitefile lsp` and exits 2.

## Site servers

```
sitefile serve [--name n] [-p [host:]port] [-d] [--spa] <ref|id|./dir>
sitefile servers [-a] [--json]                   # also: sitefile server ls
sitefile server stop [--time secs] <name|id>...
sitefile server rm [-f] <name|id>...
sitefile server logs [-f] [-n lines] <name|id>
sitefile server restart <name|id>
```

`servers` lists `NAME SITE URL STATUS ID`, where SITE is the image reference or directory being served
(`--json` calls it `site`).

`serve` serves an image from the store (the files under its root), or a directory (an argument starting with `./`,
`../` or `/`, or ending in `/`) read once at start through the sandboxed local reader: a symlink leaving the
directory is an error, and hidden files (`.git`, `.env`, ... except `.well-known`) are not served.

- Binds `127.0.0.1` and a free port unless `-p` is given. `-p 8080`, `-p 127.0.0.1:8080`, `-p [::1]:8080`.
  **`0.0.0.0` (or `::`) is bound only when you write it in `-p`** (a warning is printed). A port in use is
  `error[E_SERVER]`.
- Foreground by default: prints the URL and one access-log line per request (`GET /path 200 1234 1.2ms`); Ctrl-C
  stops it. Foreground servers are not listed by `servers`; `--name` needs `-d`.
- Behaviour: `GET` and `HEAD` only (`405` with `Allow` otherwise); `/` and `/dir/` serve `index.html`; `/dir`
  redirects (`301`) to `/dir/`; no directory listings; `404` with a small page; `content-type` from the extension;
  `ETag` is the file digest and `If-None-Match` gives `304`; `cache-control: no-cache`;
  `x-content-type-options: nosniff`. Paths are percent-decoded per segment; `..`, an encoded `/`, NUL, control
  characters and backslashes are `400`. `--spa` serves `/index.html` for unknown paths whose last segment has no
  file extension (a missing `/app.js` stays `404`).
- `-d` detaches. A server is tracked by `$SITEFILE_HOME/servers/<id>.json` (`id`, `name`, `image`, `source`, `spa`,
  requested `bind`, `host`, `port`, `pid`, `started`, `logFile`, `ready`) and `<id>.log`. `serve -d` writes the
  state, spawns a detached `node serve-worker.js <id>` with stdout/stderr on the log, and waits until the worker
  has recorded its pid and port (a failure to start is reported with the worker's error and leaves no entry).
  `servers` never trusts the file alone: **running** means the pid is alive *and* the port answers with this server's
  `x-sitefile-server: <id>` header; anything else shows as **exited**. IDs are 12 hex digits, names are unique
  (auto names look like `site-1a2b3c`).
- `stop` sends `SIGTERM`, waits `--time` seconds (default 5), then `SIGKILL`; stopping an exited server is a no-op.
  `rm` refuses a running server unless `-f`. `restart` stops and starts it again with the same name and bind
  address, serving the same image ID (a directory is re-read). `logs -f` follows the log until Ctrl-C.
