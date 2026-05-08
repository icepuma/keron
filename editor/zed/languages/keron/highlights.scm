; Re-uses the queries shipped with the tree-sitter-keron grammar.
; Zed reads `queries/highlights.scm` from the grammar directory by
; default, so this file just delegates by `inherits` if Zed supports
; it; otherwise this file is the canonical highlight query for the
; extension.

[
  "val"
  "fn"
  "reconcile"
  "if"
  "else"
  "for"
  "in"
] @keyword

(primitive_type) @type
(list_type "List" @type)
(map_type "Map" @type)

(boolean) @boolean
(number) @number
(line_comment) @comment

(string) @string
(escape) @string.escape
(interpolation
  "${" @punctuation.special
  "}" @punctuation.special)

(val_decl name: (identifier) @variable)
(fn_decl name: (identifier) @function)
(param name: (identifier) @variable.parameter)

(call callee: (identifier) @function.call)

((call callee: (identifier) @function.builtin)
 (#match? @function.builtin "^(symlink|file|directory)$"))

[
  "->"
  "++"
  "+"
  "-"
  "*"
  "/"
  "**"
  "=="
  "!="
  "<"
  "<="
  ">"
  ">="
  "="
] @operator

[ "(" ")" "[" "]" "{" "}" ] @punctuation.bracket
[ "," ":" ";" ] @punctuation.delimiter
