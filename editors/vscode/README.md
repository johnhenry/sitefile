# Sitefile for VS Code

Syntax highlighting and language server support for
[Sitefile](https://github.com/johnhenry/sitefile), the Docker-inspired static
site builder. Highlighting is a TextMate grammar; diagnostics, completion,
hover, go to definition, references, rename and the outline come from the
Rust binary's language server, `sitefile lsp` ([docs/LSP.md](../../docs/LSP.md)).

Files named `Sitefile`, `*.sitefile`, `Sitefile.*` and `*.Sitefile` open as
Sitefile.

## What it highlights

The grammar follows the real scanner and parser (`src/language/scanner.ts`,
`parser.ts`) and the template syntax in `src/render.ts`:

- **Instructions**, case-insensitive and optionally indented: `FROM`, `STAGE`,
  `ARG`, `LET`, `IMPORT`, `COPY`, `RENDER`, `TRANSFORM`, `PLUGIN`, `TOOL`,
  `SITEMAP`, `FEED`, `REDIRECT`, `HEADER`, `EXPORT`. `RUN`, `REMOVE`, `GENERATE`, `ASSERT`, `META` and `EXEC`, which the
  parser rejects, are marked invalid.
- **Clause keywords**: `AS`, `FROM`, `TO`, `FOR … IN`, `WITH`, `OPTIONS`,
  `ARGS`, `INTEGRITY`, `SORT BY … ASC|DESC`, `BASE`, `AT`, `COMPRESSION`,
  `LINK`, `TITLE`, `STATUS` (with the redirect status), and a `HEADER`'s `Name:`.
- **Values**: `scratch`; ARG/LET types (`string`, `number`, `boolean`); import
  kinds (`file`, `text`, `json`, `tree`, `markdown`, `yaml`, `csv`, `wbn`);
  export formats (`directory`, `zip`, `tar`, `tgz`, `tar.gz`, `wbn`, `oci`);
  `COMPRESSION gzip|none`; `--escape=none|html`. Unknown values are marked
  invalid.
- **Flags** `--overwrite`, `--from=<stage>`, `--escape=…` (only where the
  parser accepts them; unknown flags are marked invalid).
- **Names**: stage names, ARG/LET/IMPORT names, plugin and tool names, loop
  variables, sort keys, `name:` import references, TRANSFORM `key=value`
  options and `{input}` / `{output}` / `{option.key}` ARGS placeholders.
- **Operands**: `"double"` strings with `\` escapes, literal `'single'`
  strings, paths, `https://` URLs, `sha256:<64 hex>` integrity digests, numbers
  and booleans, and `${NAME}` / `${name.key.0}` interpolation.
- **Comments** (`#` as the first non-blank character only, also inside a
  continuation; `#` elsewhere is text) and `\` line continuations.
- **Heredocs** `<<NAME` and `<<-NAME` on `COPY`, `RENDER` and `TRANSFORM`. The
  body ends at a line that is exactly `NAME` (`<<-` also strips leading tabs).
  The delimiter picks an embedded language: `HTML`/`HTM` (HTML), `CSS`,
  `JS`/`JAVASCRIPT`/`MJS`, `MD`/`MARKDOWN`, `JSON`, `YAML`/`YML`, `SVG`/`XML`;
  anything else is plain text. An unclosed tag or string in the body cannot run
  past the closing line.
- **Templates** in `RENDER` bodies: `${x}`, `${x.y|raw}`, `${#each list as
  item}`, `${#if x}` / `${#if !x}`, `${else}`, `${/each}`, `${/if}` and the
  `$${` escape, also inside the embedded language's own strings (for example
  `href="${url}"`). Anything else inside `${…}` is marked invalid. `COPY` and
  `TRANSFORM` bodies are written byte for byte, so `${…}` there is not
  highlighted.

## Requirements

Highlighting needs nothing else. The language features need the `sitefile`
Rust binary (the crate in `rust/`, binary name `sitefile`) on your `PATH`:

```bash
cargo install --path rust        # from a clone of the repository; installs ~/.cargo/bin/sitefile
```

or build it (`cd rust && cargo build --release`) and point
`sitefile.server.path` at `rust/target/release/sitefile`. The TypeScript CLI
(the npm package's `sitefile` command) has no language server; if it is the
`sitefile` on your `PATH`, set the path to the Rust binary explicitly.

If the binary can't be found or started, the extension shows one warning with
a **Configure path** button and keeps highlighting. A server that crashes is
restarted at most 4 times in 3 minutes; then run **Sitefile: Restart Language
Server** from the command palette (changing a `sitefile.server.*` setting also
restarts it).

## Settings

| Setting | Default | |
| --- | --- | --- |
| `sitefile.server.enabled` | `true` | Start the language server for Sitefiles. |
| `sitefile.server.path` | `"sitefile"` | A command on `PATH`, an absolute path, or a path relative to the first workspace folder (`~` expands). |
| `sitefile.server.args` | `["lsp"]` | Arguments, passed as is (the server also accepts `--stdio`). |
| `sitefile.trace.server` | `"off"` | `messages` or `verbose` logs the LSP traffic to the **Sitefile Language Server** output channel. |

```json
{
  "sitefile.server.path": "~/src/sitefile/rust/target/release/sitefile",
  "sitefile.trace.server": "messages"
}
```

The client handles `file` and `untitled` Sitefile documents.

## Install locally

The extension is not on the Marketplace. Build the package and install it:

```bash
cd editors/vscode
npm install
npm run package          # builds dist/extension.js, writes sitefile-0.2.0.vsix
code --install-extension sitefile-0.2.0.vsix
```

(From the repository root: `code --install-extension editors/vscode/sitefile-0.2.0.vsix`.)

## Without the extension

Dockerfile highlighting is a reasonable stopgap. In your VS Code settings:

```json
"files.associations": {
  "Sitefile": "dockerfile",
  "*.sitefile": "dockerfile"
}
```

## Development

The client is `src/extension.ts`; the settings logic it uses (command
resolution, restart budget, document selector) is the `vscode`-free
`src/serverCommand.ts`. `npm run build` typechecks and bundles them with
esbuild into `dist/extension.js` (with `vscode-languageclient` inlined, so the
`.vsix` ships no `node_modules`); `vscode:prepublish` runs it.

The grammar is generated: edit `scripts/grammar.mjs`, then

```bash
npm run build:grammar    # writes syntaxes/*.json
npm test                 # build, then grammar, client and server tests
UPDATE_SNAPSHOTS=1 npm test   # accept intended token changes
npm run preview          # test/preview.html, rendered with shiki
```

The tests tokenize the Sitefiles in `test/fixtures/` and compare them with
`test/snapshots/*.snap` (line number → text and `[token, scopes]`). They also
fail when the parser accepts an instruction, clause keyword, value word or flag
the grammar doesn't know, when the committed JSON is out of date, or (after a
root `npm run build`) when a valid fixture no longer parses. Embedded grammars
are stubs in the tests; the preview uses the real ones.

The client tests don't download VS Code: `test/server-command.test.mjs` unit-tests
the command resolution, `test/extension-smoke.test.mjs` loads the bundle with a
stub `vscode` module (missing-binary warning, disabled server), and
`test/lsp-server.test.mjs` spawns the real `../../rust/target/release/sitefile lsp`
(built with cargo if missing, or `$SITEFILE_LSP_BIN`) with the extension's
command, document language id and `initializationOptions`, and runs
initialize → didOpen → diagnostics → completion → hover → shutdown over
`vscode-languageserver-protocol`.
