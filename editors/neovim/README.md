# sitefile for Neovim

Tree-sitter highlighting, injections (heredoc bodies by delimiter), folds and
locals for [sitefile](https://github.com/johnhenry/sitefile), using the grammar
in [`editors/tree-sitter-sitefile`](../tree-sitter-sitefile/).
`queries/sitefile/*.scm` are generated from that directory's queries (the
capture names there are already nvim-treesitter's); don't edit them here.

## Setup

1. Put this directory on your `runtimepath` so Neovim finds
   `queries/sitefile/`, e.g. with lazy.nvim:

   ```lua
   { dir = '/path/to/sitefile/editors/neovim' }
   ```

   or `vim.opt.runtimepath:append('/path/to/sitefile/editors/neovim')`.

2. Detect the filetype:

   ```lua
   vim.filetype.add({
     filename = { Sitefile = 'sitefile' },
     extension = { sitefile = 'sitefile', Sitefile = 'sitefile' },
     pattern = { ['Sitefile%..*'] = 'sitefile' },
   })
   ```

3. Install the parser. With nvim-treesitter (`main` branch), register it and
   run `:TSInstall sitefile`:

   ```lua
   vim.api.nvim_create_autocmd('User', {
     pattern = 'TSUpdate',
     callback = function()
       require('nvim-treesitter.parsers').sitefile = {
         install_info = {
           url = 'https://github.com/johnhenry/sitefile',
           location = 'editors/tree-sitter-sitefile',
           branch = 'main',
           queries = 'editors/neovim/queries/sitefile', -- optional: copies the queries too
         },
       }
     end,
   })
   ```

   On the older `master` branch the equivalent is:

   ```lua
   require('nvim-treesitter.parsers').get_parser_configs().sitefile = {
     install_info = {
       url = 'https://github.com/johnhenry/sitefile',
       location = 'editors/tree-sitter-sitefile',
       files = { 'src/parser.c', 'src/scanner.c' },
       branch = 'main',
     },
     filetype = 'sitefile',
   }
   ```

   Without nvim-treesitter, compile the parser yourself and put it on the
   runtimepath as `parser/sitefile.so`:

   ```bash
   cd editors/tree-sitter-sitefile && npm install
   npx tree-sitter build -o ../neovim/parser/sitefile.so .
   ```

4. Start highlighting (and folds) for the filetype:

   ```lua
   vim.api.nvim_create_autocmd('FileType', {
     pattern = 'sitefile',
     callback = function()
       vim.treesitter.start()
       vim.wo.foldmethod = 'expr'
       vim.wo.foldexpr = 'v:lua.vim.treesitter.foldexpr()'
     end,
   })
   ```

Injected heredoc bodies need the parsers for those languages (`html`, `css`,
`javascript`, `markdown`, `json`, `yaml`, `xml`).

## Language server

Diagnostics, completion, hover, go to definition, references, rename and
document symbols come from `sitefile lsp` ([docs/LSP.md](../../docs/LSP.md)).
It needs the `sitefile` Rust binary on `PATH` (a
[prebuilt release binary](../../README.md#prebuilt-binaries), or
`cargo install --path rust` from a clone) or its full path in `cmd`. The filetype from step 2 must be set.

Neovim 0.11+ (`vim.lsp.config`):

```lua
vim.lsp.config('sitefile_lsp', {
  cmd = { 'sitefile', 'lsp' },
  filetypes = { 'sitefile' },
  root_markers = { 'Sitefile', '.git' },
})
vim.lsp.enable('sitefile_lsp')
```

With nvim-lspconfig on older Neovim, register a custom server:

```lua
local configs = require('lspconfig.configs')
if not configs.sitefile_lsp then
  configs.sitefile_lsp = {
    default_config = {
      cmd = { 'sitefile', 'lsp' },
      filetypes = { 'sitefile' },
      root_dir = require('lspconfig.util').root_pattern('Sitefile', '.git'),
      single_file_support = true,
    },
  }
end
require('lspconfig').sitefile_lsp.setup({})
```

`:checkhealth vim.lsp` (or `:LspInfo`) shows whether it attached.

## Differences from the canonical queries

None: the canonical queries use nvim-treesitter capture names. The one custom
capture, `@keyword.unsupported` (for `RUN`, `EXEC`, …), falls back to
`@keyword` unless you define it. `brackets.scm` and `outline.scm` are Zed
formats and are not copied here.
