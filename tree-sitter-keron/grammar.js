/**
 * Tree-sitter grammar for keron. Tracks the keron-lang chumsky parser
 * but is not a substitute for it: precedence and a couple of
 * disambiguations differ where it makes the grammar simpler. Used by
 * editors (Zed, Neovim) for highlighting and structure-aware editing.
 */

module.exports = grammar({
  name: 'keron',

  word: $ => $.identifier,

  extras: $ => [
    /\s+/,
    $.line_comment,
  ],

  rules: {
    program: $ => repeat($._item),

    _item: $ => choice(
      $.use_decl,
      $.val_decl,
      $.fn_decl,
      $.reconcile_decl,
      $.if_expr,
      $.for_expr,
    ),

    // ---------- declarations ----------

    use_decl: $ => seq(
      'from',
      field('source', $.string),
      'use',
      field('names', commaSep1($.identifier)),
    ),

    val_decl: $ => seq(
      'val',
      field('name', $.identifier),
      optional(seq(':', field('type', $._type))),
      '=',
      field('value', $._expr),
    ),

    fn_decl: $ => seq(
      'fn',
      field('name', $.identifier),
      '(',
      optional(commaSep1($.param)),
      ')',
      ':',
      field('return_type', $._type),
      field('body', $.block),
    ),

    param: $ => seq(
      field('name', $.identifier),
      ':',
      field('type', $._type),
      optional(seq('=', field('default', $._expr))),
    ),

    reconcile_decl: $ => seq(
      'reconcile',
      choice($.reconcile_block, $.chain),
    ),

    reconcile_block: $ => seq(
      '{',
      $.chain,
      repeat(seq(';', $.chain)),
      optional(';'),
      '}',
    ),

    chain: $ => prec.left(seq(
      $._expr,
      repeat(seq('->', $._expr)),
    )),

    // ---------- types ----------

    _type: $ => choice(
      $.primitive_type,
      $.list_type,
      $.map_type,
    ),

    primitive_type: _ => choice(
      'String', 'Int', 'Boolean', 'Double',
      'Symlink', 'File', 'Directory', 'Resource', 'Void',
    ),

    list_type: $ => seq('List', '<', $._type, '>'),
    map_type: $ => seq('Map', '<', $._type, ',', $._type, '>'),

    // ---------- expressions ----------

    _expr: $ => choice(
      $.binary_expr,
      $.unary_expr,
      $.if_expr,
      $.for_expr,
      $.call,
      $.var,
      $.list,
      $.map,
      $.string,
      $.number,
      $.boolean,
      $.parens,
    ),

    binary_expr: $ => choice(
      prec.left(1, seq($._expr, choice('==', '!='), $._expr)),
      prec.left(2, seq($._expr, choice('<', '<=', '>', '>='), $._expr)),
      prec.left(3, seq($._expr, choice('++', '+', '-'), $._expr)),
      prec.left(4, seq($._expr, choice('*', '/'), $._expr)),
      prec.right(5, seq($._expr, '**', $._expr)),
    ),

    unary_expr: $ => prec(6, seq('-', $._expr)),

    if_expr: $ => prec.right(seq(
      'if',
      field('condition', $._expr),
      field('then', $.block),
      optional(seq(
        'else',
        field('else', choice($.if_expr, $.block)),
      )),
    )),

    for_expr: $ => seq(
      'for',
      choice(
        field('pattern', $.identifier),
        seq('(', $.identifier, ',', $.identifier, ')'),
      ),
      'in',
      field('iter', $._expr),
      field('body', $.block),
    ),

    call: $ => prec(7, seq(
      field('callee', $.identifier),
      '(',
      optional(commaSep1($.call_arg)),
      ')',
    )),

    call_arg: $ => choice(
      seq(field('name', $.identifier), '=', field('value', $._expr)),
      $._expr,
    ),

    list: $ => seq(
      '[',
      optional(commaSep1($._expr)),
      ']',
    ),

    map: $ => seq(
      '{',
      optional(commaSep1($.map_entry)),
      '}',
    ),

    map_entry: $ => seq(
      field('key', $._expr),
      ':',
      field('value', $._expr),
    ),

    parens: $ => seq('(', $._expr, ')'),

    var: $ => $.identifier,

    block: $ => seq(
      '{',
      repeat(choice($.val_decl, $.reconcile_decl)),
      optional($._expr),
      '}',
    ),

    // ---------- literals ----------

    string: $ => seq(
      '"',
      repeat(choice(
        $.string_text,
        $.escape,
        $.interpolation,
      )),
      '"',
    ),

    string_text: _ => token.immediate(/[^"\\$]+/),
    escape: _ => token.immediate(/\\["\\nrt$]/),
    interpolation: $ => seq('${', $._expr, '}'),

    number: _ => /\d+(\.\d+)?/,
    boolean: _ => choice('true', 'false'),

    identifier: _ => /[a-zA-Z_][a-zA-Z0-9_]*/,

    line_comment: _ => /#[^\n]*/,
  },
});

function commaSep1(rule) {
  return seq(rule, repeat(seq(',', rule)), optional(','));
}
