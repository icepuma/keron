; Highlight queries for keron. Loaded by Zed and any other tree-sitter
; consumer pointed at this grammar's `queries/` directory.

; ---------- keywords ----------

[
  "val"
  "fn"
  "reconcile"
  "if"
  "else"
  "for"
  "in"
] @keyword

; ---------- types ----------

(primitive_type) @type
(list_type "List" @type)
(map_type "Map" @type)

; ---------- literals ----------

(boolean) @constant.builtin.boolean
(number) @number
(line_comment) @comment

(string) @string
(escape) @string.escape
(interpolation
  "${" @punctuation.special
  "}" @punctuation.special) @embedded

; ---------- declarations ----------

(val_decl name: (identifier) @variable)
(fn_decl name: (identifier) @function)
(param name: (identifier) @variable.parameter)

; ---------- calls ----------

(call callee: (identifier) @function.call)

; Builtin resource constructors get a distinct highlight so editors can
; theme them like primitives.
((call callee: (identifier) @function.builtin)
 (#match? @function.builtin "^(symlink|file|directory)$"))

; ---------- operators ----------

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

; ---------- punctuation ----------

[ "(" ")" "[" "]" "{" "}" ] @punctuation.bracket
[ "," ":" ";" ] @punctuation.delimiter
