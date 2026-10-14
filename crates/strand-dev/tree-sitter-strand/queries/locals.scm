; Scopes, definitions and references for .strand (nvim-treesitter's
; capture names). A file's top-level `state` and `let` are the file's; a
; component's parameters, `state` and `let` are its body's; `for`
; bindings, handler and lambda parameters and handler `let`s are their
; block's (docs/spec.md, 4.2 and 5).

[
  (source_file)
  (component)
  (surface)
  (function)
  (tree_block)
  (stmt_block)
  (for_tree)
  (for_statement)
  (on_handler)
  (lambda)
] @local.scope

(component name: (identifier) @local.definition.type)
(enum name: (identifier) @local.definition.type)
(type name: (identifier) @local.definition.type)
(function name: (identifier) @local.definition.function)
(action name: (identifier) @local.definition.function)
(keyframes name: (identifier) @local.definition.function)
(service name: (identifier) @local.definition.namespace)
(state name: (identifier) @local.definition.var)
(let name: (identifier) @local.definition.var)
(parameter name: (identifier) @local.definition.parameter)
(lambda_parameter name: (identifier) @local.definition.parameter)
(lambda parameters: (lambda_parameter) @local.definition.parameter)
(for_tree binding: (identifier) @local.definition.var)
(for_statement binding: (identifier) @local.definition.var)

(identifier) @local.reference
