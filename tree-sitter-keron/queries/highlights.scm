; Highlight queries for keron. Ordered specific-to-general: the
; tree-sitter highlighter (and Helix and Zed, which follow it) gives
; precedence to the first matching pattern.

(comment) @comment

; ── strings ──────────────────────────────────────────────────────────

(escape_sequence) @string.escape

(interpolation
  "${" @punctuation.special
  "}" @punctuation.special)

(string) @string

(multiline_string) @string

(raw_string) @string

; ── literals ─────────────────────────────────────────────────────────

(number) @number

(boolean) @boolean

(null) @constant.builtin

; ── functions ────────────────────────────────────────────────────────

(function_declaration
  name: (identifier) @function)

((call_expression
  function: (identifier) @function.builtin)
  (#match? @function.builtin "^(symlink|shell|template|keron_root|os_type|os_arch|env|secret|unwrap_secret|brew|cask|cargo|winget|hostname|user|home_dir|config_dir|cache_dir|data_dir|state_dir|runtime_dir|split|join|contains|replace|trim|starts_with|ends_with|len|first|last|keys|values|get|path_join|path_parent|path_basename|path_extension|path_is_absolute|path_exists|path_is_dir|path_is_file|read_file|sort|unique|index_of|merge|without|with|parse_int|parse_double|ssh_key|gpg_key)$"))

(call_expression
  function: (identifier) @function)

; ── types ────────────────────────────────────────────────────────────

(primitive_type) @type.builtin

(type_identifier) @type

"List" @type.builtin

"Map" @type.builtin

; ── variables, parameters, fields ────────────────────────────────────

(parameter
  name: (identifier) @variable.parameter)

(field_declaration
  name: (identifier) @property)

(field_identifier) @property

(identifier) @variable

; ── keywords ─────────────────────────────────────────────────────────

[
  "val"
  "fn"
  "struct"
  "type"
  "reconcile"
  "from"
  "use"
  "if"
  "else"
  "for"
  "in"
  "match"
] @keyword

; ── operators and punctuation ────────────────────────────────────────

(list_type
  "<" @punctuation.bracket
  ">" @punctuation.bracket)

(map_type
  "<" @punctuation.bracket
  ">" @punctuation.bracket)

(nullable_type
  "?" @operator)

[
  "+"
  "-"
  "*"
  "/"
  "**"
  "++"
  "=="
  "!="
  "<"
  "<="
  ">"
  ">="
  "&&"
  "||"
  "??"
  "!"
  "="
  "->"
  "=>"
  "|"
] @operator

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  ","
  ":"
  "."
] @punctuation.delimiter
