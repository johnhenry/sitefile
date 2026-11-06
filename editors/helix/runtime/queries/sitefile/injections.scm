; Generated from editors/tree-sitter-sitefile/queries/injections.scm by
; editors/tree-sitter-sitefile/scripts/sync-queries.mjs. Do not edit.

; Heredoc bodies are injected by delimiter (matched as written or in lower case).
; RENDER bodies are templates: the ${…} tags are child nodes of heredoc_body,
; and an injection excludes child nodes by default, so the embedded language
; sees only the text around the tags while the tags keep their own highlights.

((_
  source: (heredoc_start delimiter: (heredoc_delimiter) @_delimiter)
  body: (heredoc_body) @injection.content)
  (#any-of? @_delimiter "HTML" "html" "HTM" "htm")
  (#set! injection.language "html"))

((_
  source: (heredoc_start delimiter: (heredoc_delimiter) @_delimiter)
  body: (heredoc_body) @injection.content)
  (#any-of? @_delimiter "CSS" "css")
  (#set! injection.language "css"))

((_
  source: (heredoc_start delimiter: (heredoc_delimiter) @_delimiter)
  body: (heredoc_body) @injection.content)
  (#any-of? @_delimiter "JAVASCRIPT" "javascript" "JS" "js" "MJS" "mjs")
  (#set! injection.language "javascript"))

((_
  source: (heredoc_start delimiter: (heredoc_delimiter) @_delimiter)
  body: (heredoc_body) @injection.content)
  (#any-of? @_delimiter "MARKDOWN" "markdown" "MD" "md")
  (#set! injection.language "markdown"))

((_
  source: (heredoc_start delimiter: (heredoc_delimiter) @_delimiter)
  body: (heredoc_body) @injection.content)
  (#any-of? @_delimiter "JSON" "json")
  (#set! injection.language "json"))

((_
  source: (heredoc_start delimiter: (heredoc_delimiter) @_delimiter)
  body: (heredoc_body) @injection.content)
  (#any-of? @_delimiter "YAML" "yaml" "YML" "yml")
  (#set! injection.language "yaml"))

((_
  source: (heredoc_start delimiter: (heredoc_delimiter) @_delimiter)
  body: (heredoc_body) @injection.content)
  (#any-of? @_delimiter "SVG" "svg" "XML" "xml")
  (#set! injection.language "xml"))
