# Sitefile for Helix

Tree-sitter highlighting, injections (heredoc bodies by delimiter) and locals
for [Sitefile](https://github.com/johnhenry/sitefile), using the grammar in
[`editors/tree-sitter-sitefile`](../tree-sitter-sitefile/), plus the language
server `sitefile lsp` ([docs/LSP.md](../../docs/LSP.md)).

## Setup

1. Append [`languages.toml`](languages.toml) to
   `~/.config/helix/languages.toml`.
2. Copy (or symlink) the queries into your runtime directory:

   ```bash
   mkdir -p ~/.config/helix/runtime/queries
   ln -s "$PWD/editors/helix/runtime/queries/sitefile" ~/.config/helix/runtime/queries/sitefile
   ```

3. Fetch and build the grammar, then check it:

   ```bash
   hx --grammar fetch && hx --grammar build
   hx --health sitefile
   ```

## Language server

[`languages.toml`](languages.toml) also registers the server and attaches it
to Sitefiles:

```toml
[language-server.sitefile-lsp]
command = "sitefile"
args = ["lsp"]

[[language]]
name = "sitefile"
language-servers = ["sitefile-lsp"]
```

It needs the `sitefile` Rust binary on `PATH` (`cargo install --path rust`
from a clone), or its full path in `command`. `hx --health sitefile` shows
whether Helix finds it.

## Local grammar

For a local clone, use `source = { path = "/path/to/sitefile/editors/tree-sitter-sitefile" }`
in the `[[grammar]]` entry instead of `git`/`rev`/`subpath`.

## Differences from the canonical queries

`runtime/queries/sitefile/*.scm` are generated from
`editors/tree-sitter-sitefile/queries` by `npm run sync-queries` there; don't
edit them here. Capture names are rewritten to Helix scopes, e.g.
`@number` → `@constant.numeric`, `@boolean` → `@constant.builtin.boolean`,
`@string.escape` → `@constant.character.escape`, `@module` → `@namespace`,
`@property` / `@variable.member` → `@variable.other.member`,
`@keyword.repeat` → `@keyword.control.repeat` (the full table is in
`scripts/sync-queries.mjs`). Helix has no folds query, and `brackets.scm` and
`outline.scm` are Zed formats, so they are not copied.
