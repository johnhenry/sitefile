# tree-sitter-sitefile

A [tree-sitter](https://tree-sitter.github.io/) grammar for
[Sitefile](../../docs/LANGUAGE.md), with highlight, injection, fold, bracket,
outline and locals queries. It is used by the [Zed](../zed/),
[Neovim](../neovim/) and [Helix](../helix/) setups in this repository.

The generated parser (`src/parser.c`, `src/grammar.json`,
`src/node-types.json`, `src/tree_sitter/*.h`) is committed, as is usual for
tree-sitter grammars: editors such as Zed compile it straight from the
repository. It is generated for ABI 14 (`tree-sitter generate --abi 14`) so
older runtimes can load it too.

## Development

```bash
cd editors/tree-sitter-sitefile
npm install            # tree-sitter-cli (a devDependency of this directory only)
npm run generate       # after editing grammar.js or keywords.js
npm run sync-queries   # after editing queries/*.scm
npm test
```

`npm test` runs:

- `tree-sitter test`: the corpus in `test/corpus/` (every instruction,
  heredocs in both forms, nested-looking delimiters, unterminated bodies,
  comments, continuations, error recovery) and the highlight assertions in
  `test/highlight/`;
- `test/parse-files.test.mjs`: every `Sitefile` under `examples/` and
  `conformance/cases/`, the fixtures here and the VS Code extension's, parse
  without `ERROR` or `MISSING` nodes, except the files the real parser rejects
  with `E_SYNTAX` (and one inline template with an invalid tag, `E_TEMPLATE`).
  After `npm run build` in the repository root, those lists are checked
  against the TypeScript parser itself;
- `test/keywords.test.mjs`: every instruction, clause, value word, flag and
  heredoc instruction the TypeScript and Rust parsers accept is in
  `keywords.js`, the generated grammar and `queries/highlights.scm`;
- `test/queries.test.mjs`: every query compiles and captures what it should,
  and no node gets two highlight captures (so it does not matter whether an
  editor lets the first or the last pattern win);
- `test/sync.test.mjs`: the editor copies of the queries are up to date and
  compile; the Zed copy uses only Zed's capture names;
- `test/crlf.test.mjs`: CRLF line endings.

`tree-sitter build --wasm` also works (the CLI downloads its own wasi-sdk; no
emscripten or Docker needed).

## Tree

```
source_file
  comment
  from_instruction        base: (scratch | operand)  alias: identifier
  stage_instruction       name: identifier  base: (scratch | operand)
  arg_instruction         name  type: type  value
  let_instruction         name  type  value
  import_instruction      name  source  kind: import_kind  integrity  sort: sort_clause (key, order)
  copy_instruction        flag*  source  target  [body: heredoc_body  end: heredoc_end]
  render_instruction      flag*  source  target  loop: for_clause (item, list)  [body  end]
  transform_instruction   flag*  source?  plugin  target  options: options_clause (option key value)*
                          loop: for_clause  args: args_clause (argument*)  [body  end]
  plugin_instruction      name  source  integrity
  tool_instruction        name
  export_instruction      path  format: export_format  base  root  compression
  unsupported_instruction keyword  (RUN, REMOVE, GENERATE, ASSERT, META, EXEC)
```

- Operands are `string` (with `string_content`, `escape_sequence`,
  `substitution`), `raw_string`, `path`, `url`, `word`, and in a source
  position `resource_ref` (`import: identifier`, `path`). `substitution` is
  `${name.key…}` (`name: identifier`, `key: key`). ARG/LET and OPTIONS values
  can also be `number` or `boolean`; INTEGRITY takes an `integrity` digest;
  ARGS takes `placeholder`s (`{input}`, `{output}`, `{option.key}`).
- `flag` is `--name[=value]` (`name: flag_name`, `value: flag_value`).
- A heredoc is `source: (heredoc_start delimiter: (heredoc_delimiter))` on the
  instruction line, then `body: (heredoc_body)` and `end: (heredoc_end)` after
  the header line. `<<-` strips leading tabs (only) from the closing line, as
  the parser does.
- `line_continuation` (`\` at the end of a line, plus any blank lines) and
  `comment` (a line whose first non-blank character is `#`, also inside a
  continuation) can appear anywhere.
- Keywords are anonymous nodes named in upper case (`"FROM"`, `"AS"`, …)
  whatever the case in the source. Value words (`json`, `tar.gz`, `gzip`, …)
  are plain nodes (`import_kind`, `export_format`, `compression`, `type`,
  `flag_value`); the queries highlight the known ones, like the parser checks
  them after reading the word.

### RENDER templates

`RENDER` bodies are templates, so their `heredoc_body` has children:
`template_substitution` (`name`, `key`, `filter`), `template_each` (`list:
reference`, `item`), `template_if` (`condition: reference`, `negate`),
`template_else`, `template_end` and `template_escape` (`$${`). `COPY` and
`TRANSFORM` bodies are written byte for byte and have no children.

The template tags are parsed in place rather than by injecting a second
"template" grammar. That keeps one grammar to ship (Zed, Neovim and Helix
would each need both), and it keeps the delimiter: `queries/injections.scm`
injects the body as HTML, CSS, JavaScript, Markdown, JSON, YAML or XML by its
delimiter, and an injection leaves out the content node's children by
default, so the embedded language sees the text around the tags while
`${…}` keeps its own highlighting, even inside an attribute such as
`href="${url}"`. An invalid tag (`${a + b}`) is an `ERROR` inside the body; the
closing line still ends the heredoc.

### External scanner

`src/scanner.c` handles what depends on context: instruction-ending newlines
(versus blank lines between instructions), continuations, comments (only at
the start of a line), heredoc markers, bodies and closing lines (the delimiter
is kept in the serialized state), template text up to `${` / `$${`, and the
`name` of a `name:` import reference.

## Highlight capture names

`queries/highlights.scm` uses nvim-treesitter capture names.
`scripts/sync-queries.mjs` writes the editor copies, renaming captures for Zed
and Helix (see the tables at the top of the script). Edit the files here, then
run `npm run sync-queries`.
