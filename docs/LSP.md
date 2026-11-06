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

## Starting it from an editor

Any LSP client can run the command `sitefile lsp --stdio` (the Rust binary on
`PATH`, or its full path) for files named `Sitefile` or `*.sitefile`. For
example, in Neovim:

```lua
vim.lsp.start({ name = 'sitefile', cmd = { 'sitefile', 'lsp', '--stdio' }, root_dir = vim.fn.getcwd() })
```

and in Helix (`languages.toml`):

```toml
[language-server.sitefile]
command = "sitefile"
args = ["lsp", "--stdio"]
```

VS Code and Zed extensions are tracked separately (issue #63).

## Features

| Capability | What it does |
| --- | --- |
| Diagnostics (`textDocument/publishDiagnostics`) | On open, change and save; cleared on close. Runs the real pipeline: **scan → parse → validate → plan**, plus the template parser on `RENDER` heredoc bodies. Every logical line that fails to parse gets its own error; after parsing, the first validation or planning error is reported (as `sitefile validate` would). Planner warnings (`unnamed stage is not the last one`, `INTEGRITY on a local import …`) are warnings on their whole line. Each diagnostic carries the stable error code (`E_SYNTAX`, `E_UNDEFINED`, …). |
| `ARG` without a default | An **Information** diagnostic (`E_MISSING_ARG`): a build needs `--build-arg NAME=<value>`. The rest of the file is planned with a placeholder value for it (`placeholder`, `0` or `false` by type), and an error found that way says so. |
| Completion | Instruction keywords at the start of a line; clause keywords and values in context (`IMPORT … AS` → `file text json yaml csv markdown wbn tree`, `EXPORT … AS` → `directory zip tar tgz wbn oci`, `COMPRESSION` → `gzip none`, `--escape=` → `none html`, `ARG NAME:` → types, `SORT BY key` → `ASC DESC`, `TO`, `FOR`, `IN`, `WITH`, `OPTIONS`, `ARGS`); the flags of `COPY`, `RENDER` and `TRANSFORM`; stage names defined earlier after `FROM`, `STAGE … FROM` and `--from=`; imports as `name:` sources and after `FOR … IN`; plugins and tools after `WITH`; and names inside `${`: ARG/LET names and the loop variable in operands, plus imports and block tags (`#each`, `#if`, `else`, `/each`, `/if`) in `RENDER` heredoc bodies. Keyword lists come from the parser's own tables (`rust/src/language/parser.rs`, `ast.rs`). |
| Hover | Short docs for every instruction, clause, flag and value word; for a name, its kind, the line that defines it, and (when the file plans) its value: an ARG/LET value, or an import's kind and URI. |
| Go to definition, find references | Stage names (`FROM`/`STAGE` bases, `--from=`), ARG/LET/IMPORT/PLUGIN/TOOL names (in `${…}`, `name:` sources, `FOR … IN`, `WITH`), and loop variables (`FOR item IN …`, `${#each list as item}`). |
| Rename | The same names (with `prepareRename`); the new name must be an identifier. |
| Document symbols | Stages as top-level symbols (named stages by name, an unnamed one by its `FROM` line, the implicit `FROM scratch` stage too), with their instructions as children; ARGs before the first stage are top-level. |

Server capabilities: `positionEncoding: utf-16`, full text sync
(`textDocumentSync.change = 1`, save with text; range edits are applied too
if a client sends them), `completionProvider` (trigger characters
`$ { - = space :`), `hoverProvider`, `definitionProvider`,
`referencesProvider`, `documentSymbolProvider`, `renameProvider` with
`prepareProvider`.

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
  build. Template *syntax* in heredoc bodies is checked.
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

## Limitations

- After the first validation or planning error, later ones are not reported
  until it is fixed (the validator and planner stop at the first error).
- No workspace-wide analysis: each document is analyzed on its own (there are
  no cross-file references in the language).
- No formatting, code actions, semantic tokens or signature help.
- Keys inside values (`${post.title}`) are not completed: they depend on the
  imported data.
- Only the stdio transport.
