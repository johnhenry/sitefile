; Generated from editors/tree-sitter-sitefile/queries/locals.scm by
; editors/tree-sitter-sitefile/scripts/sync-queries.mjs. Do not edit.

; ARG, LET, IMPORT, PLUGIN, TOOL and stage names share one global namespace.
(source_file) @local.scope

(arg_instruction name: (identifier) @local.definition.var)
(let_instruction name: (identifier) @local.definition.var)
(import_instruction name: (identifier) @local.definition.import)
(plugin_instruction name: (identifier) @local.definition.function)
(tool_instruction name: (identifier) @local.definition.function)
(stage_instruction name: (identifier) @local.definition.type)
(from_instruction alias: (identifier) @local.definition.type)

(substitution name: (identifier) @local.reference)
(template_substitution name: (identifier) @local.reference)
(reference name: (identifier) @local.reference)
(for_clause list: (identifier) @local.reference)
(feed_instruction list: (identifier) @local.reference)
(resource_ref import: (identifier) @local.reference)
