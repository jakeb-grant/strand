; Highlights for .strand (nvim-treesitter's capture names). Keywords are
; contextual (docs/grammar.md): a word is captured as a keyword only where
; the grammar took it as one. Specific patterns come before general ones,
; since tree-sitter-highlight (the CLI, helix) takes a node's first match;
; the plain `(identifier)` and `(token)` captures are last.

(comment) @comment @spell

; ---- Literals ---------------------------------------------------------------

(string) @string
(escape_sequence) @string.escape
(number) @number
(color) @constant
(boolean) @boolean
(null) @constant.builtin

; ---- Names ------------------------------------------------------------------

(token_key (token) @property)
(token_key (identifier) @property)
(token_key (integer) @property)

(attribute_name) @attribute

(selector name: (selector_name) @tag)

(component name: (identifier) @type)
(surface name: (identifier) @type)
(enum name: (identifier) @type)
(variant) @constant
(type name: (identifier) @type)
(type_name (identifier) @type)
(tokens name: (identifier) @type)
(tokens extends: (identifier) @type)
(keyframes name: (identifier) @function)
(service name: (identifier) @module)
(function name: (identifier) @function)
(action name: (identifier) @function)
(action method: (identifier) @function.method)
(permit kind: (identifier) @keyword)

(element name: (identifier) @tag)
(element file: (identifier) @module)
(prop name: (identifier) @property)
(field name: (identifier) @property)
(named_argument name: (identifier) @variable.parameter)
(parameter name: (identifier) @variable.parameter)
(lambda_parameter) @variable.parameter
(lambda_parameter name: (identifier) @variable.parameter)
(event_name (identifier) @function.method)

(call_expression function: (identifier) @function.call)
(call_expression
  function: (member_expression field: (field_name) @function.method.call))
(member_expression field: (field_name) @property)

(state name: (identifier) @variable)
(let name: (identifier) @variable)

; ---- Keywords ---------------------------------------------------------------

[
  "component"
  "bar"
  "panel"
  "osd"
  "lock"
  "state"
  "let"
  "export"
  "enum"
  "type"
  "tokens"
  "extends"
  "override"
  "use"
  "palette"
  "service"
  "from"
  "dbus"
  "system"
  "session"
  "file"
  "listen"
  "poll"
  "permit"
  "action"
  "keyframes"
  "rw"
  "persist"
  "key"
  "when"
  "enter"
  "exit"
  "set"
  "play"
  "on"
  "change"
  "after"
  "every"
  "while"
  (slot)
] @keyword

"fn" @keyword.function

[
  "if"
  "else"
  "match"
] @keyword.conditional

[
  "for"
  "in"
] @keyword.repeat

"await" @keyword.coroutine

; ---- Operators and punctuation --------------------------------------------

; A ternary's `?` and `:` before the operators and delimiters that would
; take them first.
(ternary_expression "?" @keyword.conditional.ternary)
(ternary_expression ":" @keyword.conditional.ternary)

[
  "=" "+=" "-=" "*=" "/="
  "==" "!=" "<" "<=" ">" ">="
  "&&" "||" "!" "??" "?" "?."
  "+" "-" "*" "/" "%"
  "=>" "->" "<->" "~"
] @operator

[ "(" ")" "[" "]" "{" "}" ] @punctuation.bracket
[ "," ";" "." ":" ] @punctuation.delimiter

; ---- Everything else --------------------------------------------------------

(token) @variable.builtin
(identifier) @variable
