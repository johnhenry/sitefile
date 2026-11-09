# CLI reference

`sitefile <command> --help` prints the options of each command. Exit codes: `0` success, `1` an error
(`error[E_CODE]: message`), `2` a usage error.

## Building

```
sitefile build [context] [-f Sitefile] [-o out] [--format fmt] [-t name[:tag]]... [--target stage] [options]
sitefile validate [context] [--json] [--target stage] [--lint-disable rule[,rule…]]
sitefile inspect  [context] [--json] [--target stage]
sitefile graph    [context] [--json | --dot] [--target stage]
sitefile why <path> [context] [--json] [--target stage]
```

`build` accepts these image-related options on top of the ones in the README:

| Option | Meaning |
| --- | --- |
| `-t, --tag name[:tag]` | also store the build as an image in the local store (repeatable). **With `-t` and no `-o`, only the store is written and nothing is created in the context; with both, both happen.** |
| `--format oci` / `-o x.oci.tar` | write an OCI image: an OCI archive for a `.tar` destination, an OCI layout directory otherwise (`dist.oci.tar` by default) |
| `--base <image>` | build the oci image on `<image>` (runnable; overrides `EXPORT ... AS oci BASE`); a bare name means Docker Hub. Pin it: `nginx:alpine@sha256:...` |
| `--insecure-registry host[:port]` | allow plain http to that registry (repeatable) |
| `--oci-root /path` | directory of the files inside the image layer (default `/srv/www`; overrides `EXPORT ... AT`) |
| `--compression gzip\|none` | overrides `EXPORT ... COMPRESSION` for the export's format. **oci**: the site layer encoding, `gzip` (default, `tar+gzip`) or `none` (plain `tar`, digest = diff_id); also the layer of `-t` images when the export is oci (or with `-t` and no `-o`). **wbn**: `gzip` wraps the whole bundle in gzip (`gzip(application/webbundle)`, what `@johnhenry/packfile`'s `fromArchive()` and `@johnhenry/fileable`'s `encode="wbn"` use), `none` (default) writes the plain bundle. Any other export format is a usage error. |
| `--oci-compression gzip\|none` | the same as `--compression`, for oci only (kept for existing scripts); giving both is a usage error |
| `-o x.wbn.gz` | selects `wbn` like `-o x.wbn` and gzips it (unless `--compression none`); with `EXPORT ... AS wbn COMPRESSION gzip` and no `-o`, the default destination is `dist.wbn.gz` |
| `--platform os/arch` | image platform metadata (default `linux/amd64`) |
| `--label key=value` | add an OCI image config label (repeatable; overrides defaults such as `org.opencontainers.image.title`) |
| `--git-labels` | add `org.opencontainers.image.source`/`.revision` from the git checkout. Off by default: they change the image ID on every commit, and finding the checkout reads outside the context. Credentials in remote URLs are dropped. |
| `--provenance` | attach a [SLSA provenance](https://slsa.dev/provenance/v1) attestation to the oci image (see [Attestations](#attestations)) |
| `--sbom` | attach an SBOM attestation: a [CycloneDX](https://cyclonedx.org/) 1.6 file list (see [Attestations](#attestations)) |

`build`, `validate` and `inspect` also accept:

| Option | Meaning |
| --- | --- |
| `--allow-exec name[=/abs/path]` | allow a `TOOL name` (an external program) to run; repeatable. A bare name is looked up on `PATH` when the build starts and the resolved real path is printed (`tool ffmpeg -> /opt/homebrew/bin/ffmpeg`). A `TOOL` that is not allowed fails with `E_UNSUPPORTED` naming the flag to add. The program runs **with your privileges**: allow only what you trust. |
| `--allow-plugin name` | allow the JavaScript `PLUGIN name` to run; repeatable. A JS `PLUGIN` that is not allowed fails with `E_UNSUPPORTED` at its line, naming the flag to add, before any import is fetched or any plugin process starts. A name that matches no `PLUGIN` is a warning. WebAssembly plugins need no flag. `validate` never runs plugins; `inspect` builds in memory, so it needs the flag like `build`. |
| `--allow-plugins` | allow every JavaScript `PLUGIN` in the Sitefile. Use it only for a Sitefile you trust as much as its plugin code. |
| `--no-cache` | do not read or write the TRANSFORM result cache for this build |
| `--keep-os-metadata` | keep OS metadata files in archives imported `AS tree`. By default `__MACOSX/` (anywhere in the path), AppleDouble `._*` files, `.DS_Store`, Finder's `Icon\r`, and `Thumbs.db` and `desktop.ini` (any case) are dropped, with one warning per import: `IMPORT s: skipped 6 OS metadata files (...); --keep-os-metadata keeps them`. Build-context directories are not filtered (use `.sitefileignore`). |
| `--strict-pins` | fail with `E_UNPINNED`, before fetching anything, when a remote `IMPORT` has neither `INTEGRITY` nor a `Sitefile.lock` entry, or a registry `FROM image` or the `BASE` image is neither `@sha256:`-pinned nor locked. `validate --strict-pins` and `graph --strict-pins` check the same without building (they read `Sitefile.lock`); with `--provenance` the pins are recorded. Conflicts with `--update-lock`. See [LANGUAGE.md](LANGUAGE.md#remote-sources-and-the-lockfile). |
| `--cache-from <ref>` | `build` only, repeatable: also look up TRANSFORM results in this [registry cache](#registry-cache) (first match wins) |
| `--cache-to <ref>` | `build` only, once: after a successful build, push the TRANSFORM results it used to this [registry cache](#registry-cache) |
| `--plugin-timeout ms` | per-call time limit for TRANSFORM plugins and tools (default 60000) |
| `--plugin-memory MiB` | memory limit for each plugin: a JavaScript plugin's heap (`node --max-old-space-size`), a wasm plugin's linear memory (default 1024). A plugin that passes it fails with `E_PLUGIN` |
| `--node path` | the Node binary JavaScript plugins run on: an absolute path, at least Node 26 (default `$SITEFILE_NODE`, else the Node running the TypeScript CLI or `node` on `PATH` for the Rust binary; [PLUGINS.md](PLUGINS.md#which-node-runs-plugins)) |
| `--lint-disable rule[,rule…]` | `validate` only, repeatable: turn [lint rules](LANGUAGE.md#lint-rules-sitefile-validate) off for this run (`all` turns every rule off). An unknown rule id is a usage error. |
| `--secret id=<id>,env=<VAR>` / `--secret id=<id>,src=<file>` | a build secret for `IMPORT … SECRET <id> AS HEADER <Name>` / `AS BEARER` (repeatable; also on `vendor` and `outdated`). Sent only to that import's origin and redacted (`***`) from every message; see [LANGUAGE.md](LANGUAGE.md#build-secrets---secret-import--secret). |

`validate` also prints **lint warnings** (`warning: Sitefile:4: [render-escape] RENDER writes /index.html without
--escape=html, …`), after the planner's warnings and before the result line; they never change the exit code.
`build`, `inspect`, `graph` and `why` do not lint. See [LANGUAGE.md](LANGUAGE.md#lint-rules-sitefile-validate).

### Building from a Git URL

```
sitefile build https://github.com/x/y.git#ref:dir [options]
sitefile build git+https://host/repo.git#<40-hex commit> [options]
```

A context argument with a URL scheme is a Git repository (like `docker build <git url>`): the tree at
`ref` (default the remote HEAD) and `dir` (default the root) is fetched with the Git client of
`IMPORT … FROM git+https://…` (same network policy, symlink, submodule and name rules; see
[LANGUAGE.md](LANGUAGE.md#git-sources-githttps)), written to a temporary directory, built from there
and removed afterwards. `build`, `validate`, `inspect`, `graph` and `why` accept it; stderr starts with
`context: <url> at commit <hex>`.

- The default `--out` is in the current directory (`./dist`, `./dist.zip`, …), not in the checkout.
- `-f` is a path inside the repository (relative, no `..`). Messages name the Sitefile `Sitefile`
  (or the `-f` path), never the temporary directory.
- The repository's `Sitefile.lock` and `vendor/sitefile` are used as in a local build, but the lockfile
  is **never written back** (there is nothing to write it to): add `--frozen-lockfile` to require every
  remote input to be locked already.
- `--watch` needs a local context (usage error). A ref is resolved on every build; name a commit for a
  reproducible one.

## Network: proxy, host rules and timeouts

Everything sitefile fetches goes through one client per implementation: `IMPORT` over http(s), Git
contexts and Git imports, `FROM image` and `BASE` pulls, `push`/`pull`, `vendor`, `outdated`, the
registry cache and `CHECKLINKS EXTERNAL`. These settings apply to all of it. Every one is opt-in, and
nothing is read from `HTTP_PROXY`, `HTTPS_PROXY` or `NO_PROXY`, so a variable set for other tools never
reroutes a build.

| Option | Environment | Meaning |
| --- | --- | --- |
| `--proxy <url>` | `SITEFILE_PROXY` | send requests through this HTTP proxy, `http://[user:password@]host[:port]` (port 80 by default). https targets go through a `CONNECT` tunnel, so TLS is end to end and the certificate is checked for the target's name. http targets are sent to the proxy in absolute form. Credentials go to the proxy as `Proxy-Authorization: Basic` and never appear in messages. Only `http://` proxy URLs are accepted. |
| `--no-proxy <hosts>` | `SITEFILE_NO_PROXY` | hosts reached directly instead of through the proxy (patterns below). Loopback is **not** bypassed by default; for a local registry, add `localhost,127.0.0.1`. |
| `--allow-hosts <hosts>` | `SITEFILE_ALLOW_HOSTS` | when set, only hosts that match a pattern may be contacted |
| `--deny-hosts <hosts>` | `SITEFILE_DENY_HOSTS` | hosts that are never contacted. **A deny rule beats an allow rule.** |
| `--network <mode>` | `SITEFILE_NETWORK` | `open` (the default) or `strict`: every host must match an `--allow-hosts` pattern, also when none is given (then nothing is contacted), and `IMPORT … SECRET` needs `--allow-secret-host` (see [Strict network mode](#strict-network-mode)). The flag replaces the variable, so `--network open` turns an inherited `SITEFILE_NETWORK=strict` off. |
| `--allow-secret-host <id=host>` | | in strict mode, let `IMPORT … SECRET <id>` send that secret to hosts matching the pattern (repeatable, comma-separated). Ignored in open mode. |
| `--connect-timeout <ms>` | `SITEFILE_FETCH_TIMEOUT` | time limit for opening each TCP connection (to the target, or to the proxy) |
| `--read-timeout <ms>` | `SITEFILE_FETCH_TIMEOUT` | longest wait for data on an open connection (idle time). For registries it replaces the per-request `--timeout`. |

`SITEFILE_FETCH_TIMEOUT` is `<ms>` (sets both timeouts) or `connect=<ms>,read=<ms>` (either part may be left
out). The total per-resource `--timeout` still applies on top of these. Host lists are comma- and/or
space-separated, and the flags repeat. Flags are added to the environment's lists; `--proxy` and the
timeout flags replace the environment's values (`--proxy ''` turns `SITEFILE_PROXY` off). A malformed
value is a usage error (exit 2). These flags work on `build`, `validate`, `inspect`, `graph`, `why`,
`pull`, `push`, `vendor` and `outdated`.

Host patterns use the `NO_PROXY` syntax, matched against the host as written in the URL:

| Pattern | Matches |
| --- | --- |
| `*` | every host |
| `example.com`, `.example.com`, `*.example.com` | `example.com` and every subdomain (`cdn.example.com`), but not `badexample.com` |
| `192.0.2.7`, `::1` | that IP-literal host |
| `10.0.0.0/8`, `fd00::/8` | IP-literal hosts in that network |
| any of these + `:port` | only that port (`[::1]:8080` for IPv6) |

A name pattern never matches an IP literal, and an IP pattern never matches a name, whatever it resolves
to. The private-address rule below covers names that resolve to internal addresses.

The rules are checked on **every hop**: the first URL, each redirect, a registry's token realm and a blob
download redirected to a CDN, the registry's own host included. A refused host fails with
`E_HOST_BLOCKED`, and the message names the rule:

```
error[E_HOST_BLOCKED]: Sitefile:2:1: refusing to connect to cdn.example.com: it matches deny-hosts rule 'example.com' (https://cdn.example.com/a.css)
error[E_HOST_BLOCKED]: Sitefile:2:1: refusing to connect to cdn.example.com: it matches no allow-hosts rule (https://cdn.example.com/a.css); add --allow-hosts cdn.example.com
```

A host that matches no allow rule also carries a `needs` (as a `PLUGIN` or `TOOL` that was not allowed
does): `{ kind: 'host', name: 'cdn.example.com', flag: '--allow-hosts' }` on the TypeScript
`SitefileError`, `SitefileError::needs` (`NeedsKind::Host`) in Rust, and in the `--json` error output
below, so a tool can offer "allow cdn.example.com and rebuild" without parsing the message. A deny-hosts
match has no `needs`: no flag overrides it.

### Strict network mode

`--network strict` (or `SITEFILE_NETWORK=strict`) turns the allow list into the only way to reach a host:

```bash
SITEFILE_NETWORK=strict sitefile build                         # fails at the first host, naming it
sitefile build --network strict --allow-hosts cdn.example.com  # contacts cdn.example.com (and its subdomains) only
```

- An empty allow list allows nothing; the error says `(strict network mode)` and carries the `needs` of
  the host to add. Hosts behind an imported Sitefile (`IMPORT … AS sitefile`) are only known once it has
  been fetched, so they come up one at a time, each with its own `needs`.
- Every hop is checked as in open mode: redirects, a registry's token realm and a blob CDN must each be
  allowed. Git contexts, `vendor`, `outdated`, `pull`/`push`, the registry cache and `CHECKLINKS EXTERNAL`
  follow the same rules (a refused `CHECKLINKS` URL is reported as broken, not fetched).
- **Secrets need their own rule.** An `IMPORT … SECRET <id>` is refused before anything is sent unless an
  `--allow-secret-host <id>=<pattern>` matches the import's host (the only origin its secrets are sent to):

  ```
  error[E_HOST_BLOCKED]: Sitefile:2:1: IMPORT data: refusing to send secret 'tok' to api.example.com (strict network mode); add --allow-secret-host tok=api.example.com
  ```

  with `needs` `{ kind: 'secret-host', name: 'tok=api.example.com', flag: '--allow-secret-host' }`. The
  host itself must be allowed too (that error comes first). There is no environment variable for it.
- **What counts as contact.** Strict mode checks a host only when sitefile is about to connect to it. A
  build whose remote inputs are all read from `sitefile vendor` (`vendor/sitefile` or `--vendor-dir`), and
  whose `FROM image`/`BASE` images are pinned (`@sha256:` or `Sitefile.lock`) and already in the local
  store, connects to nothing and needs no host. An https `IMPORT` that is not vendored is fetched on every
  build, pinned or not (there is no download cache), so it needs its host; so does a Git `IMPORT` that is
  not vendored at its commit. `validate` and `graph` of a local context never connect.

The default stays `open`, so existing scripts and CI are unchanged.

### Which hosts a build needs: `hosts`

```
sitefile hosts [context] [-f Sitefile] [--json] [--build-arg K=V]... [--build-arg-file f]... [--base image]
               [network flags]
```

Lists the hosts a build would contact, read from the plan only: nothing is fetched and no host is
contacted. Each host comes with what needs it and its status under the network flags given (the same
`--allow-hosts`, `--deny-hosts`, `--network` and `--allow-secret-host` as `build`):

```
HOST                  PORT  STATUS   USED BY
cdn.example.com       443   allowed  IMPORT css (Sitefile:2)
github.com            443   blocked  IMPORT theme (Sitefile:3)
registry-1.docker.io  443   blocked  BASE
github.com: needs --allow-hosts github.com
registry-1.docker.io: needs --allow-hosts registry-1.docker.io
IMPORT css (Sitefile:2): secret needs --allow-secret-host tok=cdn.example.com
IMPORT theme (Sitefile:3): an imported Sitefile; the hosts it needs are known once it is fetched
3 hosts (strict network mode): 1 allowed, 2 blocked
```

- Listed: every remote `IMPORT` (https and `git+https`, by the URL's host and port), every `FROM image`
  with a registry, and the BASE image of an oci export (or `--base`), by the registry's API host
  (`registry-1.docker.io` for Docker Hub). `FROM image` from the local store and local imports contact
  nothing and are not listed.
- Status: `allowed`; `blocked` (no allow-hosts rule matches; `needs` names the `--allow-hosts` to add);
  `denied` (a deny-hosts rule matches; no flag overrides it). In strict mode, an `IMPORT … SECRET` whose
  secret has no matching `--allow-secret-host` carries that `needs` on its use.
- Not known before fetching, so not listed: redirects, a registry's token realm and blob CDN, and the
  inputs of an imported Sitefile (`nested`). `CHECKLINKS --external` contacts the hosts of the site's
  external links and is reported on its own line. All of these still come up at build time, one at a
  time, each as an `E_HOST_BLOCKED` with `needs`.
- `--json`: `{"strict", "hosts": [{"host", "port", "status", "needs", "uses": [{"kind": "import" | "image" |
  "base", "name", "source", "span", "secrets", "nested", "needs"}]}], "checklinksExternal": [<line>…]}`.
  `needs` is `null` or `{kind, name, flag}` as in errors. The text and JSON are identical in the
  TypeScript and Rust CLIs.

### `--json` errors

With `--json`, a command that fails also prints the error on stdout (the `error[CODE]: …` line still goes
to stderr, and the exit code is unchanged). This works for `build`, `validate`, `inspect`, `graph`, `why`
and every other command that has `--json` (`outdated`, `images`, …); usage errors (exit 2) stay text only.

```json
{
  "error": {
    "code": "E_HOST_BLOCKED",
    "message": "Sitefile:2:1: refusing to connect to cdn.example.com: it matches no allow-hosts rule (strict network mode) (https://cdn.example.com/a.css); add --allow-hosts cdn.example.com",
    "span": { "file": "Sitefile", "line": 2, "column": 1 },
    "needs": { "kind": "host", "name": "cdn.example.com", "flag": "--allow-hosts" }
  }
}
```

`span` and `needs` are `null` when the error has none. `needs.kind` is `plugin`, `tool`, `host` or
`secret-host`; `[flag, name]` are the arguments to add. In `outdated --json`, an `error` row's object has
a `needs` key too when the error carries one.

Timeouts fail with `E_FETCH_TIMEOUT` and say which limit was hit (`…: connecting timed out after 500 ms
(connect timeout)`, `…: no data for 2000 ms (read timeout)`, or `… timed out after 30000 ms` for the total).
A proxy that refuses a tunnel fails with `E_FETCH` (`proxy http://proxy:3128 refused to connect to
host:443`, or `… requires authentication (HTTP 407)`); for registries, these are `E_REGISTRY`.

**Security note: a proxy never widens the policy.** The host rules are applied to the target, not to the
proxy. The private-address rule is also applied to the target: before a request is handed to the proxy,
sitefile resolves the target's name itself and refuses it (`E_SSRF_BLOCKED`) if any address is non-public,
just as a direct connection does at connect time. So a proxy cannot be used to reach `localhost`, the
cloud metadata address or an internal network unless `--allow-private-network` is given. The cost is
that the target's name must resolve on this machine. If the network can only resolve names through the
proxy, the fetch fails with `E_FETCH` (`cannot resolve … to check it against the private-network rule
before using the proxy`). One gap remains: the proxy resolves the name again on its own, and a name
whose DNS answer changes between the two lookups (DNS rebinding) is resolved by the proxy, not by
sitefile. Use `--allow-hosts` to pin builds to known hosts. The proxy itself is named by you, so it may
be on a private address. Embedders set all of this in code, never through the environment
(`FetchPolicy.network` and `RegistryOptions.network`, `NetworkSettings` in both implementations;
`networkFromEnv()` / `network_from_env()` read the variables when wanted).

## Offline builds: `vendor`

```
sitefile vendor [context] [-f Sitefile] [--vendor-dir dir] [--build-arg K=V]... [--build-arg-file f]...
                [--frozen-lockfile | --update-lock] [--allow-http] [--allow-private-network]
                [--max-download-bytes n] [--timeout ms] [--base image] [--platform os/arch]
                [--insecure-registry host[:port]]...
```

Fetches every remote `IMPORT` and the `BASE` image as a build would (same checks; `Sitefile.lock` is updated
like a build updates it) and replaces `<context>/vendor/sitefile` (or `--vendor-dir`, relative to the context)
with `blobs/sha256/<hex>` and `manifest.json`. It prints
`Vendored 3 resources and 1 image (7 blobs, 1234567 bytes) to vendor/sitefile`, then `Updated Sitefile.lock` when
the lockfile changed. `build` and `inspect` accept `--vendor-dir <dir>` to read from another vendor directory
(`validate` reads no sources and ignores it); without it they use `vendor/sitefile` when its `manifest.json` exists. See
[LANGUAGE.md](LANGUAGE.md#vendoring-sitefile-vendor) for what is stored and how builds verify it.

```bash
sitefile vendor                          # once, online
sitefile build --frozen-lockfile         # later: no network needed, no --allow-* flags

## Checking pins: `outdated`

```
sitefile outdated [context] [-f Sitefile] [--json] [--build-arg K=V]... [--build-arg-file f]... [--no-lockfile]
                  [--allow-http] [--allow-private-network] [--max-download-bytes n] [--timeout ms]
                  [--base image] [--insecure-registry host[:port]]...
```

Reports every remote `IMPORT`, registry `FROM image` and the `BASE` image without changing anything (see
[LANGUAGE.md](LANGUAGE.md#checking-pins-sitefile-outdated) for what each status means and what cannot be known):

```
KIND    NAME  SOURCE                               PINNED        BY         LATEST        STATUS   NEWER TAG
import  data  https://example.com/data.json        ce4883ce4883  integrity  ce4883ce4883  current  -
import  css   https://example.com/style.css        9f2b1c9f2b1c  lockfile   3f876f3f876f  changed  -
base    BASE  docker.io/library/nginx:1.25-alpine  05b54105b541  lockfile   05b54105b541  current  1.27-alpine
3 remote inputs: 1 changed, 2 current
```

Digests are shortened to 12 hex digits (`--json` has them in full); an `error` row is followed by a
`name: error[CODE]: message` line. The exit code is 0 whatever the statuses are.
### Build target: `--target <stage>`

Like `docker build --target`, `--target <stage>` builds up to a **named** stage (`FROM … AS name` or
`STAGE name`) and stops there, for debugging and partial builds. `build`, `validate`, `inspect` and
`serve --watch` accept it (with `build --watch` every rebuild uses it).

- **What runs**: the stages from the first one through the target, in source order. Later stages are not
  run. The Sitefile is still parsed, validated and planned as a whole (an error anywhere in it fails the
  build), every `IMPORT` is resolved and `Sitefile.lock` is used as for a full build, and `TOOL`/JS `PLUGIN`
  permissions (`--allow-exec`, `--allow-plugin`) are checked for the whole Sitefile.
- **What is exported**: the target stage's **whole tree**, from `/`, not the `EXPORT` path (an earlier stage
  rarely has files there). The `EXPORT` instruction's **format and options still apply** (`AS zip`,
  `AS oci` with `BASE`/`AT`/`COMPRESSION`, `wbn` `BASE`), and `--format`, `-o` extensions, `--base-url`,
  `--oci-root`, `-t` and the other export flags work as usual; the default destination is the usual one
  (`<context>/dist`, or `dist.<ext>` for the EXPORT format). The `/_redirects` and `/_headers` files that
  `REDIRECT`/`HEADER` make are **not** written: they belong to the EXPORT path of a full build.
  Naming the last stage still exports its whole tree, without the EXPORT path or site files.
- **validate**: plans up to the target and reports it: `Sitefile: OK (0 imports, 2 operations, 2 stages,
  target assets)` (operations and stages up to the target). `--json` adds `"target": "<name>"`, lists only
  those stages, and gives `"export": "/"`.
- **Errors**: a name that is not a stage is a usage error (exit 2) listing the named stages:
  `error: --target 'nope' does not name a stage (stages: theme, assets)`, or
  `error: --target 'x' does not name a stage (the Sitefile has no named stages; name one with FROM … AS <name>)`.
  An unnamed stage cannot be a target. On `serve`, `--target` needs `--watch`.
- A target stage with no files warns `--target <name>: stage '<name>' contains no files`.
- **Imported stages** (`FROM theme+assets`, `--from=theme+assets`, see
  [LANGUAGE.md](LANGUAGE.md#sitefile-imports-import--as-sitefile)): only those the stages up to the target read are
  built; every `IMPORT … AS sitefile` is still read and its imports resolved. `--target` names a stage of this
  Sitefile, never `theme+assets`.

```bash
sitefile inspect examples/multistage --target theme      # the 'theme' stage's files
sitefile build examples/multistage --target assets -o /tmp/assets
```

## Explaining a build: `graph` and `why`

```
sitefile graph [context] [--json | --dot] [build options]
sitefile why <path> [context] [--json] [build options]
```

Both take the build options (`-f`, `--build-arg`, `--build-arg-file`, `--target`, `--allow-plugin`, ...).
Their text, `--json` and `--dot` outputs are identical in the TypeScript and Rust CLIs (conformance checks them).

### `sitefile graph`

The dependency graph of the plan, like `bazel query`: what each operation reads and where its tree goes. It
plans only (like `validate`: nothing is read or fetched), so it shows what the Sitefile names, not what files a
directory holds or which variables a template uses.

- **Nodes** (`id`, `kind`, `label`, and `line` when there is a Sitefile line):
  `import:<name>` (`IMPORT posts ./posts AS markdown`), `plugin:<name>`, `tool:<name>`, `image:<ref>` (a
  `FROM image` base, labelled `image ghcr.io/me/theme:1`), `local:./<path>` (a context
  path an operation or `FROM` reads), `op:<line>` (an operation, labelled `COPY ./src /out/`,
  `RENDER ./t.html TO /index.html`, `RENDER ./t.html FOR post IN posts` (with ` BY key` and ` PAGE n` when given), `TRANSFORM motion ./title.txt`,
  `SITEMAP /sitemap.xml`, `FEED posts /feed.xml`, `FINGERPRINT /assets/**`, `CHECKLINKS --warn`; FOR destinations are not evaluated), `stage:<name>` or
  `stage:#<n>` for an unnamed stage (the stage's final tree, labelled `FROM ./theme AS theme`),
  `stage:<import>+<stage>` for a stage of a Sitefile imported `AS sitefile` (labelled `stage assets of IMPORT theme`,
  with the IMPORT's line; the imported Sitefile is not read, so its own operations are not shown),
  `site:_redirects`/`site:_headers` (the files EXPORT writes from REDIRECT/HEADER lines) and `export`
  (`EXPORT / AS directory`, with ` (--target <name>)` under `--target`).
- **Edges** point from what is read to what reads it: `base` (a stage's starting tree, a context directory, an
  image or an earlier stage, to its first operation), `next` (an operation's tree to the next operation, the last one to its
  stage), `input` (a source, `COPY --from` stage, plugin, tool or `FOR`/`FEED` list import to the operation,
  an `AS sitefile` import to each of its stages this Sitefile names, and an `IMPORT … SCHEMA` file to its import) and
  `export` (the last stage and the site files to `export`).
- **Order**: imports, plugins, tools, images (first use), imported stages (first use), local paths (first use), then per stage its operations and the stage, then
  the site files and `export`. Edges: the schema files in import order, then in the order the stages are walked, without
  duplicates.
- **Text**: one `id  label` line per node, followed by `  <- from (kind)` for each incoming edge, and
  `N nodes, M edges`.
- `--json`: `{"file", "target"?, "nodes": [{"id", "kind", "label", "line"?}], "edges": [{"from", "to", "kind"}]}`.
- `--dot`: Graphviz (`sitefile graph --dot | dot -Tsvg > graph.svg`): `rankdir=LR`, a shape per kind (cylinder
  imports, tab images, folder paths, component plugins/tools, box operations, box3d stages, note site files, doubleoctagon
  export), dashed `input` edges and bold `base`/`export` edges. `--dot` with any other command, or with `--json`,
  is a usage error.

```
$ sitefile graph examples/multistage
local:./theme  ./theme
local:./pages/index.html  ./pages/index.html
local:./pages/about.html  ./pages/about.html
op:6  COPY ./pages/index.html /vendor/index.html
  <- local:./theme (base)
  <- local:./pages/index.html (input)
stage:theme  FROM ./theme AS theme
  <- op:6 (next)
...
export  EXPORT / AS directory
  <- stage:#3 (export)
11 nodes, 11 edges
```

### `sitefile why <path>`

Which instructions wrote an exported file, from what inputs, and what overwrote it (like `ninja -d explain`).
`why` builds in memory like `inspect` (with a frozen lockfile; nothing is exported) and uses the per-file origin
tracking of the build: it compares each path before and after every operation of every stage.

- `<path>` is a path in the export (`/index.html`; `index.html` means the same). A path that is not in the
  output is `error[E_NOT_FOUND]: '/x' is not in the build output`; a directory is
  `error[E_NOT_FOUND]: '/blog' is a directory in the build output; name a file`; `..` is `E_PATH`. A missing
  `<path>` is a usage error.
- **History**, oldest first: every write that changed the file, in the stage where it happened, and, through
  `COPY --from=<stage>` and `FROM <stage>`, the writes in earlier stages to the file it was copied from. Each
  entry has the instruction's location (`Sitefile:12`), its label (as in `graph`), the stage (`build`, or `#2`
  when unnamed), the path in that stage's tree, the action (`write`, or `overwrite` when it replaced different
  content) and the content digest after it. The last entry wrote the exported file. **Writes of identical content
  are no-ops** (the first writer stays the owner) and are not listed.
- **Inputs** of an entry: `./src/index.html` (a context file, also inside a copied directory or a `FROM ./dir`
  base), `stage build:/out/index.html`, `import theme:/style.css` (or `import data` for a whole import), `<<EOF`
  (an inline body), the template of a RENDER, `plugin <name>`/`tool <name>` and the source of a TRANSFORM,
  `list posts` for `FOR`/`FEED`, `the .html files in the tree` for SITEMAP, `<old path> (renamed)` for a file FINGERPRINT
  renamed (preceded by the history of the old path), `references to renamed files` for a file whose references it
  rewrote and `the files it renamed` for its MANIFEST, and for `/_redirects`/`/_headers`
  every REDIRECT/HEADER line (`Sitefile:19 REDIRECT`). Variables a template reads are not listed.
- **Text**: `<path>  <mode>  <size> bytes  <digest>`, then one numbered line per entry,
  `  1. Sitefile:6 COPY ./src /out/  [stage build]  wrote /out/index.html`, each followed by `       from <input>` lines.
- `--json`: `{"path", "mode", "size", "digest", "origin", "history": [{"location", "instruction", "stage", "path",
  "action", "digest", "inputs": [...]}]}`.
- With `--target`, `<path>` is a path in the target stage's tree.
- Through a stage of a Sitefile imported `AS sitefile` (`FROM theme+assets`, `COPY --from=theme+assets`), the
  history continues into that Sitefile: its entries have its locations (`theme/Sitefile:9`) and the stage as this
  Sitefile names it (`theme+assets`; `theme+inner+x` two imports deep).

```
$ sitefile why /index.html conformance/cases/why-overwrite
/index.html  644  16 bytes  sha256:979a05...
  1. conformance/cases/why-overwrite/Sitefile:6 COPY ./src /out/  [stage build]  wrote /out/index.html
       from ./src/index.html
  2. conformance/cases/why-overwrite/Sitefile:7 COPY ./override/index.html /out/index.html  [stage build]  overwrote /out/index.html
       from ./override/index.html
  3. conformance/cases/why-overwrite/Sitefile:13 COPY --from=more /out /  [stage site]  wrote /index.html
       from stage more:/out/index.html
```

## Variants: `sitefile bake`

```
sitefile bake [targets...] [-f sitefile.bake.yaml] [--print [--json]] [-j n] [--profile dir]
              [--build-arg K=V]... [--build-arg-file f]... [build flags]
```

Like `docker buildx bake`: build several variants of one or more Sitefiles (locales, staging and production) with
one command. The variants live in a **bake file**, not in the Sitefile: `-f <file>`, or else the first of
`sitefile.bake.yaml`, `sitefile.bake.yml`, `sitefile.bake.json` in the current directory. YAML files use the same
safe subset as `IMPORT ... AS yaml` ([LANGUAGE.md](LANGUAGE.md#data-files-yaml-csv-and-web-bundles-see-examplesdata));
another extension is a usage error. See `examples/bake/`.

```yaml
groups:
  default: [site]                     # what `sitefile bake` builds without names
  en: [site-en-staging, site-en-production]
targets:
  common:                             # shared settings (not in the default group, so not built alone)
    context: .
  site:
    inherits: common
    matrix:
      locale: [en, fr]
      env: [staging, production]
    arg-files: ["env/${env}.env"]
    args: {LOCALE: "${locale}"}
    output: dist/${env}/${locale}
```

- **Top level**: `targets` (a non-empty mapping) and optional `groups`. Any other key is `E_DATA`.
- **Target fields** (all optional; every path is **relative to the bake file's directory** unless absolute):

  | Field | Meaning | Default |
  | --- | --- | --- |
  | `context` | build context | the bake file's directory |
  | `file` | Sitefile (`-f`) | `<context>/Sitefile` |
  | `args` | mapping of ARG name to a string, number or boolean (`--build-arg`) | none |
  | `arg-files` | list of `.json`/`.yaml`/`.env` files (`--build-arg-file`) | none |
  | `target` | stop at this stage (`--target`) | the whole build |
  | `output` | export destination (`-o`) | `dist/<target name>`, with the format's extension (`dist/<name>.zip`) when `format` is set |
  | `format` | `--format` (`directory`, `zip`, `tar`, `tgz`/`tar.gz`, `wbn`, `oci`) | from `output`'s extension, else the Sitefile's `EXPORT ... AS` |
  | `tags` | list of `name[:tag]`: also store the build as an image (`-t`; the output is still written) | none |
  | `inherits` | a target name or a list: start from those targets' fields, in order | none |
  | `matrix` | mapping of variable name to a non-empty list of values | none |

  With `format` unset and an `output` without an archive extension, a Sitefile that exports `AS zip` writes a zip
  file at `output`; set `format` (or name `output` `*.zip`) to be explicit.
- **Inheritance**: fields of the `inherits` targets are applied in order, then the target's own; `args` merge key
  by key (later wins), every other field is replaced. `matrix` and `inherits` themselves are not inherited. An
  unknown parent or a cycle (`inherits cycle: a -> b -> a`) is `E_DATA`.
- **Matrix**: a target with a `matrix` becomes one target per combination of values, the first variable varying
  slowest, named `<target>-<value>-<value>...` (characters other than letters, digits, `.`, `_`, `-` become `_`):
  `site-en-staging`, `site-en-production`, `site-fr-staging`, `site-fr-production`. In its string fields (also
  inherited ones and `args` values) `${name}` is replaced by the variable's value; any other `${...}` is `E_DATA`
  (`target 'site-en': output: '${lcoale}' is not a matrix variable (locale)`). Targets without a matrix are not
  interpolated. Two targets with one name are `E_DATA`.
- **Selection**: name targets and groups to build, in order and without duplicates. A matrix target's name selects
  all its variants; a variant's own name (`site-fr-production`) selects just it; a group selects its members (groups
  can list groups). With no names: the `default` group, or every target in file order when there is none. An
  unknown name is a usage error listing the targets and groups; a group naming an unknown target is `E_DATA`.
- **Conflicts**, checked for the selected targets before anything is built (`E_CONFLICT`): two targets writing the
  same output (`bake targets 'a' and 'b' both write 'dist/x'`), one output inside another
  (`... write overlapping outputs 'dist' and 'dist/docs'`), or one tag in two targets (`... both tag 'site:1'`).

**Running.** Each target is a separate `sitefile build` process (`build <context> -f <file> -o <output>
[--target] [--format] [--build-arg-file]... [--build-arg]... [-t]...` plus the flags below), so a crash or a
plugin in one target cannot affect another. Up to `-j, --jobs` (default 4) run at once. Targets whose builds
could write the same `Sitefile.lock` (the file next to their Sitefile) never run at the same time, unless
`--frozen-lockfile` or `--no-lockfile` is given. All builds share `$SITEFILE_HOME`: the TRANSFORM result cache (a
result computed by one target is a hit for the next one that needs it), the image store and the cache size limit,
which bake applies once at the end instead of after every build. Remote imports are fetched by each build (pin them
or `sitefile vendor` once to avoid repeated downloads); in-process sharing of import resolution is not done.

**Output.** Each target's output and errors are printed, each line prefixed with `[<target>] `, in plan order (a
target's lines appear once it and every target before it finished; standard error before standard output):

```
[site-en-staging] Exported 2 files to dist/staging/en (sha256:...)
[site-en-production] Exported 2 files to dist/production/en (sha256:...)
...
Baked 4 targets
```

**Failures.** Every target runs even when one fails. Then nothing else is printed to standard output, the last line
on standard error is `error[E_BAKE]: 1 of 4 targets failed: site-fr-staging` and the exit code is 1 (also when a
target's failure is a usage error in that build). Errors in the bake file itself stop before any build: exit 1
(`E_DATA`, `E_CONFLICT`, `E_NOT_FOUND`) or 2 (usage).

**`--print`** shows the resolved plan without building (`--json` only with `--print`), identical in the TypeScript
and Rust CLIs:

```
site-en-staging
  context   .
  file      Sitefile
  output    dist/staging/en
  arg       LOCALE=en
  arg-file  env/staging.env
  matrix    locale=en env=staging
...
4 targets from sitefile.bake.yaml
```

The lines are `context`, `file`, `target` (if set), `output`, `format` (if set), one `arg` per build arg in
definition order (inherited ones first), one `arg-file` and one `tag` each, and `matrix` for a variant.
`--json`: `{"file", "targets": [{"name", "context", "file", "args": {...}, "argFiles": [...], "target", "output",
"format", "tags": [...], "matrix": {...}}]}`, with `null` for an unset `target`, `format` or `matrix`.

**Flags.**

| Flag | Effect in bake |
| --- | --- |
| `-f, --file`, `--print`, `--json`, `-j, --jobs` | bake's own (see above) |
| `--build-arg K=V`, `--build-arg-file f` | added to every target: `--build-arg` wins over the bake file's `args` and arg files; `--build-arg-file` files (relative to the current directory) are read after the target's own `arg-files` (a target's `args` still win over them, as `--build-arg` always wins over files) |
| `--profile <dir>` | one trace per target: `<dir>/<target>.json` |
| `--allow-exec`, `--allow-plugin`, `--allow-plugins`, `--no-cache`, `--cache-from`, `--strict-pins`, `--frozen-lockfile`, `--update-lock`, `--no-lockfile`, `--vendor-dir`, `--allow-http`, `--allow-private-network`, `--allow-hardlinks`, `--keep-os-metadata`, `--max-download-bytes`, `--timeout`, `--plugin-timeout`, `--plugin-memory`, `--node`, `--insecure-registry`, `--force` | passed unchanged to every build (the same conflicts are usage errors up front) |
| `--cache-to` | refused: each target would replace the results the others pushed. Push from one `sitefile build --cache-to` |
| `-o`, `-t`, `--format`, `--target`, `--watch` and the image flags (`--base`, `--label`, `--provenance`, ...) | not accepted: set them per target in the bake file (`output`, `tags`, `format`, `target`); watch mode and image options are per build |

## Watch mode

```
sitefile build --watch [--poll] [--poll-interval ms] [--no-css-inject] [--console]
                       [--no-overlay] [--navigate-to-changed]
                       [--reload full|morph] [context | ./plain-directory] [build options]
sitefile serve --watch [-p [host:]port] [--spa] [-f Sitefile] [--target stage] [--build-arg K=V]...
                       [--build-arg-file f]... [--no-cache] [--no-lockfile] [--keep-service-workers]
                       [--poll] [--poll-interval ms] [--no-css-inject] [--console]
                       [--no-overlay] [--navigate-to-changed]
                       [--reload full|morph] [context | ./plain-directory]
```

`build --watch` builds once, then rebuilds whenever something it reads changes, until Ctrl-C (exit 0).
`serve --watch` builds the context **in memory** (like `inspect`; no export is written), serves the result like
`serve`, and reloads open pages after each good rebuild.

**A plain directory**: `serve --watch ./dir` on a directory that has no `Sitefile`, given none of `-f`,
`--target`, `--build-arg` and `--build-arg-file`, serves the directory itself, the way `serve ./dir` does (through
the sandboxed local reader: symlinks may not leave it, hidden files other than `.well-known` are not served), and
re-reads it when something in it changes, with the same watcher, ignore rules, debounce and `--poll`. A directory
with a `Sitefile`, or any of those flags, is a build context. The messages are `Read N files (sha256:...)`,
`Change detected, re-reading` and, when the directory cannot be read (a symlink that escapes it, say),
`Read failed; serving the last good copy`. Archive files are not watched (`serve site.zip` reads the file at start;
`server restart` picks up a new copy), and `-d` cannot be combined with `--watch` in either mode.

- **What is watched**: every file under the build context, the Sitefile (also when `-f` points outside the context)
  and each `--build-arg-file` (re-read on every build, so editing one changes the ARGs).
- **What is not**: the export destination and the exporter's temporary siblings (`.<out>.sitefile-tmp-*`,
  `.<out>.sitefile-old-*`), anything under a `.git` directory, `Sitefile.lock` (the build writes it) and the
  store (`$SITEFILE_HOME`). A build's own writes never trigger another build.
- **Debounce**: a rebuild starts once nothing has changed for 100 ms, so saving several files is one rebuild.
  Changes that arrive while a build runs are coalesced into one follow-up build.
- **Polling** (`--poll`, `--poll-interval <ms>`): file system events do not arrive for network shares (SMB, NFS),
  some Docker bind mounts and WSL paths under `/mnt`. `--poll` compares a stat snapshot of the watched paths (type,
  size, mtime, ctime and inode; symlinks not followed; ignored paths not walked) every 1000 ms instead;
  `--poll-interval <ms>` sets the interval (a whole number, 10 or more) and implies `--poll`. Both need `--watch`
  (`error: --poll needs --watch`) and work for `build --watch` and `serve --watch`. A change shows up at the next
  scan, so a rebuild can start up to one interval later. sitefile does not detect network file systems itself; pass
  `--poll` when saves do not trigger rebuilds.
- **Incremental rebuilds**: the watch session keeps the results of the previous builds in memory and re-runs only
  the steps whose inputs changed: an import whose files are unchanged is not parsed again (a Markdown or data
  collection re-parses only the edited documents), an operation that reads nothing that changed reuses its files,
  and `RENDER … FOR` / `TRANSFORM … FOR` (and a directory TRANSFORM, per file) re-run only the elements that changed (unless the template reads the whole
  list). A directory export writes only the files that changed and removes the ones (and the directories) that are
  gone, after checking that the output is still exactly what the previous build wrote; otherwise, and for archive
  formats, it is written in full (see [ARCHITECTURE.md](ARCHITECTURE.md#incremental-export)). Every input is still
  read and hashed on every build, so the output is byte for byte what a clean build writes, with the same warnings
  and errors (see [ARCHITECTURE.md](ARCHITECTURE.md#incremental-rebuilds)). `SITEFILE_NO_INCREMENTAL=1` turns both
  off. With `--profile`, each operation's event says whether it was reused (`memo`: `hit`, `files` or `miss`).
- **Messages**: `Change detected, rebuilding` before each rebuild, then the usual `Exported ...` line
  (`serve --watch`: `Built N files (sha256:...)`). After the first build: `Watching <context> for changes (Ctrl-C to
  stop)`.
- **Failures**: a failed rebuild prints its `error[CODE]: ...` line and `Build failed; keeping the last good
  output`, and the loop keeps watching. A failed build never touches `dist/`. A full export is atomic (written to a
  temporary sibling and swapped in); an incremental one writes the changed files to a temporary sibling first and
  then renames each into place, so every file is always its old or its new version and the output mixes the two
  builds only for the moment those renames take; `serve --watch` keeps serving the last good build. If the *first* build fails, `build --watch` keeps watching and `serve --watch` serves an empty
  site until a build succeeds. A usage error in the first build (for example a malformed `--build-arg`) exits with
  code 2. A context that is not a directory is `E_NOT_FOUND`.
- **Restrictions**: `--watch` is for `build` only among the build commands (`validate`/`inspect --watch` are usage
  errors) and cannot be combined with `-t` (one image per save would fill the store). `serve --watch` runs in the
  foreground: `-d` and `--name` are refused, and `-f`, `--target`, `--build-arg`, `--build-arg-file`, `--no-cache` and
  `--no-lockfile` (and the live-reload flags such as `--keep-service-workers`) need `--watch`. For other build flags, run `build --watch` and serve its output elsewhere.

### Live reload

Only under `serve --watch`:

- `GET /__sitefile/reload` is reserved (it takes precedence over a file at that path) and answers with a
  `text/event-stream` that sends `retry: 1000`, then one event per good rebuild (or re-read) that changed the
  served files: `data: {"type":"css","paths":["/a.css",...]}` when every changed file is a stylesheet
  (`text/css`) that was already there (CSS injection, below), else `data: {"type":"reload"}`. A rebuild whose
  output is identical sends `data: {"type":"ok"}`. Each rebuild (or re-read) starts with `data: {"type":"building"}`,
  and a failed one sends `data: {"type":"error","code":"E_...","message":"...","file":"/ctx/Sitefile","line":2,"column":1}`
  (`file`, `line` and `column` only when the error has a Sitefile location). A stream that opens while the last
  build is broken gets that error event right after `retry: 1000`. The stream is not written to the access log.
- **Go to the changed page** (`--navigate-to-changed`, off by default): when a rebuild added or changed exactly one
  HTML file (other files may change too), its `reload` event names the page that serves it:
  `data: {"type":"reload","path":"/posts/hello/"}` (`/dir/index.html` becomes `/dir/`, other files keep their path;
  segments are percent-encoded). Open pages go there instead of reloading, unless they already show it. Two or more
  changed pages, or none, give a plain reload.
- **Morphing instead of reloading** (`--reload morph`, the default; `--reload full` turns it off): a `reload` event
  where only existing HTML files changed (none added or removed, nothing of another type) carries `"morph":true`.
  The client then fetches its own page again and updates the document in place: text and attributes change,
  elements are matched by position, tag and `id`, the scroll position stays, and the focused `<input>`,
  `<textarea>` or `<select>` (and every `<textarea>`'s text) is left as the person edited it. If the page's scripts
  (each `<script>`'s `src`, `type` and text) differ from the new ones, or anything fails, it reloads instead.
- **Scroll position across reloads**: before a full reload the client saves the scroll position for the path in
  `sessionStorage` and restores it once the reloaded page has loaded (not when it goes to another page).
- **Rebuilding badge and error overlay** (`--no-overlay` turns both off): on `building` the client shows a small
  "Rebuilding..." badge in the corner; on `error` it shows a dismissible overlay with the code, the message and the
  Sitefile location, noting that the page shows the last good build (and when a service worker controls the
  page). Both live in a shadow root, so the site's CSS does not reach them. The next `ok`, `css` or `reload` event
  clears them.
- **CSS injection** (`--no-css-inject` turns it off; then a CSS-only change sends `{"type":"reload"}`): on a
  `css` event the client points each same-origin `<link rel="stylesheet">` whose path is listed at the same URL with
  a fresh `__sitefile=<time>` query parameter (the old sheet stays until the new one has loaded), so the page keeps
  its scroll position and state. When no link matches (a sheet only reached through `@import`), it reloads. Any
  other event, or one it cannot parse (an older client), reloads the page.
- **Through proxies and tunnels**: the stream (and its HEAD) has `cache-control: no-cache, no-transform` and
  `x-accel-buffering: no`, so nginx-style proxies neither buffer nor compress it, and a `: ping` comment every 15 s
  keeps idle connections open. The client opens the stream at `reload` relative to the script's own URL, so a page
  reached through another public origin connects back through it; when an answer is not an event stream (a proxy
  error page), it reconnects after 1 s, doubling the wait up to 30 s.
- `GET /__sitefile/reload.js` is reserved the same way and answers with the client script
  (`text/javascript; charset=utf-8`, `cache-control: no-cache`, not access-logged). It opens the event stream at
  `reload`, relative to its own URL, and reloads the page on each event. The script is the same in both
  implementations and ends with a call carrying the session's options, e.g. `({"resetServiceWorkers":true});`.
- **Service workers**: a service worker that caches the site would answer the reload with the old build, so
  before reloading the client unregisters every service worker of the page's origin and deletes every Cache
  Storage cache (`caches`). The site's own registration code installs its worker again on the reloaded page.
  `--keep-service-workers` turns this off (for working on the service worker's update flow itself). Other storage
  (cookies, `localStorage`, IndexedDB) is never touched, and nothing is sent as `Clear-Site-Data`.
- Every `text/html` file response to a **page navigation** gets one script tag, inserted before the last `</body>`
  (ASCII case-insensitive), or appended when there is none: `<script src="/__sitefile/reload.js"></script>`.
  A navigation is a request with `Sec-Fetch-Mode: navigate`; without that header, one with `Sec-Fetch-Dest`
  `document`, `iframe` or `frame`; without either, one whose `Accept` names `text/html`. HTML fetched by a page
  script (`fetch('/partial.html')`: `Sec-Fetch-Mode: cors`, `Accept: */*`) and requests from tools like curl are
  served unchanged. `Content-Length` matches the injected body, and the ETag gets a `-live` suffix so a cached
  un-injected copy is not reused. Other files are served unchanged.
- The script is external and same-origin, so a Content-Security-Policy with `script-src 'self'` allows it. When
  the `Content-Security-Policy` that `_headers` sets on an injected page still blocks it (its `script-src-elem`,
  `script-src` or `default-src` has none of `'self'`, `*`, `http:`, `https:`, or has `'strict-dynamic'`), or blocks
  the event stream (`connect-src`, else `default-src`, likewise), `serve --watch` prints one warning per policy:
  `warning: live reload: the Content-Security-Policy that _headers sets for /index.html does not allow scripts
  from 'self' (/__sitefile/reload.js), so open pages will not reload: script-src 'nonce-x'`. Host sources are not
  matched (a policy naming only `http://127.0.0.1:8080` is warned about), and a CSP in a `<meta http-equiv>` tag
  is not checked.
- **Browser console** (`--console` adds `log` and `info`): the client forwards uncaught errors (`error` events),
  unhandled promise rejections, `console.error` and `console.warn` with
  `POST /__sitefile/console` (reserved like the other paths; any other method is `405`). The body is JSON
  `{level, message, stack, url, line, col}`, at most 16 KiB (`413`); anything else is `400`. Only a page of this
  server may post: `Origin` must be `http://<Host>` or `https://<Host>` and `Sec-Fetch-Site`, when sent,
  `same-origin` (else `403`; a tunnel that rewrites `Host` therefore drops them). The server answers `204` and
  writes, to stdout and not as access-log lines,
  `browser error: Uncaught TypeError: x is undefined (http://127.0.0.1:8080/app.js:3:5)` (runs of control
  characters become one space; the message is cut at 1000 characters, the URL at 300) followed by up to five stack
  lines indented by four spaces. At most 50 messages are logged per 10 s; the first one over prints
  `browser: more than 50 console messages in 10 s; dropping the rest`. The endpoint exists only under `--watch`,
  and it only ever writes to the log.
- Nothing is injected without `--watch`, and the script is never part of a build's output or an export.

Change detection differs by implementation, the behaviour above does not: the TypeScript CLI uses `fs.watch`
(recursive), the Rust CLI polls the watched paths every 100 ms (see [RUST.md](RUST.md#known-differences)); with
`--poll` both poll.

## Profiling (`--profile`)

```
sitefile build|inspect|validate|graph [context] --profile trace.json [options]
sitefile why <path> [context] --profile trace.json [options]
```

`--profile <file>` writes a timing trace of the command in the [Chrome trace event
format](https://docs.google.com/document/d/1CvAClvFfyA5R-PhYUmn5OOQtYMH4h6I0nSsKchNAySU) (JSON object form): open it
in `ui.perfetto.dev`, `chrome://tracing` or speedscope to see which steps are slow. The file is written when the
command ends, also when the build fails (so you can see how far it got); under `--watch` it is rewritten after every
build. If it cannot be written, a successful build fails with `error[E_EXPORT]: cannot write the --profile file
'<file>'` (exit 1); a failed build keeps its own error. A trace file inside the build context is never read as an
input (like the export destination), and `--watch` ignores its changes.

Every event is a complete event (`"ph": "X"`, `ts` and `dur` in microseconds from the start of the command) on
`pid` 1. The build runs on `tid` 1; import *N* (0-based, in Sitefile order) runs on `tid` *N*+2, because imports
resolve concurrently; `thread_name` metadata events name each thread. Events are listed in the order they start in
the Sitefile (imports in declaration order), so two traces of the same build differ only in `ts` and `dur`; the
TypeScript and Rust CLIs write the same events, names, categories and args (conformance checks it).

| `cat` | `name` | `args` |
| --- | --- | --- |
| `command` | `build`, `inspect`, `validate`, `graph` or `why`: the whole command | |
| `build` | `parse`: read, parse, validate and plan the Sitefile | `imports`, `stages` (with `--target`, the stages built), `target` (only with `--target`) |
| `import` | `import <name>` | `uri`, `as` (the `AS` kind, default `file`), `kind` (`file`, `tree` or `value`) |
| `plugin` | `PLUGIN <name>`: check and register a plugin | `path` |
| `stage` | `stage <name>` (`stage #N` for an unnamed stage) | `from` (`scratch`, a stage name or `./dir`), `files` (tree size at the end) |
| `operation` | `<Sitefile>:<line> <INSTRUCTION>`, e.g. `Sitefile:5 RENDER` | `line`, `files` (tree size after it); under `--watch` also `memo`: `hit` (result reused), `files` (its files reused, added to a changed tree) or `miss` (it ran) |
| `transform` | `TRANSFORM <plugin or tool> <dest>`: one plugin call or tool run (one per `FOR` element or per file of a directory source, in path order; none for what a `--watch` rebuild reuses) | `plugin`, `tool` (true for a `TOOL`), `dest`, `input` (directory sources: the file, e.g. `./photos/a.jpg`), `cache`: `hit`, `miss` or `off` (`--no-cache`, or not cacheable). Calls of a directory source may overlap in time (up to 4 at once) |
| `import` | `BASE <image>`: resolve or pull the `oci` BASE | |
| `export` | `export`: encode and write the output (`build` only) | `format` |

An event the build failed inside has `"incomplete": true` and ends when the trace is written. `<Sitefile>` is the
Sitefile's path relative to the working directory, as in error messages.

## Transform cache

TRANSFORM results (plugins and `TOOL`s) are cached in `$SITEFILE_HOME/cache/transform` (default `~/.sitefile/cache/transform`),
beside the image store but separate from it (`image prune` does not touch it). See [PLUGINS.md](PLUGINS.md#caching).

```
sitefile cache info [--json]              location, entries, blobs, size
sitefile cache prune [--older-than days]  delete every cached result, or only entries unused for N days
sitefile cache prune --plugin <name>      only the results of that PLUGIN or TOOL (with --older-than: unused for N days)
sitefile cache prune --max-size <size>    apply the size limit below now, with that size
sitefile cache du [--json]                size of every cache and the store, and what may be evicted (alias: cache ls)
```

`--plugin` keeps every other plugin's entries, entries that do not name a plugin (written before entries recorded
it), and every blob another entry still uses; it conflicts with `--max-size`.

### What lives under `$SITEFILE_HOME`

| Path | What | Safe to delete? |
| --- | --- | --- |
| `cache/transform/entries/<key>.json`, `cache/transform/blobs/<hex>` | TRANSFORM results (both implementations; each entry names its PLUGIN or TOOL) | yes: a pure cache, rebuilt on the next miss. **Evicted automatically.** |
| `cache/wasm/<key>.cwasm` | compiled wasm plugins (Rust build only; [RUST.md](RUST.md#wasm-module-cache)) | yes: a pure cache, recompiled on the next miss. **Evicted automatically.** |
| `index.json`, `oci-layout`, `blobs/sha256/<hex>` of **tagged** images | the image store: your images | **no**. Never evicted automatically; `rmi` removes tags. |
| untagged images (`<none>`: a re-tagged build's old image, a BASE image pulled by digest) and blobs no image uses | the image store's garbage and pulled BASE images | yes: `image prune`, or **evicted automatically** (a BASE image is pulled again when needed). |
| `blobs/sha256/.tmp-*`, `*.tmp` | writes in progress (temp file + rename) | only when old (1 hour); a running pull or build may own them. |
| `servers/<id>.json`, `servers/<id>.log` | site server state and logs | stopped servers: `server rm`. Never evicted. |
| `lock` | held while the store is modified (stale locks are broken) | never by hand while sitefile runs. |

Remote `IMPORT`s (https URLs) are not cached on disk: they are fetched on every build and pinned by `Sitefile.lock`
(integrity), so there is no remote resource cache to clean. BASE images are pulled into the image store (above).
For offline builds, `sitefile vendor` keeps copies **in the context** (`vendor/sitefile`), not under `$SITEFILE_HOME`.

### Cache size limit

At the end of every `build` and `inspect`, sitefile keeps the **evictable** data (TRANSFORM results, compiled wasm
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

### Registry cache

```
sitefile build --cache-to   ghcr.io/team/site-cache:main --allow-plugins     # CI on main: share results
sitefile build --cache-from ghcr.io/team/site-cache:main --allow-plugins     # a teammate or a PR build: reuse them
sitefile build --cache-from reg.example.com/c:pr-12 --cache-from reg.example.com/c:main --cache-to reg.example.com/c:pr-12
```

`--cache-from <ref>` (repeatable) and `--cache-to <ref>` (once) share the [TRANSFORM cache](#transform-cache) through an
OCI registry, like buildx's registry cache. Keys are the local cache's keys (content addressed, see
[PLUGINS.md](PLUGINS.md#caching)), so a shared result is reused exactly when a local one would be.

- **Format**: one OCI artifact (image-spec v1.1), an `application/vnd.oci.image.manifest.v1+json` manifest with
  `artifactType: application/vnd.sitefile.cache.v1+json`, the empty config (`application/vnd.oci.empty.v1+json`, `{}`),
  and as layers first the **index** (`application/vnd.sitefile.cache.index.v1+json`), then every result blob
  (`application/vnd.sitefile.cache.blob.v1`, sorted by digest). The index is canonical JSON (sorted keys, no
  whitespace) mapping each cache key to its entry: `{"entries":{"<key>":{"files":[["<path>","<sha256 hex>",<size>]],
  "kind":"file"|"files"}},"version":1}` (`path` is `''` for a one-file result, else relative to the TRANSFORM's
  destination directory). The same entries always give the same bytes, so both implementations push identical
  manifests.
- **`--cache-from`**, before the build: fetches each manifest and its index (at most 64 MiB) and prints
  `Cache from <ref>: N entries`. A result's blobs are downloaded **only on a hit**: when the local cache misses and the
  key is in an index (the first `--cache-from` that has it). Every blob is checked against its digest and size while it
  streams to a temp file, then lands in the local cache (`cache/transform`), so the next build is local. An index entry
  must name blobs listed in the manifest with the same size and safe relative paths, or the whole index is refused.
- **`--cache-to`**, after a **successful** build (a failed build pushes nothing): pushes the entries this build used
  (local hits, registry hits and new results) as a new artifact at that tag, replacing what was there (entries the
  build did not use are not carried over; list the previous cache in `--cache-from` to reuse it). Blobs the registry
  repository already has are not uploaded again (`HEAD` first), blobs held by a `--cache-from` repository on the same
  registry are mounted, then the manifest is `PUT`. One line goes to stderr:
  `Cache to <ref>: 3 entries, 2 blobs uploaded, 0 mounted, 3 already present (sha256:...)`. A tag is required (not a
  digest); the default tag is `latest`.
- **Never fails a build**: an unreachable registry, a missing or rejected credential, a missing tag (a first CI run),
  something that is not a sitefile cache, a corrupt blob or a bad index is a warning (`warning: --cache-from <ref> not
  used: ...`, `warning: --cache-from <ref>: ...; not using it for the rest of this build`, `warning: --cache-to <ref>
  not pushed: ...`) and the TRANSFORM runs as usual. Only usage errors exit 2: a reference without a registry host,
  `--cache-to` twice or with a digest, `--cache-from`/`--cache-to` with `validate`, `inspect`, `--watch` or
  `--no-cache`.
- **Size limit**: a build downloads at most [`SITEFILE_CACHE_MAX`](#cache-size-limit) bytes of results (then
  `warning: --cache-from: not fetching more cached results: they would exceed SITEFILE_CACHE_MAX=...`), and what it
  downloads is evicted like any other TRANSFORM result.
- **Registry access** is the one `push` and `pull` use (see [Registries](#registries-push-and-pull)): https, plain http
  only for loopback and `--insecure-registry` hosts; redirects and token realms re-validated and refused on private
  addresses unless `--allow-private-network`; credentials from `$SITEFILE_REGISTRY_AUTH` or the Docker config
  (`--username`/`--password-stdin` are not build options), only when the registry asks, never printed.
- **Nothing local or secret is pushed**: keys are SHA-256 hashes, entries hold output-relative paths, digests and sizes,
  and blobs are the TRANSFORM results themselves (what the build exports anyway). No hostnames, home or context paths,
  timestamps or credentials. Results of plugins exporting `cache = false` are never cached, so never pushed.
- **Trust**: a result from `--cache-from` is used as if the plugin or tool had produced it. Pull only from a repository
  whose writers you trust (CI pushes, pull requests read).

## File manifests and `sitefile verify`

An image ID covers the whole tree. To check single files of a deployed or extracted copy, write a
manifest with the export and verify the copy against it later:

```sh
sitefile build . -o site.zip --manifest site.manifest.json
unzip site.zip -d /srv/www
sitefile verify /srv/www --manifest site.manifest.json
sitefile verify site.zip --manifest site.manifest.json --json
sitefile save mysite:1 -o out --manifest out.manifest.json
```

`--manifest <file>` (build and save) writes, next to any export format, every exported file with its
size and SHA-256, and a Merkle root over them. It is written with the export's destination rules (an
existing file outside the context needs `--force`), and it may not be inside an exported directory. It
does not depend on the format: a directory, zip or OCI export of the same build gives the same bytes.

```json
{
  "mediaType": "application/vnd.sitefile.manifest.v1+json",
  "root": "sha256:…",
  "files": [
    { "path": "/index.html", "size": 207, "digest": "sha256:…" }
  ]
}
```

`files` is in path order (UTF-16 code units, as JavaScript sorts strings). `root` is the
[RFC 6962](https://www.rfc-editor.org/rfc/rfc6962#section-2.1) Merkle Tree Hash of the leaves
`<path> 0x00 <digest>` (UTF-8, the digest written `sha256:<hex>`): a leaf hashes as
SHA-256(0x00 ‖ leaf), an inner node as SHA-256(0x01 ‖ left ‖ right), split at the largest power of two
below the count; no files hash as SHA-256 of nothing. One file can be proven against the root with the
log2(n) sibling hashes on its path, as in a Certificate Transparency inclusion proof.

`sitefile verify <dir|archive|ref|id> [--manifest <file>] [--json]` loads the target and compares it with
the manifest: a listed file that is absent is `missing`, one with another size or digest is `changed`, a
file the manifest does not list is `extra`. It prints one line per difference (`OK: N files match <root>`
when there is none) and fails with `E_INTEGRITY`, exit 1, when there is any. A directory is read like a
build context: hidden files count, `.git` is skipped, symlinks must stay inside it, and a manifest file
inside the directory is not counted as extra. An archive (zip, tar, tar.gz, wbn, OCI archive) is read from
its bytes with the default archive limits. An image needs no manifest: without one, verify checks every
stored blob against its digest. The manifest itself is checked first: its root must match its files.

`--json` prints `{ ok, root, files, missing: [path], changed: [{ path, expected: { size, digest },
actual: { size, digest } }], extra: [path] }`.

## Comparing builds: `sitefile diff`

`sitefile diff <a> <b> [--json] [--exit-code]` lists what changed going from `<a>` to `<b>`. Each side is
a directory, an archive file (zip, tar, tar.gz, wbn, OCI archive) or an image reference or ID, read the
way `verify` reads it (hidden files count, `.git` is skipped). Files are compared by digest and mode, never
by timestamp:

```
C  /index.html  (207 B -> 210 B)
A  /new.txt  (1 B)
C  /run.sh  (10 B -> 10 B, mode 644 -> 755)
-  /_redirects: /old /new 301
+  /_redirects: /older /new 301
+  /_headers: /assets/* Cache-Control: immutable
1 added, 0 removed, 3 changed
```

`A` is added, `D` removed, `C` changed, in path order. When `/_redirects` or `/_headers` differ, the rules
added (`+`) and removed (`-`) follow: a `_redirects` rule is its line with fields joined by one space; a
`_headers` rule is `<path> <Name>: <value>`, one per header line. Blank lines and `#` comments are not
rules. The exit status is 0 whether or not the two differ; `--exit-code` makes it 1 when they do.

`--json` prints `{ added: [file], removed: [file], changed: [{ path, before: file, after: file }], rules:
{ "/_redirects": { added: [rule], removed: [rule] }, "/_headers": … } }`, where a file is
`{ path, size, digest, mode }` (`mode` is `644` or `755`) and `rules` has a key only for a file whose rules
changed. (`sitefile image diff` is the older, images-only form with bare `A|D|C <path>` lines.)

Archive exports give every file one fixed mtime (`SOURCE_DATE_EPOCH`, else 0), so after extracting a new
version `rsync` without `-c` sees unchanged sizes and times and can skip changed files, and nginx sends the
same `Last-Modified` for every version. Compare with `diff` (or `verify` against a manifest) instead, and
use `rsync -c` (checksums) to upload.

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
- **push** then pushes the image's [attestations](#attestations), if it has any, as OCI 1.1 *referrers*: each
  artifact manifest (with its blobs) by digest. A registry that implements the referrers API says so with an
  `OCI-Subject` header; for any other registry (Docker's `registry:2`, for one) the *referrers tag schema* is used:
  the image index at tag `sha256-<image hex>` lists them, merged with whatever it already lists. The image's own tag
  and digest are untouched.
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

### Attestations

```
sitefile build -o site.oci.tar --provenance --sbom          # in the OCI archive / layout
sitefile build -t localhost:5000/blog:1 --provenance --sbom  # kept with the stored image ...
sitefile push localhost:5000/blog:1                          # ... and pushed with it
```

`--provenance` and `--sbom` (both opt-in, like `docker buildx build --provenance/--sbom`) attach
[in-toto](https://github.com/in-toto/attestation) statements to an oci image: an `EXPORT ... AS oci` /
`--format oci` / `-o *.oci.tar` export, or an image stored with `-t` (any export format then); otherwise they are a
usage error. They are command-line flags, not Sitefile syntax: an attestation describes one build of the file, it is not
part of the site.

- **Attachment**: each statement is the single layer (`application/vnd.in-toto+json`, annotated
  `in-toto.io/predicate-type`) of an OCI 1.1 artifact manifest (`artifactType: application/vnd.in-toto+json`, the empty
  config `application/vnd.oci.empty.v1+json`) whose **`subject` is the image manifest**. The image itself is never
  changed, so **its ID is the same with or without attestations** (and the same as before this feature). An OCI
  archive or layout lists the attestation manifests in `index.json` after the image (without a name, with their
  `artifactType` and predicate annotation) and holds their blobs; `-t` keeps them in the store
  (`$SITEFILE_HOME/referrers/sha256/<image hex>`, an image index of them), `save --format oci` writes them back out,
  `push` sends them as referrers (above). A later build of the same image with `--provenance` or `--sbom` replaces
  the stored attestation of that kind; `rmi`/`image prune` remove them with the image. `pull` does not fetch them.
  This is the OCI 1.1 referrers model rather than BuildKit's attestation manifests inside an image index
  (`vnd.docker.reference.type: attestation-manifest`): wrapping the image in an index would change what the tag and
  the image ID point at, and the local store, `save`, `serve` and `pull` work on image manifests.
- **Provenance** (`predicateType: https://slsa.dev/provenance/v1`): `buildDefinition.buildType` is
  `https://github.com/johnhenry/sitefile/blob/main/docs/CLI.md#attestations`; `externalParameters` holds the
  Sitefile path (relative to the context), the `--build-arg`/`--build-arg-file` values (`args`) and the platform;
  `resolvedDependencies` lists the Sitefile with its sha256, every `IMPORT` (`import:<name>`, its URI, the digest the
  build used: for remote ones the one pinned in `Sitefile.lock`, with the final URL after redirects as
  `downloadLocation`; for local directories the tree digest `inspect` prints) and the `BASE` image with its manifest
  digest; `runDetails.builder` is `{"id": "https://github.com/johnhenry/sitefile", "version": {"sitefile": "0.1.0"}}`
  (the same for both implementations, whose output is byte-identical).
  **Build args whose name has a `_`-separated word like `key`, `token`, `password`, `passwd`, `pword`, `secret`,
  `auth`, `apikey` or `credential(s)` (`API_TOKEN`, `DB_PASSWORD`) are never recorded**: only their names, in
  `externalParameters.redactedArgs`. Other build-arg values are recorded: do not pass secrets in build args.
- **SBOM** (`predicateType: https://cyclonedx.org/bom`): a CycloneDX 1.6 JSON document. `metadata.component` is the
  image (`version` = its digest), `components` the `BASE` image (if any) and every file of the site layer: its path in
  the image (`/srv/www/index.html`), SHA-256, and `sitefile:mode` / `sitefile:origin` properties (the instruction that
  wrote it, e.g. `Sitefile:5 RENDER`). CycloneDX rather than SPDX 2.3 because SPDX requires a creation timestamp, a
  unique document namespace and a SHA-1 for every file; CycloneDX needs none of them, so the document is a pure function
  of the build.
- **Deterministic**: canonical JSON, the subject is named `_` (the digest is what matters; retagging changes nothing),
  and there are **no timestamps unless `SOURCE_DATE_EPOCH`** is set (then `runDetails.metadata.startedOn`/`finishedOn`
  and the SBOM's `metadata.timestamp`). The same build gives byte-identical attestations.
- Attestations are not signed; sign them with your registry tooling (cosign, notation) if you need that.

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
| `save <ref\|id> -o out [--format fmt] [--base-url url] [--force]` | export an image. Formats as in `build -o`: a directory, `.zip`, `.tar`, `.tar.gz`/`.tgz`, `.wbn`, `.wbn.gz` (gzipped); **`.oci.tar` (or `--format oci` with a `.tar` destination) writes the stored image itself as an OCI archive, and `--format oci` with any other destination as an OCI layout directory** (same digests as the store, attestations included). A non-empty destination needs `--force`. |
| `image diff <a> <b>` | `A`/`D`/`C` lines for added, removed and changed paths. |

## Language server

```
sitefile lsp [--stdio]
```

Runs the Sitefile language server (LSP 3.17, JSON-RPC over stdin/stdout) for editors: diagnostics from the real
parse/validate/plan pipeline and lint rules, quick fixes (code actions), completion, hover, go to definition, references, rename and document symbols. `--stdio`
is accepted (clients pass it) and is the only transport. It exits 0 after `shutdown` + `exit`. See [LSP.md](LSP.md).

**Rust binary only** (a tooling exception, see [RUST.md](RUST.md#tooling-exception-the-language-server)): the
TypeScript CLI prints a usage error pointing to `rust/target/release/sitefile lsp` and exits 2.

## Site servers

```
sitefile serve [--name n] [-p [host:]port] [-d] [--spa] <ref|id|./dir|file>
sitefile serve --watch [-p [host:]port] [--spa] [build options] [context]   # see Watch mode
sitefile servers [-a] [--json]                   # also: sitefile server ls
sitefile server stop [--time secs] <name|id>...
sitefile server rm [-f] <name|id>...
sitefile server logs [-f] [-n lines] <name|id>
sitefile server restart <name|id>
```

`servers` lists `NAME SITE URL STATUS ID`, where SITE is the image reference, directory or archive file being
served (`--json` calls it `site`).

`serve` serves an image from the store (the files under its root), or a directory (an argument starting with `./`,
`../` or `/`, or ending in `/`) read once at start through the sandboxed local reader: a symlink leaving the
directory is an error, and hidden files (`.git`, `.env`, ... except `.well-known`) are not served.

`serve` also serves an **archive file**: `sitefile serve site.zip`, `./site.tar`, `site.tgz` (`.tar.gz`),
`site.wbn`, or an OCI image archive (`site.oci.tar`, as `build -o x.oci.tar` and `save` write it). A path-like
argument (as above) that is a regular file is an archive; a bare name is an image reference first, and an archive
file only when no image has that name. The format is read from the bytes, never the extension, and the file is
read at start, so `server restart` picks up a new copy. A zip, tar or Web Bundle is extracted with the default
archive limits (10 000 entries, 500 MiB, a 100:1 compression ratio; `E_ARCHIVE_LIMIT`), refuses unsafe entries
(absolute or `..` paths, links, devices; `E_ARCHIVE_UNSAFE`), and is served like a directory: its paths as they
are in the archive (a single top-level folder is kept) and without hidden files except `.well-known`. A
directory or a zip, tar or Web Bundle is also served without OS metadata files (`__MACOSX/`, `._*`, `Thumbs.db`,
`desktop.ini`, `Icon\r`; `.DS_Store` is hidden anyway), with a warning saying how many were skipped;
`--keep-os-metadata` serves them (it is kept in the server record as `"keepOsMetadata": true` and applies on
`server restart`). So a Finder-made zip (`site/` plus `__MACOSX/`) serves as the single folder `site/`. An OCI
archive serves the first image in its `index.json` exactly as that image would be served from the store, after
every blob it reads is checked against its digest and the site layer against the same limits. A file that is
none of these is `error[E_TYPE]: <file>: not a recognized archive (expected tar, tar.gz, zip or web bundle)`.
In a server record (`$SITEFILE_HOME/servers/<id>.json`) the source is `{"kind": "archive", "path": ...}`, next
to `image` (`imageId`) and `dir` (`path`).

- Binds `127.0.0.1` and a free port unless `-p` is given. `-p 8080`, `-p 127.0.0.1:8080`, `-p [::1]:8080`.
  **`0.0.0.0` (or `::`) is bound only when you write it in `-p`** (a warning is printed). A port in use is
  `error[E_SERVER]`.
- **`--cors`** adds `Access-Control-Allow-Origin: *` to every response (files, `304`, `404`, redirects, errors;
  a `_headers` value for the path wins) and answers `OPTIONS` on any path with `204`,
  `Access-Control-Allow-Methods: GET, HEAD`, the request's `Access-Control-Request-Headers` echoed as
  `Access-Control-Allow-Headers` (when it is a plain list of header names) and `Access-Control-Max-Age: 600`.
  Without it `OPTIONS` is `405` as before. Stored in the server record (`cors`).
- **Host allow-list (DNS rebinding).** A request whose `Host` names something other than `localhost`, a
  `*.localhost` name or an IP address (any IPv4 or bracketed IPv6 literal, so the bind address and, on
  `0.0.0.0`, every interface address work) gets `403` with a page naming `--allowed-host`, before anything
  else is looked at (the live-reload endpoints too). A malicious page that rebinds its own name to
  `127.0.0.1` therefore cannot read the server. `--allowed-host <name>` (repeatable, ASCII
  case-insensitive) adds a name: `example.test`, or `.example.test` for it and every subdomain (tunnels,
  `<name>.local`); `--allowed-host '*'` turns the check off. A request without `Host` (HTTP/1.0) is answered.
  The names are stored in the server record (`allowedHosts`), so `-d` and `server restart` keep them; the
  liveness probe of `servers` uses the IP address and is never refused. The library's `createSiteServer`
  checks only when `allowedHosts` is given.
- Foreground by default: prints the URL and one access-log line per request (`GET /path 200 1234 1.2ms`); Ctrl-C
  stops it. Foreground servers are not listed by `servers`; `--name` needs `-d`.
- Behaviour: `GET` and `HEAD` only (`405` with `Allow` otherwise); `/` and `/dir/` serve `index.html`; clean
  URLs: `/about` serves `/about.html` when there is no file `/about`, and `/about/` redirects (`301`, query kept) to
  `/about` when only `/about.html` answers it (after any `_redirects` rule for the path); `/about.html` and
  `/about/index.html` still answer as themselves; otherwise a directory `/dir` redirects (`301`) to `/dir/`; no directory listings; a missing path is `404` with the site's `/404.html` if it has one (else a small
  page); `content-type` from the extension (case-insensitive) through one table shared by both implementations
  and by Web Bundle export: `tools/gen-mime.mjs` writes `src/mime-data.ts` and `rust/src/mime_data.rs` (types
  from nginx's `mime.types`, else the IANA registrations: HTML, CSS, JavaScript including `.cjs`, JSON and
  source maps, XML/XSLT, feeds, WebVTT, CSV, Markdown, YAML, TOML, fonts, images, audio, video, HLS/DASH,
  glTF, Web Bundles and common archives); text types get `; charset=utf-8`; any other extension is
  `application/octet-stream`. A few well-known files without an extension are typed by their exact path first:
  `/.well-known/apple-app-site-association` and `/apple-app-site-association`, `openid-configuration`,
  `oauth-authorization-server` and `nodeinfo` under `/.well-known/` are `application/json`, `/.well-known/webfinger`
  is `application/jrd+json`, `/.well-known/host-meta` is `application/xrd+xml` (`_headers` can still override);
  `ETag` is the file digest and `If-None-Match` gives `304`; `cache-control: no-cache`;
  **byte ranges**: file responses send `Accept-Ranges: bytes`, and a `GET` with one range (`Range: bytes=a-b`,
  `bytes=a-` or `bytes=-n`) gets `206 Partial Content` with `Content-Range: bytes a-b/size` and those bytes (an
  end past the file is clamped); a range starting at or past the end (or `bytes=-0`, or any range of an empty
  file) is `416` with `Content-Range: bytes */size`. `If-Range` must equal the ETag exactly (strong comparison;
  a date or other value gets the whole file with `200`), and `If-None-Match` is checked first. Several ranges
  (`bytes=0-1,5-6`), other units and malformed values are ignored (the whole file, `200`), as RFC 9110 allows;
  `HEAD` ignores `Range` and reports the full length. Under `serve --watch` a range applies to the page as
  sent (with the reload script). `404` pages are never ranged;
  **`Last-Modified`** on file responses (`200`, `206`, `304`): for an image, when it was first stored (as
  `images` shows it; the config's `created` is the Unix epoch unless `SOURCE_DATE_EPOCH` is set, so it is not
  used); for a directory, the newest modification time of the files under it (hidden entries but `.well-known`
  skipped); for an archive, the file's modification time; under `--watch`, the time of the build being served.
  `If-Modified-Since` (an IMF-fixdate; other date formats are ignored) gives `304` when the files are not newer,
  but only when the request has no `If-None-Match`, which wins;
  **`103 Early Hints`**: before a `200` HTML page answering a `GET` over HTTP/1.1, the link-values of its `Link`
  header (from `_headers`) with `rel=preload` or `rel=preconnect` are sent in a `103` response, as Cloudflare
  does (`Link: </app.css>; rel=preload; as=style`); the page itself keeps the whole `Link` header;
  `x-content-type-options: nosniff`. An absolute-form request target (`GET http://host/path?q`, as proxies send
  it; RFC 9112 section 3.2.2) is answered by its path and query; the authority is ignored. Paths are
  percent-decoded per segment; `..`, an encoded `/`, NUL, control
  characters and backslashes are `400`. `--spa` serves `/index.html` for unknown paths whose last segment has no
  file extension (a missing `/app.js` stays `404`).
- **`_redirects` and `_headers`** at the site root (the Netlify / Cloudflare Pages files, which `REDIRECT` and
  `HEADER` write; see LANGUAGE.md "Site files") are read once at start and never served themselves.
  - `_redirects`: one `from to [status]` per line (`#` comments). Supported: exact paths (a trailing slash on
    the request is ignored, so `/old/` matches `/old`), and a trailing `/*` whose remainder replaces `:splat` in
    the target (`/blog/* https://blog.example/:splat`). Statuses `301` (the default), `302`, `303`, `307`, `308`
    redirect with `Location` (the request's query string is kept unless the target has one); `200` serves the
    target path's file instead (a rewrite, e.g. `/app/* /app/index.html 200`); `404` serves it with status
    404. Like Netlify, a rule applies only when no file answers the path, unless its status ends in `!`
    (`301!`), which is checked first. The first matching rule wins. Not supported (the line is ignored):
    `:placeholders`, `*` anywhere but the end, query or country/language conditions, extra fields, and targets
    with characters outside printable ASCII.
  - `_headers`: a path pattern line (`*` matches any characters, `/` included), then indented `Name: value`
    lines. Every matching block applies, in file order; a header set by several blocks gets its values joined
    with `, `. They are added to file, `206`, `304`, `404` and `416` responses (not to redirects) and can replace
    `cache-control` and the other defaults; `content-length`, `etag`, `location`, `connection`,
    `keep-alive`, `transfer-encoding`, `accept-ranges`, `content-range` and `last-modified` cannot be set. Lines with an invalid name or a control character in
    the value are ignored, as are `! Name` detach lines and patterns that are not paths.
- `-d` detaches. A server is tracked by `$SITEFILE_HOME/servers/<id>.json` (`id`, `name`, `image`, `source`, `spa`,
  requested `bind`, `host`, `port`, `pid`, `started`, `logFile`, `ready`, and `project` for servers `up` started) and `<id>.log`. `serve -d` writes the
  state, spawns a detached `node serve-worker.js <id>` with stdout/stderr on the log, and waits until the worker
  has recorded its pid and port (a failure to start is reported with the worker's error and leaves no entry).
  `servers` never trusts the file alone: **running** means the pid is alive *and* the port answers with this server's
  `x-sitefile-server: <id>` header; anything else shows as **exited**. IDs are 12 hex digits, names are unique
  (auto names look like `site-1a2b3c`).
- `stop` sends `SIGTERM`, waits `--time` seconds (default 5), then `SIGKILL`; stopping an exited server is a no-op.
  `rm` refuses a running server unless `-f`. `restart` stops and starts it again with the same name and bind
  address, serving the same image ID (a directory or archive file is re-read). `logs -f` follows the log until Ctrl-C.

### Several servers: `up`, `down`, `ps`

```
sitefile up   [-f sitefile.servers.yaml] [--time secs]
sitefile down [-f sitefile.servers.yaml] [--time secs]
sitefile ps   [-f sitefile.servers.yaml] [--json]
```

A **servers file** describes several detached servers, like a Compose file. Without `-f`, the first of
`sitefile.servers.yaml`, `sitefile.servers.yml` and `sitefile.servers.json` in the current directory is used.

```yaml
servers:
  docs:                       # the server's name (as serve -d --name)
    source: ./docs/dist       # what serve takes: a directory or archive file, or an image
    publish: 8081             # [host:]port, as serve -p (default: 127.0.0.1 and a free port)
  app:
    source: ./app/dist
    spa: true                 # as serve --spa (default false)
  theme:
    source: theme:1           # an image from the local store
```

- `servers` is the only top-level key; each server takes `source` (required), `publish` (a string, or a port
  number) and `spa`. Unknown keys, bad names and bad values are `E_DATA` naming the file and server. Names
  follow `serve --name` (letters, digits, `_`, `.`, `-`), and a name of digits only is refused.
- A `source` written as a path (starting with `./`, `../`, `/` or `~`, or ending in `/`) is relative to the
  servers file's directory, not the current one. A bare name is an image reference or ID, else an archive file
  of that name next to the servers file (`site.zip`).
- `up` resolves every entry first, so a typo or a missing directory starts nothing, then starts the servers in
  file order as `serve -d` would, recorded with the servers file's real path (`"project"` in the server record).
  It is idempotent:
  - a server that is running with the same source (the same image ID, or the same real path), `publish` and
    `spa` is left alone: `docs (<id>) is up to date at <url>`;
  - an exited one is restarted with the same ID (`Restarted …`);
  - one whose entry changed is stopped and replaced (`Recreated …`);
  - a new one is started (`Started …`).

  A server with the same name that the file did not start (`serve -d --name docs`, or another servers file)
  is refused (`E_SERVER`) before anything starts. A server the file started but no longer lists gets a
  warning and keeps running until `down`. If one server fails to start, the others still start, and `up`
  exits 1.
- `down` stops (SIGTERM, then SIGKILL after `--time` seconds) and removes every server the file started,
  also ones it no longer lists, and prints `Removed <name> (<id>)` for each. Other servers are not touched.
- `ps` lists the file's servers in file order (`NAME SITE URL STATUS ID`): `running`, `exited`, or `not
  created`. For a server that was never started, SITE is the `source` as written. Servers the file started
  but no longer lists come after the others, with `(not in the file)` added to their status. `--json` gives
  `[{"name", "site", "url", "status", "id", "listed"}]`.
- Not yet: watch-mode servers (`serve --watch` runs in the foreground only), and parked folders
  (`<name>.localhost` for every subfolder, which needs host-name routing). See johnhenry/sitefile#223.
