# Sitefile for Zed

A Zed extension for [Sitefile](https://github.com/johnhenry/sitefile): syntax
highlighting from the tree-sitter grammar in
[`editors/tree-sitter-sitefile`](../tree-sitter-sitefile/), heredoc bodies in
the language their delimiter names (`<<HTML`, `<<CSS`, `<<JS`, `<<MD`,
`<<JSON`, `<<YAML`, `<<SVG`/`<<XML`), `${…}` template tags inside `RENDER`
bodies, an outline of stages, imports, plugins and tools, bracket pairs and
`#` comments.

Files named `Sitefile`, `*.Sitefile` and `*.sitefile` open as Sitefile. Zed's
`path_suffixes` take no globs, so for `Sitefile.prod` and friends add this to
your settings:

```json
{ "file_types": { "Sitefile": ["Sitefile.*"] } }
```

## Install Dev Extension

The extension is not in the Zed extension registry.

1. Clone this repository.
2. In Zed, run **zed: install dev extension** from the command palette (or
   **Extensions → Install Dev Extension**) and pick the `editors/zed`
   directory.
3. Open a `Sitefile`.

Zed downloads the grammar from the `repository` and `rev` in
`extension.toml` and compiles it to WebAssembly itself (it needs no local
toolchain for that).

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

- `extension.toml`: the extension and its grammar (`repository`, `rev`,
  `path`). `rev` is a commit on `main` that contains the grammar; update it
  when the grammar changes.
- `languages/sitefile/config.toml`: file matching, comments, brackets.
- `languages/sitefile/*.scm`: generated from
  `editors/tree-sitter-sitefile/queries` by `npm run sync-queries` there
  (captures renamed to Zed's names). Don't edit them here.
