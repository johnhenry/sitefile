; Outline (Zed's outline.scm format): stages, imports and plugins.
(stage_instruction
  "STAGE" @context
  name: (_) @name) @item

(from_instruction
  "FROM" @context
  base: (_) @name
  ("AS" @context
    alias: (_) @name)?) @item

(import_instruction
  "IMPORT" @context
  name: (_) @name) @item

(plugin_instruction
  "PLUGIN" @context
  name: (_) @name) @item

(tool_instruction
  "TOOL" @context
  name: (_) @name) @item
