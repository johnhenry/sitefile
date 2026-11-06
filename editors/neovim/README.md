# Sitefile for Neovim

Tree-sitter highlighting, injections (heredoc bodies by delimiter), folds and
locals for [Sitefile](https://github.com/johnhenry/sitefile), using the grammar
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

## Differences from the canonical queries

None: the canonical queries use nvim-treesitter capture names. The one custom
capture, `@keyword.unsupported` (for `RUN`, `EXEC`, …), falls back to
`@keyword` unless you define it. `brackets.scm` and `outline.scm` are Zed
formats and are not copied here.
