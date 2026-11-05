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
| `--oci-root /path` | directory of the files inside the image layer (default `/srv/www`; overrides `EXPORT ... AT`) |
| `--platform os/arch` | image platform metadata (default `linux/amd64`) |
| `--label key=value` | add an OCI image config label (repeatable; overrides defaults such as `org.opencontainers.image.title`) |
| `--git-labels` | add `org.opencontainers.image.source`/`.revision` from the git checkout. Off by default: they change the image ID on every commit, and finding the checkout reads outside the context. Credentials in remote URLs are dropped. |

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
  tag again with different content leaves the old image dangling until `image prune`.
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

The first path component is a registry host when it contains `.` or `:` or is `localhost`. References are parsed and
validated everywhere (`E_USAGE` for a bad one), but **only local references are usable**: a registry host fails with
`E_UNSUPPORTED` ("registries are not supported yet"). A **short image ID** (12 to 64 hex digits, optionally
`sha256:`-prefixed) resolves when it is unique.

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
