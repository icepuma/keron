/**
 * Tree-sitter grammar for keron (https://github.com/icepuma/keron).
 *
 * Mirrors the operator table in crates/keron-lang/src/ast.rs:
 * field access > call > `**` > unary > `*` `/` > `+` `-` `++` >
 * comparisons > `&&` > `||` > `??` (right-assoc, loosest).
 * `**` binds tighter than unary minus (`-2 ** 2` is `-(2 ** 2)`).
 *
 * Raw multiline strings (`r#"""..."""#`, N hashes on both sides) need
 * a matching-hash-count backreference, so they live in an external
 * scanner (src/scanner.c).
 */

const PREC = {
  field: 10,
  call: 9,
  power: 8,
  unary: 7,
  multiplicative: 6,
  additive: 5,
  comparative: 4,
  and: 3,
  or: 2,
  coalesce: 1,
};

module.exports = grammar({
  name: "keron",

  externals: ($) => [$.raw_string],

  extras: ($) => [/\s/, $.comment],

  word: ($) => $.identifier,

  rules: {
    source_file: ($) => repeat($._statement),

    _statement: ($) =>
      choice(
        $.use_declaration,
        $.val_declaration,
        $.function_declaration,
        $.struct_declaration,
        $.type_alias_declaration,
        $.reconcile_statement,
        $._expression,
      ),

    // ── declarations ───────────────────────────────────────────────

    use_declaration: ($) =>
      seq(
        "from",
        field("source", $.string),
        "use",
        field("name", $._import_name),
        repeat(seq(",", field("name", $._import_name))),
      ),

    // Imports carry both value names (fns, vals) and capitalized
    // struct / type-alias names.
    _import_name: ($) => choice($.identifier, $.type_identifier),

    val_declaration: ($) =>
      seq(
        "val",
        field("name", $.identifier),
        optional(seq(":", field("type", $._type))),
        "=",
        field("value", $._expression),
      ),

    function_declaration: ($) =>
      seq(
        "fn",
        field("name", $.identifier),
        field("parameters", $.parameter_list),
        ":",
        field("return_type", $._type),
        field("body", $.block),
      ),

    parameter_list: ($) => seq("(", commaSep($.parameter), ")"),

    parameter: ($) =>
      seq(
        field("name", $.identifier),
        ":",
        field("type", $._type),
        optional(seq("=", field("default", $._expression))),
      ),

    struct_declaration: ($) =>
      seq(
        "struct",
        field("name", $.type_identifier),
        "{",
        commaSep($.field_declaration),
        "}",
      ),

    field_declaration: ($) =>
      seq(
        field("name", $.identifier),
        ":",
        field("type", $._type),
        optional(seq("=", field("default", $._expression))),
      ),

    type_alias_declaration: ($) =>
      seq(
        "type",
        field("name", $.type_identifier),
        "=",
        barSep1(field("variant", $.string)),
      ),

    // ── reconcile ──────────────────────────────────────────────────

    reconcile_statement: ($) =>
      seq("reconcile", choice($.reconcile_block, $.reconcile_chain)),

    reconcile_block: ($) => seq("{", repeat1($.reconcile_chain), "}"),

    reconcile_chain: ($) =>
      prec.right(seq($._expression, repeat(seq("->", $._expression)))),

    // ── blocks ─────────────────────────────────────────────────────

    block: ($) => seq("{", repeat($._block_statement), "}"),

    _block_statement: ($) =>
      choice($.val_declaration, $.reconcile_statement, $._expression),

    // ── types ──────────────────────────────────────────────────────

    _type: ($) =>
      choice(
        $.primitive_type,
        $.list_type,
        $.map_type,
        $.nullable_type,
        $.type_identifier,
      ),

    primitive_type: () => choice("String", "Int", "Boolean", "Double", "Void"),

    list_type: ($) => seq("List", "<", field("element", $._type), ">"),

    map_type: ($) =>
      seq("Map", "<", field("key", $._type), ",", field("value", $._type), ">"),

    nullable_type: ($) => seq($._type, "?"),

    // ── expressions ────────────────────────────────────────────────

    _expression: ($) =>
      choice(
        $.number,
        $.boolean,
        $.null,
        $.string,
        $.multiline_string,
        $.raw_string,
        $.identifier,
        $.list,
        $.map,
        $.unary_expression,
        $.binary_expression,
        $.call_expression,
        $.struct_literal,
        $.field_expression,
        $.if_expression,
        $.for_expression,
        $.match_expression,
        $.parenthesized_expression,
      ),

    parenthesized_expression: ($) => seq("(", $._expression, ")"),

    unary_expression: ($) =>
      prec(
        PREC.unary,
        seq(field("operator", choice("-", "!")), field("operand", $._expression)),
      ),

    binary_expression: ($) => {
      const table = [
        [prec.right, PREC.power, "**"],
        [prec.left, PREC.multiplicative, choice("*", "/")],
        [prec.left, PREC.additive, choice("+", "-", "++")],
        [
          prec.left,
          PREC.comparative,
          choice("==", "!=", "<", "<=", ">", ">="),
        ],
        [prec.left, PREC.and, "&&"],
        [prec.left, PREC.or, "||"],
        [prec.right, PREC.coalesce, "??"],
      ];
      return choice(
        ...table.map(([assoc, precedence, operator]) =>
          assoc(
            precedence,
            seq(
              field("left", $._expression),
              field("operator", operator),
              field("right", $._expression),
            ),
          ),
        ),
      );
    },

    field_expression: ($) =>
      prec.left(
        PREC.field,
        seq(
          field("receiver", $._expression),
          ".",
          field("field", alias($.identifier, $.field_identifier)),
        ),
      ),

    call_expression: ($) =>
      prec(
        PREC.call,
        seq(field("function", $.identifier), field("arguments", $.argument_list)),
      ),

    argument_list: ($) => seq("(", commaSep($.argument), ")"),

    argument: ($) =>
      seq(
        optional(seq(field("name", $.identifier), "=")),
        field("value", $._expression),
      ),

    struct_literal: ($) =>
      seq(
        field("name", $.type_identifier),
        "{",
        commaSep($.struct_literal_field),
        "}",
      ),

    struct_literal_field: ($) =>
      seq(
        field("name", alias($.identifier, $.field_identifier)),
        optional(seq(":", field("value", $._expression))),
      ),

    if_expression: ($) =>
      prec.right(
        seq(
          "if",
          field("condition", $._expression),
          field("consequence", $.block),
          optional(
            seq("else", field("alternative", choice($.if_expression, $.block))),
          ),
        ),
      ),

    for_expression: ($) =>
      seq(
        "for",
        field("pattern", choice($.identifier, $.for_entry_pattern)),
        "in",
        field("iterable", $._expression),
        field("body", $.block),
      ),

    for_entry_pattern: ($) =>
      seq(
        "(",
        field("key", $.identifier),
        ",",
        field("value", $.identifier),
        ")",
      ),

    match_expression: ($) =>
      seq(
        "match",
        field("scrutinee", $._expression),
        "{",
        commaSep($.match_arm),
        "}",
      ),

    match_arm: ($) =>
      seq(
        field("pattern", $._pattern),
        optional(seq("if", field("guard", $._expression))),
        "=>",
        field("value", $._expression),
      ),

    // ── patterns ───────────────────────────────────────────────────

    _pattern: ($) =>
      choice(
        $.number,
        $.negative_number_pattern,
        $.boolean,
        $.null,
        $.string,
        $.wildcard_pattern,
        $.identifier,
        $.struct_pattern,
      ),

    negative_number_pattern: ($) => seq("-", $.number),

    wildcard_pattern: () => "_",

    struct_pattern: ($) =>
      seq(
        field("name", $.type_identifier),
        "{",
        commaSep($.struct_pattern_field),
        "}",
      ),

    struct_pattern_field: ($) =>
      seq(
        field("name", alias($.identifier, $.field_identifier)),
        optional(seq(":", field("pattern", $._pattern))),
      ),

    // ── collections ────────────────────────────────────────────────

    list: ($) => seq("[", commaSep($._expression), "]"),

    map: ($) => seq("{", commaSep($.map_entry), "}"),

    map_entry: ($) =>
      seq(field("key", $._expression), ":", field("value", $._expression)),

    // ── strings ────────────────────────────────────────────────────

    string: ($) =>
      seq(
        '"',
        repeat(
          choice($._string_content, $.escape_sequence, $.interpolation, "$"),
        ),
        token.immediate('"'),
      ),

    _string_content: () => token.immediate(prec(1, /[^"\\$\n\r]+/)),

    multiline_string: ($) =>
      seq(
        '"""',
        repeat(
          choice(
            $._multiline_string_content,
            $.escape_sequence,
            $.interpolation,
            "$",
          ),
        ),
        token.immediate('"""'),
      ),

    // A lone `"` inside a multiline body is content. It must stay at
    // default lexical precedence so the three-quote closer wins by
    // longest match when a run reaches 3 (explicit precedence would
    // trump token length).
    _multiline_string_content: () =>
      choice(token.immediate(prec(1, /[^"\\$]+/)), token.immediate('"')),

    escape_sequence: () => token.immediate(/\\[\\"nrt$]/),

    interpolation: ($) =>
      seq(token.immediate("${"), field("expression", $._expression), "}"),

    // ── terminals ──────────────────────────────────────────────────

    identifier: () => /[_a-z][a-zA-Z0-9_]*/,

    type_identifier: () => /[A-Z][a-zA-Z0-9_]*/,

    number: () => /[0-9]+(\.[0-9]+)?/,

    boolean: () => choice("true", "false"),

    null: () => "null",

    comment: () => token(seq("#", /[^\n\r]*/)),
  },
});

function commaSep(rule) {
  return optional(commaSep1(rule));
}

function commaSep1(rule) {
  return seq(rule, repeat(seq(",", rule)), optional(","));
}

function barSep1(rule) {
  return seq(rule, repeat(seq("|", rule)));
}
