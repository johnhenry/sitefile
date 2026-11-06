; Sitefile highlights. Capture names follow the nvim-treesitter conventions;
; scripts/sync-queries.mjs rewrites them for Zed and Helix.
; No two patterns capture the same node, so the result does not depend on
; whether an editor lets the first or the last matching pattern win.

(comment) @comment

; `# sitefile=1`: the syntax version directive.
(directive) @keyword.directive

(line_continuation) @punctuation.special

; Instructions and clauses (case-insensitive in the source).
[
  "FROM" "STAGE" "ARG" "LET" "IMPORT" "COPY" "RENDER" "TRANSFORM" "PLUGIN" "TOOL" "EXPORT"
  "SITEMAP" "FEED" "REDIRECT" "HEADER"
  "AS" "TO" "INTEGRITY" "SORT" "BY" "ASC" "DESC" "WITH" "OPTIONS" "ARGS" "BASE" "AT" "COMPRESSION"
  "LINK" "TITLE" "STATUS" "IMAGE" "SECRET" "BEARER"
] @keyword

(secret_clause id: (identifier) @variable)
(secret_clause header: (header_name) @property)

(for_clause [ "FOR" "IN" ] @keyword.repeat)

; RUN, REMOVE, GENERATE, ASSERT, META, EXEC are rejected by the parser.
(unsupported_instruction keyword: _ @keyword.unsupported)

; Stages
(scratch) @constant.builtin
(from_instruction alias: (identifier) @label)
(stage_instruction name: (identifier) @label)
; A bare identifier base names an earlier stage.
((from_instruction base: (word) @label)
  (#match? @label "^[A-Za-z_][A-Za-z0-9_]*$"))
((stage_instruction base: (word) @label)
  (#match? @label "^[A-Za-z_][A-Za-z0-9_]*$"))

; Names
(arg_instruction name: (identifier) @variable)
(let_instruction name: (identifier) @variable)
(import_instruction name: (identifier) @module)
(resource_ref import: (identifier) @module)
(plugin_instruction name: (identifier) @function)
(tool_instruction name: (identifier) @function)
(transform_instruction plugin: (identifier) @function.call)
(for_clause item: (identifier) @variable.parameter)
(for_clause list: (identifier) @variable)
(sort_clause key: (identifier) @property)
(option key: (identifier) @property)
(placeholder) @variable.builtin
(feed_instruction list: (identifier) @module)
(header_instruction name: (word) @property)
((redirect_instruction status: (status) @number)
  (#any-of? @number "301" "302" "303" "307" "308"))

; Value words: known values are highlighted, anything else is left plain
; (the parser rejects it). Matched in lower and upper case.
((type) @type.builtin
  (#any-of? @type.builtin "string" "number" "boolean" "STRING" "NUMBER" "BOOLEAN"))
; `LET name: html`: an html fragment (a keyword token, any case).
(let_instruction type: (type) @type.builtin (#match? @type.builtin "^[Hh][Tt][Mm][Ll]$"))
((import_kind) @type.builtin
  (#any-of? @type.builtin
    "file" "text" "json" "tree" "markdown" "yaml" "csv" "wbn" "html"
    "FILE" "TEXT" "JSON" "TREE" "MARKDOWN" "YAML" "CSV" "WBN" "HTML"))
((export_format) @type.builtin
  (#any-of? @type.builtin
    "directory" "zip" "tar" "tgz" "tar.gz" "wbn" "oci"
    "DIRECTORY" "ZIP" "TAR" "TGZ" "TAR.GZ" "WBN" "OCI"))
((compression) @constant.builtin
  (#any-of? @constant.builtin "gzip" "none" "GZIP" "NONE"))

; Flags
(flag "--" @attribute)
(flag name: (flag_name) @attribute)
((flag name: (flag_name) @_name value: (flag_value) @label)
  (#eq? @_name "from"))
((flag name: (flag_name) @_name value: (flag_value) @number)
  (#eq? @_name "chmod")
  (#any-of? @number "644" "0644" "755" "0755"))
((flag name: (flag_name) @_name value: (flag_value) @string.special)
  (#any-of? @_name "exclude" "include"))
((flag name: (flag_name) @_name value: (flag_value) @constant.builtin)
  (#eq? @_name "escape")
  (#any-of? @constant.builtin "none" "html" "NONE" "HTML"))

; Operands
(string) @string
(raw_string) @string
(escape_sequence) @string.escape
(path) @string.special.path
(url) @string.special.url
(integrity) @string.special
(number) @number
(boolean) @boolean

; ${NAME} / ${name.key} in operands
(substitution name: (identifier) @variable)
(substitution key: (key) @variable.member)

; Heredocs
(heredoc_start [ "<<" "<<-" ] @operator)
(heredoc_delimiter) @label
(heredoc_end) @label
; Bodies in an embedded language are left to the injected grammar.
((_
  source: (heredoc_start delimiter: (heredoc_delimiter) @_delimiter)
  body: (heredoc_body) @string)
  (#not-any-of? @_delimiter
    "HTML" "html" "HTM" "htm" "CSS" "css" "JAVASCRIPT" "javascript" "JS" "js" "MJS" "mjs"
    "MARKDOWN" "markdown" "MD" "md" "JSON" "json" "YAML" "yaml" "YML" "yml" "SVG" "svg" "XML" "xml"))

; RENDER templates and html fragments
(template_substitution name: (identifier) @variable)
(template_substitution key: (key) @variable.member)
(template_substitution filter: (identifier) @function.builtin)
(reference name: (identifier) @variable)
(reference key: (key) @variable.member)
(template_each item: (identifier) @variable.parameter)
[ "#each" "/each" ] @keyword.repeat
[ "#if" "else" "/if" ] @keyword.conditional
"as" @keyword
(template_escape) @string.escape

; Punctuation and operators
[ "${" "}" ] @punctuation.special
[ "=" "|" "!" ] @operator
[ ":" "." ] @punctuation.delimiter
