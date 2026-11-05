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

See `examples/motion/` for a complete plugin (text → animated SVG).

## The module

A plugin is one ES module file ending in `.mjs`, **in its own folder** (for
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

**Trust model.** Node describes its permission model as a "seat belt" for
trusted code, not a defence against deliberately malicious code. Treat a
plugin like any dependency: run plugins you trust, keep them in your
repository, and pin them with `INTEGRITY` (which covers the module file
itself; other files in its folder are not hashed). Because a plugin can read
its whole folder, Sitefile refuses a plugin placed directly in the build
context root.

## Determinism

Sitefile builds are reproducible, so plugins should be too: produce the same
bytes for the same request. Avoid `Date.now()`, `Math.random()` and anything
that depends on the machine. (Reading the clock is not blocked; it is just a
bad idea.)

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
writes), and `sitefile cache info|prune` to inspect or clear it. The cache
neither reads nor changes `Sitefile.lock`, so `--frozen-lockfile` is unaffected.

## Not yet

- Plugins from URLs or npm packages (local files only for now)
