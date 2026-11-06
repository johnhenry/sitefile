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
node --permission --allow-fs-read=<the plugin's folder> …
```

| A plugin can | A plugin cannot |
| --- | --- |
| read files in its own folder (helpers, fonts, data) | read other files, including your project |
| import `node:` built-ins and files in its folder | write files, spawn processes, use workers or native addons |
| use CPU and memory | open network connections |
| | see environment variables (it gets an empty environment) |

Each call has a time limit (`--plugin-timeout`, default 60 s) and results are
capped at 64 MiB. Processes are stopped when the build ends.

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
  variable or config file for it (as with `--allow-exec`).
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
in your repository, and pin it with `INTEGRITY` (which covers the module file
itself; other files in its folder are not hashed). Because a plugin can read
its whole folder, Sitefile refuses a plugin placed directly in the build
context root.

## Determinism

Sitefile builds are reproducible, so plugins should be too: produce the same
bytes for the same request. Avoid `Date.now()`, `Math.random()` and anything
that depends on the machine. (Reading the clock is not blocked; it is just a
bad idea.)

## WebAssembly plugins

`PLUGIN name FROM ./plugins/x.wasm` loads a WebAssembly module instead of a
JavaScript one. Everything else (TRANSFORM, OPTIONS, FOR, INTEGRITY, caching)
works the same. See `examples/wasm/` for a plugin written in Rust
(`plugins/badge-src`, built with `./build.sh`).

**The module imports nothing.** That is checked when the build starts, and
it's what makes wasm plugins the strongest sandbox Sitefile offers: with no
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
that is terminated after `--plugin-timeout`; linear memory is capped at 1 GiB
and results at 64 MiB. Traps, out-of-range results, invalid JSON and
`{"error"}` all fail the build with `E_PLUGIN`.

## The image plugin

Sitefile ships a first-party WebAssembly plugin for responsive images:
[`plugins/image/image.wasm`](../plugins/image/), with its Rust source and
build script next to it. Copy the `.wasm` into your project and pin it:

```
PLUGIN image FROM ./plugins/image.wasm INTEGRITY sha256:…   # the digest in examples/image/Sitefile
TRANSFORM ./photos/sky.jpg WITH image TO /img/ OPTIONS widths=400,800,1200 quality=80
TRANSFORM ./logo.png WITH image TO /img/ OPTIONS widths=64,128 format=webp,png
TRANSFORM ./photos/sky.jpg WITH image TO /img/sky-thumb.png OPTIONS widths=160
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

A directory destination also gets `<name>.json`, for templates and for the
`img` template helper planned in #105. Version 1:

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

Templates can't read files from the tree yet, so for now a page writes its
`srcset` and `width`/`height` by hand (as `examples/image/templates/index.html`
does), using the numbers in the metadata file; #105 will read it instead.

**Rebuilding the module.** `plugins/image/build.sh` builds it with
`cargo build --release --target wasm32-unknown-unknown --locked --offline`
(crate versions pinned exactly in `Cargo.toml` and `Cargo.lock`; path
prefixes remapped so the build doesn't embed your home directory), copies it
to `examples/image/plugins/` and updates the example's `INTEGRITY`.
`./build.sh --check` only compares the result with the committed module. The
committed module was built with rustc 1.93.1; another compiler version gives
a working module with different bytes, and so a different digest and cache
key. `cargo test` in `plugins/image` runs the native tests and fuzzing.

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
  `OPTIONS`, `FOR`, and last `ARGS …`: the argv template. Placeholders:
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
`blobs/<sha256>`). Every read re-verifies each blob's digest and size, so a
corrupt or truncated entry is ignored, removed, and rewritten after the rebuild;
a cache that cannot be written never fails the build.

**Caching assumes deterministic code.** A plugin that depends on the clock,
randomness, the network or the machine should opt out by exporting `cache`:

```js
export const cache = false;         // or: export default { apiVersion: 1, cache: false, transform }
```

Its results are then never stored or reused (it is started on every build).
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

## Not yet

- Plugins from URLs or npm packages (local files only for now)
