# sitefile for Zed

A Zed extension for [sitefile](https://github.com/johnhenry/sitefile): syntax
highlighting from the tree-sitter grammar in
[`editors/tree-sitter-sitefile`](../tree-sitter-sitefile/), heredoc bodies in
the language their delimiter names (`<<HTML`, `<<CSS`, `<<JS`, `<<MD`,
`<<JSON`, `<<YAML`, `<<SVG`/`<<XML`), `${…}` template tags inside `RENDER`
bodies, an outline of stages, imports, plugins and tools, bracket pairs and
`#` comments. Diagnostics, completion, hover, go to definition, references,
rename and document symbols come from the language server `sitefile lsp`
([docs/LSP.md](../../docs/LSP.md)), registered as `sitefile-lsp`.

Files named `Sitefile`, `*.Sitefile` and `*.sitefile` open as Sitefile. Zed's
`path_suffixes` take no globs, so for `Sitefile.prod` and friends add this to
your settings:

```json
{ "file_types": { "Sitefile": ["Sitefile.*"] } }
```

## Requirements

- Highlighting needs nothing.
- The language server needs the `sitefile` **Rust** binary (crate `rust/`,
  binary `sitefile`) on the `PATH` Zed sees for the worktree:

  ```bash
  cargo install --path rust        # from a clone; installs ~/.cargo/bin/sitefile
  ```

  or set its path in the settings (below). The TypeScript CLI has no language
  server. If the binary isn't found, Zed shows the extension's error in the
  language server log and highlighting keeps working.
- Installing the dev extension compiles `src/lib.rs` to WebAssembly, which
  needs Rust installed through `rustup` (Zed adds the `wasm32-wasip2` target
  itself).

## Settings

By default the extension runs `sitefile lsp`, with `sitefile` looked up on the
worktree's `PATH`. To use another binary or arguments, in `settings.json`:

```json
{
  "lsp": {
    "sitefile-lsp": {
      "binary": {
        "path": "/path/to/sitefile/rust/target/release/sitefile",
        "arguments": ["lsp"]
      }
    }
  }
}
```

`binary.env` adds environment variables. To turn the server off for
Sitefiles:

```json
{ "languages": { "Sitefile": { "language_servers": ["!sitefile-lsp", "..."] } } }
```

## Install Dev Extension

The extension is not in the Zed extension registry.

1. Clone this repository.
2. In Zed, run **zed: install dev extension** from the command palette (or
   **Extensions → Install Dev Extension**) and pick the `editors/zed`
   directory.
3. Open a `Sitefile`.

Zed downloads the grammar from the `repository` and `rev` in
`extension.toml` and compiles it to WebAssembly itself, and builds the
extension crate (`Cargo.toml`, `src/lib.rs`) with your `rustup` toolchain.
To check the crate without Zed: `cargo check` here (it is its own Cargo
workspace, not part of `rust/`; `target/` is ignored).

### Using a local grammar

To try grammar changes before they are pushed, point the grammar at your
clone. In `extension.toml`:

```toml
[grammars.sitefile]
repository = "file:///absolute/path/to/sitefile"
rev = "<output of: git rev-parse HEAD>"
path = "editors/tree-sitter-sitefile"
```

The `rev` must be a committed revision (Zed checks it out), so commit the
grammar first. Then run **zed: rebuild dev extension** (or reinstall it).
Don't commit the `file://` URL.

## Files

- `extension.toml`: the extension, its grammar (`repository`, `rev`,
  `path`) and the `sitefile-lsp` language server. `rev` is a commit that
  contains the grammar; update it when the grammar changes.
- `Cargo.toml`, `src/lib.rs`: the extension code (`zed_extension_api`), which
  only builds the command that starts the language server.
- `languages/sitefile/config.toml`: file matching, comments, brackets.
- `languages/sitefile/*.scm`: generated from
  `editors/tree-sitter-sitefile/queries` by `npm run sync-queries` there
  (captures renamed to Zed's names). Don't edit them here.
