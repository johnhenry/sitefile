# Sitefile for VS Code

Syntax highlighting for [Sitefile](https://github.com/johnhenry/sitefile), the
Docker-inspired static site builder. Declarative only: a TextMate grammar and a
language configuration, no extension code.

Files named `Sitefile`, `*.sitefile`, `Sitefile.*` and `*.Sitefile` open as
Sitefile.

## What it highlights

The grammar follows the real scanner and parser (`src/language/scanner.ts`,
`parser.ts`) and the template syntax in `src/render.ts`:

- **Instructions**, case-insensitive and optionally indented: `FROM`, `STAGE`,
  `ARG`, `LET`, `IMPORT`, `COPY`, `RENDER`, `TRANSFORM`, `PLUGIN`, `TOOL`,
  `EXPORT`. `RUN`, `REMOVE`, `GENERATE`, `ASSERT`, `META` and `EXEC`, which the
  parser rejects, are marked invalid.
- **Clause keywords**: `AS`, `FROM`, `TO`, `FOR … IN`, `WITH`, `OPTIONS`,
  `ARGS`, `INTEGRITY`, `SORT BY … ASC|DESC`, `BASE`, `AT`, `COMPRESSION`.
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

## Install locally

The extension is not on the Marketplace. Build the package and install it:

```bash
cd editors/vscode
npm install
npm run package          # writes sitefile-0.1.0.vsix
code --install-extension sitefile-0.1.0.vsix
```

(From the repository root: `code --install-extension editors/vscode/sitefile-0.1.0.vsix`.)

## Without the extension

Dockerfile highlighting is a reasonable stopgap. In your VS Code settings:

```json
"files.associations": {
  "Sitefile": "dockerfile",
  "*.sitefile": "dockerfile"
}
```

## Development

The grammar is generated: edit `scripts/grammar.mjs`, then

```bash
npm run build:grammar    # writes syntaxes/*.json
npm test                 # grammar tests (vscode-textmate + vscode-oniguruma)
UPDATE_SNAPSHOTS=1 npm test   # accept intended token changes
npm run preview          # test/preview.html, rendered with shiki
```

The tests tokenize the Sitefiles in `test/fixtures/` and compare them with
`test/snapshots/*.snap` (line number → text and `[token, scopes]`). They also
fail when the parser accepts an instruction, clause keyword, value word or flag
the grammar doesn't know, when the committed JSON is out of date, or (after a
root `npm run build`) when a valid fixture no longer parses. Embedded grammars
are stubs in the tests; the preview uses the real ones.
