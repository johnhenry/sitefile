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

## Not yet

- Plugins from URLs or npm packages (local files only for now)
- Running external programs such as `ffmpeg` (planned as a separate, explicitly
  allow-listed adapter)
- Caching plugin results between builds
