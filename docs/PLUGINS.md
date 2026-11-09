# Writing renderer plugins

A plugin turns an input (a file, a value, or one element of a list) into one
or more output files. Use one when templates aren't enough: rendering text to
a motion graphic, generating OpenGraph cards, minifying, syntax highlighting,
converting formats.

```
PLUGIN motion FROM ./plugins/motion.mjs [INTEGRITY sha256:…]
TRANSFORM ./title.txt WITH motion TO /title.svg OPTIONS style=typewriter duration=2
TRANSFORM WITH motion TO /cards/${post.slug}.svg OPTIONS text=${post.title} FOR post IN posts
```

See `examples/motion/` for a complete plugin (text → animated SVG), and
[The image plugin](#the-image-plugin) for the first-party plugin that resizes
and converts images.

## The module

A JavaScript plugin is one ES module file ending in `.mjs`, **in its own folder** (for
example `./plugins/motion.mjs`). Export `transform`, or a default object with
it:

```js
export const apiVersion = 1;

export async function transform(request) {
  const text = request.input?.text ?? request.options.text ?? '';
  return { bytes: `<svg xmlns="http://www.w3.org/2000/svg"><text y="20">${escape(text)}</text></svg>` };
}
```

### The request

```ts
{
  apiVersion: 1,
  dest: string,                       // artifact path being written, e.g. "/title.svg"
  options: Record<string, string>,    // OPTIONS key=value … (always strings; ${ARG} and ${item.key} are interpolated)
  input?: {                           // when the TRANSFORM has a file source
    path: string,                     // "./title.txt", "name:/path", "<<EOF" …
    name: string,                     // file name
    bytes: Uint8Array,
    text?: string,                    // set when the bytes are valid UTF-8
  },
  data?: unknown,                     // when the source is a value import (json, yaml, csv, markdown, wbn)
  item?: unknown,                     // the current element, with FOR item IN list
}
```

Sources work like COPY's: a local path, `name:` / `name:/path` for an import,
`--from=stage /path`, or an inline `<<EOF` body. With `FOR … IN …` the source
is optional.

### A directory of files

A directory source calls `transform` once per file, in path order, each with
its own `input` and `dest` ([LANGUAGE.md](LANGUAGE.md#plugins-plugin-and-transform-see-docspluginsmd-examplesmotion)):

```
TRANSFORM --include=*.jpg ./photos/ WITH image TO /img/ OPTIONS widths=400,800
TRANSFORM ./photos/ WITH image TO /img/thumbs/*.png OPTIONS widths=160
TRANSFORM --exclude=*.min.css ./css/ WITH minify TO /css/*
```

- `TO /img/`: `dest` is `/img/<relative dir>` (a directory: return `{ files }`).
- `TO /thumbs/*.png`: `dest` is `/thumbs/<relative dir>/<name>.png` (one file).
- `TO /css/*`: `dest` is `/css/<relative path>` (one file, same name).

Each call is cached under the same key as a single-file TRANSFORM of that
file to that destination, so splitting or merging TRANSFORM lines keeps the
cache. Wasm plugins and tools run up to 4 calls at a time (each wasm call gets
its own instance anyway); a JavaScript plugin gets one call at a time.
Results, warnings and errors are always in path order, and a failure names
the file. Two files whose outputs collide with different content are
`E_CONFLICT` (rename one or filter with `--include`/`--exclude`).

### The result

Return one of:

- a `string` or `Uint8Array`, or `{ bytes }`: one file written to `dest`
- `{ files: { "a.svg": …, "sub/b.png": … } }`: several files under `dest`,
  which should end in `/`; keys are relative POSIX paths (no `..`)

Throwing an `Error` fails the build with `E_PLUGIN` and your message.
`console.log` output appears as build warnings prefixed `[plugin name]`.

Writes follow the usual conflict rules: different content at an existing path
fails unless the TRANSFORM has `--overwrite`.

## The sandbox

Each plugin runs in its own Node process started with Node's permission model:

```
node --permission --allow-fs-read=<the plugin's folder> --max-old-space-size=<MiB> …
```

| A plugin can | A plugin cannot |
| --- | --- |
| read files in its own folder (helpers, fonts, data) | read other files, including your project |
| import `node:` built-ins and files in its folder | write files, spawn processes, use workers or native addons |
| use CPU, and memory up to `--plugin-memory` (default 1 GiB) | open network connections |
| | see environment variables (it gets an empty environment) |

Each call has a time limit (`--plugin-timeout`, default 60 s) and results are
capped at 64 MiB. Processes are stopped when the build ends.

Memory is capped by `--plugin-memory <MiB>` (default 1024, the same limit as a
wasm plugin's linear memory), passed to Node as `--max-old-space-size`. A
plugin whose heap passes it is stopped by V8 and the build fails with
`E_PLUGIN: PLUGIN name: ran out of memory (more than the 1024 MiB limit)`
(V8's report and native stack trace are not shown). The limit is on the
JavaScript heap: the contents of `Buffer`s, typed arrays and `ArrayBuffer`s
live outside it and are only bounded indirectly, so a plugin that allocates
large buffers can use more memory than the limit before it is stopped.

### Which Node runs plugins

The sandbox is Node's permission model, so it is only as strong as the Node
binary that enforces it. That binary is:

1. `--node /absolute/path` (`BuildOptions.node` / `BuildOptions::node` for
   programs that call the library), else
2. `$SITEFILE_NODE` (an absolute path; ignored when empty), else
3. the default: in the TypeScript CLI, the Node running sitefile
   (`process.execPath`); in the Rust binary, `node` on `PATH`. A `PATH` you
   don't control can swap the runtime, so name the binary when it matters.

A relative `--node` or `SITEFILE_NODE` is a usage error. Before the first
JavaScript plugin starts, sitefile resolves the binary to its real path, runs
`<node> --version` (empty environment) and requires **Node 26.0.0 or later**:
network permissions arrived in Node 25 (an older `--permission` does not block
sockets), and 26 is the version sitefile requires and tests with. An older or
unrecognized binary fails the build at the `PLUGIN` line with `E_PLUGIN`, for
example `PLUGIN motion: node v22.1.0 (/usr/bin/node) is too old for the plugin
sandbox: JavaScript plugins need Node 26.0.0 or later; choose another with
--node or SITEFILE_NODE`. The binary that passed is printed (`plugins run on
node v26.11.1 (/opt/node/bin/node)`), next to the paths of allowed tools.
The check runs when the first plugin process starts, so a build whose
plugins are all cache hits does not run it.

## Trust model: `--allow-plugin`

Node describes its permission model as a "seat belt" for trusted code, not a
defence against deliberately malicious code. A JavaScript plugin is code from
the repository you are building, so building someone else's Sitefile would
otherwise run their code on your machine. JavaScript plugins therefore run
only when you allow them on the command line, by the name the `PLUGIN` line
declares:

```
sitefile build --allow-plugin motion   # this plugin (repeatable)
sitefile build --allow-plugins         # every PLUGIN in the Sitefile
```

- Without it the build stops with `E_UNSUPPORTED` at the `PLUGIN` line and
  the exact flag to add, before any import is fetched or any plugin process
  starts. The Sitefile cannot allow its own plugins; there is no environment
  variable or config file for it (as with `--allow-exec`). For programs that
  call the library, that error (and a `TOOL`'s) also carries `needs`:
  `{ kind: 'plugin' | 'tool', name, flag: '--allow-plugin' | '--allow-exec' }`
  in TypeScript, `SitefileError::needs` (`Needs`) in Rust, so nothing has to
  parse the message. Refused hosts carry one too (`kind: 'host'` and
  `'secret-host'`, see [CLI.md](CLI.md#strict-network-mode)), and `--json`
  prints it on stdout ([CLI.md](CLI.md#--json-errors)).
- An `--allow-plugin` name that matches no `PLUGIN` is a warning.
- `validate` never runs plugins and needs no flag. `inspect` builds in memory,
  so it needs the flag like `build`. The language server never runs plugins.
- **WebAssembly plugins need no flag.** A wasm module that imports nothing
  (checked when the build starts) can only compute: no files, network, clock,
  randomness or processes, and it runs under wasmtime (Rust) or V8 (TypeScript) with a time and
  memory limit. That is a real sandbox rather than a seat belt, so treating
  it like a template keeps untrusted builds useful. A `.wasm` path that is a
  symlink to a JavaScript file runs as JavaScript and needs the flag.

Allowing a plugin is still trusting it: treat it like any dependency, keep it
in your repository, and pin it with `INTEGRITY`. Because a plugin can read its
whole folder, sitefile refuses a plugin placed directly in the build context
root.

### What `INTEGRITY` pins

A JavaScript plugin can read and import every file in its folder (helpers,
data, fonts, a vendored `node_modules`), so **`INTEGRITY` on a JavaScript
plugin pins the whole folder**, not just the module: it is `sha256:` followed by
the same folder digest the TRANSFORM cache uses (below). A changed, added or
removed file anywhere under the folder fails the build with `E_INTEGRITY`
before any plugin starts; the error gives the digest the folder has now, so
the easiest way to pin a plugin is to write any digest and copy the one from
the error:

```
error[E_INTEGRITY]: Sitefile:3:1: integrity mismatch for ./plugins/ (PLUGIN motion's folder: INTEGRITY covers every file in it): expected sha256:…, got sha256:48225c…
```

The digest is SHA-256 over one line per regular file, in UTF-16 code unit
order of the relative path, recursing into directories:
`<relative/path>\0sha256:<hex of the file>\n`. A pinned folder may hold only
regular files and directories: a symlink (which could change what an import
resolves to without changing any file) is `E_INTEGRITY`, as is a folder over
2000 files or 128 MiB. The check runs when the build starts; sitefile does not
stop the files changing between the check and the plugin reading them, so
keep the folder in your repository. A **wasm** plugin reads nothing, so its
`INTEGRITY` is the digest of the module file (`shasum -a 256 x.wasm`).

Before johnhenry/sitefile#158, `INTEGRITY` on a JavaScript plugin hashed only
the module file; such a pin now fails with the folder's digest in the error.

## Determinism

sitefile builds are reproducible, so plugins should be too: produce the same
bytes for the same request. Avoid `Date.now()`, `Math.random()` and anything
that depends on the machine. (Reading the clock is not blocked; it is just a
bad idea.)

**State carries over between calls.** A JavaScript plugin runs as one Node
process for the whole build: the module is loaded once and every
`transform()` call of every TRANSFORM line that uses it runs in that process,
one at a time. Module-level variables, caches and counters keep their values
from one call to the next, so a result can depend on which calls came first,
and on whether earlier calls were answered from the cache instead (then they
never ran). Keep `transform()` a function of its request. (A wasm plugin gets
a fresh instance for every call, so nothing carries over.)

## WebAssembly plugins

`PLUGIN name FROM ./plugins/x.wasm` loads a WebAssembly module instead of a
JavaScript one. Everything else (TRANSFORM, OPTIONS, FOR, INTEGRITY, caching)
works the same. See `examples/wasm/` for a plugin written in Rust
(`plugins/badge-src`, built with `./build.sh`).

**The module imports nothing.** That is checked when the build starts, and
it's what makes wasm plugins the strongest sandbox sitefile offers: with no
imports a module can't open files, use the network, read the clock or get
randomness. It can only compute. So a wasm plugin may live anywhere in the
context (it can't read its folder), and its results are always cacheable.

### ABI v1

Build for `wasm32-unknown-unknown` (Rust: a `cdylib`; C: `clang
--target=wasm32 -nostdlib -Wl,--no-entry -Wl,--export-all`; or hand-written
WAT) and export:

| Export | Signature | Meaning |
| --- | --- | --- |
| `memory` | memory | the module's linear memory |
| `sitefile_alloc` | `(len: i32) -> i32` | reserve `len` bytes for the request; return a pointer |
| `sitefile_transform` | `(ptr: i32, len: i32) -> i64` | read the request at `ptr`, return `(resultPtr << 32) \| resultLen` |
| `sitefile_api_version` | `() -> i32` | optional; must return `1` |

The **request** is UTF-8 JSON: the same object JS plugins receive, with
`input.bytes` base64-encoded. The **result** is UTF-8 JSON, one of:

```json
{ "text": "<svg …>" }
{ "bytes": "<base64>" }
{ "files": { "a.svg": "<base64>", "sub/b.png": "<base64>" } }
{ "error": "option \"color\" must be #rgb" }
```

Each call gets a **fresh instance**, so nothing carries over between calls
and the module may simply leak its allocations. Calls run in a worker thread
that is terminated after `--plugin-timeout`; linear memory is capped by
`--plugin-memory` (default 1 GiB) and results at 64 MiB. Traps, out-of-range results, invalid JSON and
`{"error"}` all fail the build with `E_PLUGIN`.

## The image plugin

sitefile ships a first-party WebAssembly plugin for responsive images:
[`plugins/image/image.wasm`](../plugins/image/), with its Rust source and
build script next to it. Copy the `.wasm` into your project and pin it:

```
PLUGIN image FROM ./plugins/image.wasm INTEGRITY sha256:…   # the digest in examples/image/Sitefile
TRANSFORM ./photos/sky.jpg WITH image TO /img/ OPTIONS widths=400,800,1200 quality=80
TRANSFORM ./logo.png WITH image TO /img/ OPTIONS widths=64,128 format=webp,png
TRANSFORM ./photos/sky.jpg WITH image TO /img/sky-thumb.png OPTIONS widths=160
# A whole folder: one call per photo (/img/<name>-<width>.jpg, /img/<name>.json) ...
TRANSFORM --include=*.jpg ./photos/ WITH image TO /img/ OPTIONS widths=400,800
# ... or one thumbnail per photo, named after it (/img/thumbs/<name>.png).
TRANSFORM ./photos/ WITH image TO /img/thumbs/*.png OPTIONS widths=160
```

Like every wasm plugin it needs no `--allow-plugin`, can't reach files, the
network or the clock, and its results are cached by content digest (an
unchanged photo is not decoded again). The same module runs under V8 and
wasmtime, so both implementations write identical bytes; the conformance
suite and both test suites check that with pinned digests. See
`examples/image/`.

**Formats.**

| | Decode | Encode |
| --- | --- | --- |
| PNG | yes (8 and 16 bit, palette, gray, alpha; APNG: first frame) | yes: 8-bit RGB, or RGBA when any pixel is transparent; adaptive filters, the png crate's `Balanced` deflate level |
| JPEG | yes (baseline and progressive, gray, CMYK) | yes: baseline, `quality` (default 82), 4:2:2 chroma subsampling, JFIF header |
| GIF | yes (first frame) | no (written as PNG by default) |
| WebP | yes (lossy and lossless; animated: first frame) | lossless only |
| AVIF, HEIC, SVG, TIFF | no | no |

There is no pure-Rust lossy WebP or AVIF encoder the plugin could pin, so
lossless WebP is meant for graphics and screenshots; for photos it is often
larger than the JPEG. AVIF is out of scope for now. Animated images become
their first frame. Metadata (EXIF, ICC profiles, text chunks, XMP) is never
copied; EXIF orientation is applied first, so a portrait photo stays
portrait. Colors are not converted from embedded ICC profiles (the pixel
values are kept as they are, which is right for sRGB images).

**Resizing** uses one fixed filter: Lanczos-3 (`sinc(x)·sinc(x/3)`, |x| < 3),
widened by the scale factor when shrinking, on premultiplied alpha (so
transparent pixels don't bleed), in sRGB space (no gamma conversion), with
14-bit integer weights and integer arithmetic. The height keeps the aspect
ratio, rounded half up. Images are never enlarged: a width larger than the
source is dropped, and if none is left the source width is used. At the
source width the pixels are copied unchanged.

**Destinations.**

- `TO /dir/` (anything whose last segment has no image extension): one file
  per width and format, `<name>-<width>.<ext>` (`ext` is `png`, `jpg` or
  `webp`), plus a metadata file `<name>.json`. `<name>` is the source's file
  name without its extension (characters other than letters, digits, `-`,
  `_` and `.` become `-`), or the `name` option.
- `TO /file.png|.jpg|.jpeg|.webp`: exactly one image; the extension picks the
  format, so `format`, `name` and more than one width are errors.

**Options** (all optional; anything else is an error):

| Option | Meaning |
| --- | --- |
| `widths=400,800,1200` | output widths, 1 to 16384, at most 16 (sorted, duplicates dropped). Default: the source width |
| `format=webp,jpeg` | output formats in order: `png`, `jpeg` (or `jpg`), `webp`. Default: JPEG for a JPEG source, WebP for WebP, otherwise PNG |
| `quality=82` | JPEG quality, 1 to 100 |
| `background=#ffffff` | color transparent pixels are composited over for JPEG (`#rgb` or `#rrggbb`) |
| `name=hero` | the file name stem in a directory destination |
| `max_pixels=40000000` | refuse larger images; at most the default, 40,000,000 |

**Limits.** The image size is read from the file header (PNG IHDR, GIF
screen descriptor, WebP VP8/VP8L/VP8X, JPEG SOF) before anything is decoded,
so a decompression bomb (a small file claiming 60000×60000 pixels) is refused
at once with `E_PLUGIN`. Decoders may allocate at most 512 MiB, the module at
most 1 GiB, a call is stopped after `--plugin-timeout` (default 60 s; one
TRANSFORM of a 24 megapixel JPEG to three widths as JPEG and PNG took 5 to 15
seconds on a laptop, so raise it for very large images or many widths), and
the result is capped at 64 MiB. In the Rust build, compiling the 800 KB module
takes a few seconds the first time; it is then kept in the wasm module cache
(docs/RUST.md). Malformed, truncated or unsupported files fail
with `E_PLUGIN` and a message such as `not a PNG, JPEG, GIF or WebP image`;
the decoders are fuzzed lightly (mutated and truncated copies of every test
image, natively and through both runtimes).

### The metadata file

A directory destination also gets `<name>.json`, which the template helpers
read (docs/LANGUAGE.md, "File helpers"). Version 1:

```json
{
  "version": 1,
  "source": { "name": "sky.jpg", "format": "jpeg", "width": 1600, "height": 1000 },
  "width": 1200,
  "height": 750,
  "src": "/img/sky-1200.jpg",
  "type": "image/jpeg",
  "srcset": "/img/sky-400.jpg 400w, /img/sky-800.jpg 800w, /img/sky-1200.jpg 1200w",
  "sources": [
    { "type": "image/jpeg", "srcset": "/img/sky-400.jpg 400w, /img/sky-800.jpg 800w, /img/sky-1200.jpg 1200w" }
  ],
  "variants": [
    { "path": "/img/sky-400.jpg", "type": "image/jpeg", "width": 400, "height": 250, "size": 20826 }
  ]
}
```

- `source`: the input's name, decoded format (`png`, `jpeg`, `gif`, `webp`)
  and size after EXIF orientation.
- `width`, `height`, `src`, `type`, `srcset`: the largest variant of the
  **last** format listed (the fallback for a plain `<img>`).
- `sources`: one entry per format, in `format` order, for `<picture>`
  `<source>` elements (modern formats first, fallback last).
- `variants`: every file written, by width then format: its absolute artifact
  path, media type, size in pixels and in bytes.
- Paths are where the TRANSFORM wrote the files. A later step that renames
  them (fingerprinting, #98) must map them. New fields may be added within
  version 1; a change to existing ones bumps `version`.

Templates read it with the file helpers: `${file "/img/sky.jpg" | img alt="…"
sizes="…"}` (the source's name, or `/img/sky.json` itself) writes
`<img src srcset width height …>` from it, inside a `<picture>` with a
`<source type srcset>` per other format when there are several; `url`, `type`,
`width`, `height` and `srcset` give the single values. See
`examples/image/` (a page and a gallery).

**Rebuilding the module.** `plugins/image/build.sh` builds it with
`cargo build --release --target wasm32-unknown-unknown --locked --offline`
(crate versions pinned exactly in `Cargo.toml` and `Cargo.lock`; path
prefixes remapped so the build doesn't embed your home directory), copies it
to `examples/image/plugins/` and updates the example's `INTEGRITY`.
`./build.sh --check` only compares the result with the committed module. The
committed module was built with rustc 1.93.1; another compiler version gives
a working module with different bytes, and so a different digest and cache
key. `cargo test` in `plugins/image` runs the native tests and fuzzing.

## Example: rendering a fileable tree

[`@johnhenry/fileable`](https://opensource.johnhenry.me/fileable/) describes
file trees in JSX. `examples/fileable/` is a JavaScript plugin that renders
such a tree straight into the build and returns it as `{ files }`, with no
directory or zip on disk in between:

```
PLUGIN fileable FROM ./plugins/fileable/fileable.mjs
IMPORT docs FROM ./docs.json AS json
TRANSFORM docs: WITH fileable TO /docs/ OPTIONS template=docs.mjs title="Fileable docs"
```

`template` names a module in the plugin's `templates/` folder (a plain file
name, no directories). Its default export receives `{ options, data, item,
input, dest }` and returns a fileable tree, built with fileable's
`h`/`dir`/`file` builders or with JSX compiled ahead of time; the plugin
renders it with fileable's in-memory `toFiles()` and the tree's paths land
under `TO`.

It is an example, not part of sitefile: sitefile has no runtime dependencies
and fileable is not one. Because a plugin reads only its own folder,
fileable has to be in it. `node examples/fileable/vendor.mjs` copies the
files Node loads for `@johnhenry/fileable/browser` (about 45 files, 600 KB:
fileable's browser entry point and the parts of marked, minimatch and
node-html-parser it uses) from the repository's `node_modules`, where
fileable is a devDependency, into `plugins/fileable/node_modules/`. That copy
is gitignored and rebuilt by `npm run examples` and `npm run conformance`. In
your own project, vendor it the same way (or with any bundler) and commit or
rebuild it; `INTEGRITY` covers the vendored files too (the whole folder), so
pin it only when the vendored copy is reproducible or committed.

The browser entry point needs no filesystem, processes or environment, so it
fits the sandbox. What does need them fails with `E_PLUGIN` and says why:
fileable's `cmd` attribute (a shell command; a plugin cannot start processes,
and Node's permission model would refuse `child_process` anyway, so use
`TOOL` and `--allow-exec`), local `src=` paths and `<Dir from>` (pass the
content as the TRANSFORM source or a value import instead), `src="env://…"`
(the environment is empty) and `encode="zip"`/`"wbn"` (let sitefile write the
archive). Like every JavaScript plugin it needs `--allow-plugin fileable`.

## External tools: `TOOL` and `--allow-exec`

Plugins cannot run programs. For real media output (video, audio, images) a
build can instead call an external program such as `ffmpeg`, but only through
an explicit, command-line allow-list:

```
TOOL ffmpeg
TRANSFORM --overwrite WITH ffmpeg TO /intro.mp4 OPTIONS size=1280x720 \
  ARGS -hide_banner -loglevel error -f lavfi -i "color=c=black:s={option.size}:d=3:r=30" \
       -c:v mpeg4 -fflags +bitexact -flags:v +bitexact -map_metadata -1 {output}
```

```
sitefile build --allow-exec ffmpeg                 # looked up on PATH
sitefile build --allow-exec ffmpeg=/opt/bin/ffmpeg # or an absolute path
```

- `TOOL name` only *declares* the need. Without `--allow-exec name` the build
  stops with `E_UNSUPPORTED` and the exact flag to add. The program is resolved
  to an absolute real path when the build starts, and printed.
- A TRANSFORM that names a tool takes a source file as for plugins (or none),
  `OPTIONS`, `FOR`, and last `ARGS …`: the argv template. A directory source
  runs the tool once per file; it then needs `TO <dir>/*` (same names) or
  `TO <dir>/*.ext` (`TRANSFORM ./clips/ WITH ffmpeg TO /video/*.mp4 ARGS -i
  {input} … {output}`), and the tool's output is shown per file in path order. Placeholders:
  `{input}` (the source, copied to a file in a per-call temp directory),
  `{output}` (required; the single file the tool must write; its extension
  comes from `<dest>` so tools can pick a format), and `{option.key}`. Both
  file placeholders are *relative* names (`input.txt`, `output.mp4`) because the
  tool's working directory is that temp directory, so filter syntaxes that
  treat `:` or `,` specially need no escaping. Any other `{…}` is literal.
- **No shell, ever.** The program is started with an argv array
  (`spawn(path, argv, { shell: false })`). `;`, `$(…)`, backticks, `*`, `|` in
  arguments or option values reach the program as plain text. Values are
  substituted once and never re-scanned. (An argument that *starts with* `-` is
  still an option to the tool: do not put untrusted `${…}` values in option
  position.)
- Per call: a fresh temp directory (removed afterwards) holding the input;
  working directory = that directory; environment = `PATH=/usr/bin:/bin` only
  (none of your variables or secrets; the OS may add its own); stdin closed;
  killed after `--plugin-timeout` (default 60 s); output larger than 64 MiB (or
  not a regular file, or missing) is rejected. stdout/stderr are kept to their
  tail and shown as build warnings (`[tool ffmpeg] …`, at most 20 lines each);
  use `-hide_banner -loglevel error` to keep that quiet. A non-zero exit is
  `E_PLUGIN` with the last stderr lines.

**Trust model, honestly.** Node's `--permission` sandbox only governs Node
code: it cannot confine a native binary. A tool runs **with your user's
privileges** and can read or write anything you can, and the network too. The
allow-list (you name each program on the command line, the Sitefile cannot
widen it) and the missing shell are the controls, not a sandbox. Allow
programs you trust, with arguments you have reviewed; a Sitefile from someone
else can still make an allowed program do what its arguments permit. Prefer
`name=/absolute/path` when `PATH` is not trustworthy.

**Determinism.** Reproducible builds need reproducible tools. ffmpeg embeds
its encoder name, timestamps and metadata by default; use
`-fflags +bitexact -flags:v +bitexact -map_metadata -1` (the test suite checks
this gives identical bytes). Multi-threaded encoders can still differ between
versions or CPUs. Caching (below) makes repeated builds stable by reusing the
first result.

## Caching

TRANSFORM results are cached between builds in
`$SITEFILE_HOME/cache/transform` (default `~/.sitefile/cache/transform`): an
unchanged TRANSFORM is not run again, and **the plugin process is not even
started** (plugins start lazily, on their first cache miss).

The key is the SHA-256 of a canonical JSON (sorted keys) of:

- the cache format version;
- the code: for a plugin, a digest of every file in its folder (it can read
  them all, so any may matter; folders over 2000 files or 128 MiB are not
  cached, with a warning); for a tool, its resolved real path, size and mtime,
  and the interpolated ARGS;
- the request: `dest`, `options`, the input (path, name, content digest), the
  value-import `data`, and the `FOR` `item`.

Results are stored by content hash (`entries/<key>.json` pointing at
`blobs/<sha256>`). An entry also records the `PLUGIN` or `TOOL` that produced
it (`"plugin": "<name>"`, after `version`, `kind` and `files`), so
`sitefile cache prune --plugin <name>` can clear one plugin's results; the name
is not part of the key, and entries written before it was recorded (no
`plugin`) are read as before. Both implementations write the same bytes. Every read re-verifies each blob's digest and size, so a
corrupt or truncated entry is ignored, removed, and rewritten after the rebuild;
a cache that cannot be written never fails the build.

**Caching assumes deterministic code.** WebAssembly plugins are deterministic
by construction (no imports: no clock or randomness, and a fresh instance per
call). JavaScript plugins are not: they have `Date`, `Math.random` and module
state that lasts for the whole build. So a JavaScript plugin says which it is
by exporting `cache`:

```js
export const cache = true;          // deterministic: results may be cached and reused
export const cache = false;         // or: export default { apiVersion: 1, cache: false, transform }
```

With `cache = false` its results are never stored or reused (it is started on
every build). Any other value is `E_PLUGIN` when it loads. A plugin that
exports neither is still cached, as before, but the first call of a build
that caches or memoizes results (a cache directory, or `serve --watch`)
warns once:

```
warning: PLUGIN motion does not export cache, so its results are reused as if it were deterministic: export const cache = true to confirm, or cache = false if a result can depend on the clock, randomness or an earlier call
```
Anything not in the key is invisible to the cache: a tool that reads files
other than `{input}`, or a different version of a tool installed at the same
path with the same size and mtime. Use `--no-cache` to bypass (no reads, no
writes), and `sitefile cache info|prune|du` to inspect or clear it; it is kept
under `SITEFILE_CACHE_MAX` (default 1 GiB, least recently used entries evicted
first, see [CLI.md](CLI.md#cache-size-limit)). The cache
neither reads nor changes `Sitefile.lock`, so `--frozen-lockfile` is unaffected.

The cache can be shared through an OCI registry with `build --cache-from <ref>`
and `--cache-to <ref>` ([CLI.md](CLI.md#registry-cache)): the same keys, so a
result computed on one machine is reused on another only when everything in its
key is equal (a plugin's folder digest is portable; a tool's key includes its
resolved path, size and mtime, so tool results only match where the same
binary sits at the same path with the same mtime, such as one CI image). A plugin that exports
`cache = false` is never pushed. Only use `--cache-from` with a registry
repository you trust: a cached result is used as if the plugin had returned it.

### What sitefile trusts as-is

Some stored results are used without being recomputed or checked against
their inputs. Each is only as trustworthy as whoever can write to it:

| Trusted as-is | Why | Protected by |
| --- | --- | --- |
| `--cache-from <ref>` results (JavaScript, wasm and tool results alike) | a hit is written into the output as if the plugin or tool had returned it; the blob digests prove the bytes are the ones the entry names, not that the plugin produced them | you: only pull from a registry repository whose writers you trust |
| the local TRANSFORM cache (`$SITEFILE_HOME/cache/transform`) | the same, for results from earlier builds on this machine | file permissions on `$SITEFILE_HOME` (anyone who can write there can change a build's output) |
| the compiled wasm module cache (`$SITEFILE_HOME/cache/wasm`, Rust only) | compiled native code is loaded with wasmtime's `Module::deserialize_file`, which is `unsafe`: a forged file runs as native code, outside the wasm sandbox | the directory is created `0700` and entries `0600`, and a key covers the module bytes, the wasmtime version, the target and the codegen settings ([RUST.md](RUST.md#wasm-module-cache)); `SITEFILE_NO_WASM_CACHE=1` turns it off |

`--no-cache` skips the TRANSFORM caches (local and registry, and cannot be
combined with `--cache-from`); `SITEFILE_NO_WASM_CACHE=1` skips the wasm one.

## Not yet

- Plugins from URLs or npm packages (local files only for now)
