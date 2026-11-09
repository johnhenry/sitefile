# sitefile examples

Each directory is a build context (a `Sitefile` and its inputs). `npm run
examples` builds every one with the TypeScript CLI into a temporary directory
and checks what it is meant to show (`examples/run.mjs`; it fails if a
directory is added without a check). `npm run conformance` builds them all with
both implementations and requires identical output.

| Example | Demonstrates |
| --- | --- |
| [`hello/`](./hello/) | `ARG` + `RENDER --escape=html`: `--build-arg TITLE="Tom & <Jerry>"` reaches the page HTML-escaped, and `COPY` places the CSS. |
| [`inline/`](./inline/) | A template written inside the Sitefile (`<<HTML` heredoc) renders like a template file. |
| [`multistage/`](./multistage/) | Several `FROM` stages and `COPY --from`: only the last stage is exported. |
| [`shared-stages/`](./shared-stages/) | `IMPORT theme FROM ./theme AS sitefile ARG …`: another Sitefile's stages are reusable as `theme+<stage>`, and an `ARG` passed to it changes its output. |
| [`blog/`](./blog/) | Markdown posts with a frontmatter `SCHEMA`, one page per post (`FOR`), per tag (`BY`) and per page of 2 (`PAGE`), plus `FEED`, `SITEMAP`, `FINGERPRINT` and `CHECKLINKS`. |
| [`precompress/`](./precompress/) | `PRECOMPRESS`: deterministic `.gz` sidecars of the text files (not of the ones under `MIN`), for nginx `gzip_static` and Caddy `precompressed`. |
| [`data/`](./data/) | YAML, CSV and Web Bundle imports as template data, and a `.wbn` unpacked `AS tree`. |
| [`motion/`](./motion/) | A JavaScript `PLUGIN` (text → animated SVG) runs only with `--allow-plugin motion`; without it the build fails before anything runs. |
| [`canonical/`](./canonical/) | `CANONICAL … PRETTY`: a `<link rel="canonical">` in every page that has none, without `.html`. |
| [`fileable/`](./fileable/) | A JavaScript plugin renders a `@johnhenry/fileable` tree into the build as `{ files }` (`TRANSFORM docs: WITH fileable TO /docs/`). fileable is vendored into the plugin folder by `node fileable/vendor.mjs` (gitignored), since a plugin reads only its own folder; its `cmd` attribute fails clearly (`--build-arg TEMPLATE=cmd.mjs`). |
| [`wasm/`](./wasm/) | An import-free WebAssembly plugin runs with no flag. |
| [`image/`](./image/) | The first-party image plugin (pinned with `INTEGRITY`) writes resized variants and a metadata file. |
| [`theme/`](./theme/) | A site image: built with `-t theme:1` into the local image store rather than exported. |
| [`themed-site/`](./themed-site/) | `FROM theme:1`: a site that starts from a stored theme image. Needs `theme/` built first (the runner does it). |
| [`bake/`](./bake/) | `sitefile bake`: a matrix of 2 locales × 2 environments builds four variants side by side. |

## Running

```sh
npm run examples            # build, then check all of them
node examples/run.mjs blog  # one (after npm run build)
sitefile build examples/hello --build-arg TITLE="My site"
```

## Runtime requirements

Node 26 and nothing else (plus `npm ci` at the repository root for `fileable/`, whose plugin vendors the `@johnhenry/fileable` devDependency): no network, no container engine and no external
tools. Remote imports, registries and `TOOL` programs are covered by the
network conformance cases (`conformance/cases/net-*`) and the test suite, which
run local servers instead.

These examples are directories, not the family's usual
`examples/NN-<what-it-proves>.mjs` scripts, because a Sitefile build context
is the unit a reader copies; renaming them would also break the conformance
cases and docs that refer to them by name.
