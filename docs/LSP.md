# Language server (`sitefile lsp`)

The Rust binary includes a language server for Sitefiles:
[LSP 3.17](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)
over stdin/stdout (JSON-RPC 2.0 with `Content-Length` framing).

```bash
(cd rust && cargo build --release)
rust/target/release/sitefile lsp          # editors usually pass --stdio, which is accepted
```

The server exists only in the Rust implementation. This is a deliberate
**tooling exception** (see [RUST.md](RUST.md#tooling-exception-the-language-server)):
the Sitefile language is identical in both implementations, and the server is
an editor tool built on the Rust parser. The TypeScript CLI answers
`sitefile lsp` with a usage error that points here.

## Editor setup

The server is the `sitefile` **Rust** binary; put it on `PATH` with
`cargo install --path rust` (from a clone) or give editors its full path.
Every client runs `sitefile lsp` (`--stdio` is accepted too) for files named
`Sitefile`, `*.sitefile` or `*.Sitefile`.

| Editor | Setup |
| --- | --- |
| VS Code | [editors/vscode](../editors/vscode/README.md): the extension starts the server (`sitefile.server.path`, `sitefile.server.args`, `sitefile.trace.server`; command **Sitefile: Restart Language Server**). |
| Zed | [editors/zed](../editors/zed/README.md): the extension registers `sitefile-lsp` and finds `sitefile` on `PATH` (or `lsp.sitefile-lsp.binary.path`). |
| Neovim | [editors/neovim](../editors/neovim/README.md#language-server): `vim.lsp.config('sitefile_lsp', { cmd = { 'sitefile', 'lsp' }, filetypes = { 'sitefile' } })`, or an nvim-lspconfig custom server. |
| Helix | [editors/helix](../editors/helix/README.md#language-server): `[language-server.sitefile-lsp]` with `command = "sitefile"`, `args = ["lsp"]`, and `language-servers = ["sitefile-lsp"]` on the language. |

Any other LSP client: command `sitefile lsp`, stdio transport, language id
`sitefile`. The server needs no `initializationOptions` and ignores them.

## Features

| Capability | What it does |
| --- | --- |
| Diagnostics (`textDocument/publishDiagnostics`) | On open, change and save; cleared on close. Runs the real pipeline: **scan → parse → validate → plan**, plus the template parser on `RENDER` heredoc bodies. Every logical line that fails to parse gets its own error; after parsing, the first validation or planning error is reported (as `sitefile validate` would). Planner warnings (`unnamed stage is not the last one`, `INTEGRITY on a local import …`) are warnings on their whole line. Each diagnostic carries the stable error code (`E_SYNTAX`, `E_UNDEFINED`, …). |
| Lint warnings | `validate`'s [lint rules](LANGUAGE.md#lint-rules-sitefile-validate) are Warning diagnostics on their instruction's first line, with the rule id as the diagnostic `code` (`render-escape`, …; `lint-comment` for a bad `# sitefile-lint:` comment) and the same text as `validate` prints. |
| Code actions (quick fixes) | `textDocument/codeAction` for the diagnostics in the requested range: `render-escape` → **Add --escape=html**; `unpinned-import` → **Add INTEGRITY from Sitefile.lock** and `unpinned-image` → **Pin the digest from Sitefile.lock** (only when the `Sitefile.lock` next to the document has the URL / the image tag); the unnamed-stage warning → **Name this stage** (`FROM … AS stageN`, or a `STAGE stageN` line before an implicit stage); every lint rule → **Disable <rule> for this instruction** (`# sitefile-lint: disable-next-line=…` above it) and **Disable <rule> in this file** (`# sitefile-lint: disable=…` on the first line). |
| `ARG` without a default | An **Information** diagnostic (`E_MISSING_ARG`): a build needs `--build-arg NAME=<value>`. The rest of the file is planned with a placeholder value for it (`placeholder`, `0` or `false` by type), and an error found that way says so. |
| Completion | Instruction keywords at the start of a line; clause keywords and values in context (`IMPORT … AS` → `file text json yaml csv markdown wbn tree`, `EXPORT … AS` → `directory zip tar tgz wbn oci`, `COMPRESSION` → `gzip none`, `--escape=` → `none html`, `ARG NAME:` → types, `SORT BY key` → `ASC DESC`, `TO`, `FOR`, `IN`, `WITH`, `OPTIONS`, `ARGS`; `SITEMAP`/`FEED` clauses in order (`BASE`, `LINK`, `TO`, `TITLE`), `REDIRECT … STATUS` → `301 302 303 307 308`, `MANIFEST` after a `FINGERPRINT` pattern, `--warn`/`--external` after `CHECKLINKS`); the flags of `COPY` (`--overwrite`, `--from=`, `--exclude=`, `--chmod=`, `--parents`), `RENDER`, `TRANSFORM` (`--overwrite`, `--from=`, `--include=`, `--exclude=`) and the site instructions; imports after `FEED`; stage names defined earlier after `FROM`, `STAGE … FROM` and `--from=`; imports as `name:` sources and after `FOR … IN`; plugins and tools after `WITH`; and names inside `${`: ARG/LET names and the loop variable in operands, plus imports and block tags (`#each`, `#if`, `else`, `/each`, `/if`) in `RENDER` heredoc bodies. Keyword lists come from the parser's own tables (`rust/src/language/parser.rs`, `ast.rs`). |
| Hover | Short docs for every instruction, clause, flag and value word, and for the `# sitefile=1` version directive; for a name, its kind, the line that defines it, and (when the file plans) its value: an ARG/LET value, or an import's kind and URI. |
| Go to definition, find references | Stage names (`FROM`/`STAGE` bases, `--from=`), ARG/LET/IMPORT/PLUGIN/TOOL names (in `${…}`, `name:` sources, `FOR … IN`, `FEED <import>`, `WITH`), and loop variables (`FOR item IN …`, `${#each list as item}`). |
| Rename | The same names (with `prepareRename`); the new name must be an identifier. |
| Document symbols | Stages as top-level symbols (named stages by name, an unnamed one by its `FROM` line, the implicit `FROM scratch` stage too), with their instructions as children; ARGs before the first stage are top-level. |

Server capabilities: `positionEncoding: utf-16`, full text sync
(`textDocumentSync.change = 1`, save with text; range edits are applied too
if a client sends them), `completionProvider` (trigger characters
`$ { - = space :`), `hoverProvider`, `definitionProvider`,
`referencesProvider`, `documentSymbolProvider`, `renameProvider` with
`prepareProvider`, `codeActionProvider` (kind `quickfix`).

Protocol: `initialize`, `initialized`, `shutdown` and `exit` (exit code 0
after `shutdown`, 1 otherwise or at end of input). Requests before
`initialize` get `ServerNotInitialized`, requests after `shutdown`
`InvalidRequest`, unknown requests `MethodNotFound`; unknown notifications
and `$/cancelRequest` are ignored (requests are answered synchronously).

## How it works

- **No IO.** Analysis never reads the build context, fetches, runs plugins or
  tools, or writes anything: planning binds variables and normalizes paths
  only. So errors that need the files (`E_NOT_FOUND`, a bad JSON import,
  `E_CONFLICT`, an undefined name used in a template) appear only when you
  build. Template *syntax* in heredoc bodies is checked. The one exception is
  a code action: the pin quick fixes read the `Sitefile.lock` next to the
  document (never the network) when you ask for actions.
- **Single-threaded.** One loop reads a message, handles it and replies.
  Diagnostics are published when no further input is already buffered, so a
  burst of edits is analyzed once, for the last version.
- **Positions** are UTF-16 code units, which is also how Sitefile error spans
  count columns, so `file:line:col` maps directly to an LSP range (the range
  covers the word at that position).
- **Editor features** (completion, hover, navigation, symbols) use a small
  tolerant index of the document (`rust/src/lsp/index.rs`) that follows the
  scanner's line rules (comments, `\` continuations, heredoc bodies) and keeps
  working while the file does not parse. Diagnostics always come from the
  real parser.

## Agreement with `sitefile validate`

Diagnostics come from the same parser, validator and planner as
`sitefile validate`, and tests hold them to it: for every Sitefile under
`conformance/cases` and `examples`, and for the parser fuzz inputs
(`rust/tests/fixtures/sitefile`), the LSP's first error has the code, line,
column and message that `validate` prints (Rust and TypeScript CLIs alike),
and its warnings are `validate`'s warnings (`rust/tests/lsp_parity.rs`,
`language::differential`). The differences are deliberate:

- **Several syntax errors.** `validate` stops at the first error; the LSP
  reports one per logical line that fails to parse. The first is the same.
  A scanner error (an unclosed heredoc, two `<<` bodies, a bad or misplaced
  `# sitefile=` directive) is the only error, as in `validate`.
- **ARGs without a default.** `validate` fails with `E_MISSING_ARG` unless
  `--build-arg` gives a value. The LSP reports an Information diagnostic
  instead and checks the rest with a placeholder (`placeholder`, `0`,
  `false`), which matches `validate --build-arg NAME=placeholder`; an error
  found that way ends with `(checked with placeholder values for NAME)`.
- **Template syntax.** The LSP also parses `RENDER` heredoc bodies as
  templates (`E_TEMPLATE`); `validate` does not (a build does).
- **Ranges.** An error's range covers the word at its column; an error at
  the end of a line (`expected TO`) covers the line's last character. An
  error without a position covers the first line.
- **Warnings** (planner and lint) are the same text, on the whole line they name. A
  `--build-arg … was ignored` warning cannot occur (the LSP takes no build
  arguments).

## Limitations

- After the first validation or planning error, later ones are not reported
  until it is fixed (the validator and planner stop at the first error).
- No workspace-wide analysis: each document is analyzed on its own (there are
  no cross-file references in the language).
- No formatting, semantic tokens or signature help. Code actions are quick fixes only.
- Keys inside values (`${post.title}`) are not completed: they depend on the
  imported data.
- Only the stdio transport.
