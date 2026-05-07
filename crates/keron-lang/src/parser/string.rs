//! String literal parser with `${expr}` interpolation.
//!
//! Lexical rules:
//!
//! - `\"`, `\\`, `\n`, `\r`, `\t`, `\$` are escape sequences.
//! - `${expr}` opens an interpolation; the inner expression is parsed
//!   by the supplied `expr` parser (mutual recursion).
//! - A bare `$` not followed by `{` is a literal `$`. To write a
//!   literal `${`, escape with `\$`.
//!
//! The parser produces `Expr::Literal(Literal::String(_))` when the
//! string contains no interpolations (so simple strings keep their
//! existing AST shape) and `Expr::Interpolation(parts)` otherwise.

use chumsky::prelude::*;

use crate::ast::{Expr, Literal, Spanned, StringPart};

use super::util::{Extra, pad, span_to_range};

pub(super) fn string_expr<'src, P>(
    expr: P,
) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let escape = just('\\').ignore_then(choice((
        just('"').to('"'),
        just('\\').to('\\'),
        just('n').to('\n'),
        just('r').to('\r'),
        just('t').to('\t'),
        just('$').to('$'),
    )));

    // A bare `$` is a literal char only when not followed by `{`.
    // `and_is` runs both parsers atomically at the same position; the
    // combined parser fails without consuming if the lookahead fails.
    let bare_dollar = just('$').and_is(just("${").not()).to('$');

    let normal = any().filter(|c: &char| *c != '"' && *c != '\\' && *c != '$');
    let text_char = choice((escape, bare_dollar, normal));
    let text_run = text_char
        .repeated()
        .at_least(1)
        .collect::<String>()
        .map(StringPart::Text);

    let interpolation = just("${")
        .ignore_then(expr.padded_by(pad()))
        .then_ignore(just('}'))
        .map(|e| StringPart::Expr(Box::new(e)));

    let parts = choice((interpolation, text_run))
        .repeated()
        .collect::<Vec<_>>();

    parts
        .delimited_by(just('"'), just('"'))
        .map_with(|parts, e| Spanned {
            node: collapse(parts),
            span: span_to_range(e.span()),
        })
}

fn collapse(parts: Vec<StringPart>) -> Expr {
    if parts.iter().all(|p| matches!(p, StringPart::Text(_))) {
        let mut s = String::new();
        for p in parts {
            if let StringPart::Text(t) = p {
                s.push_str(&t);
            }
        }
        Expr::Literal(Literal::String(s))
    } else {
        Expr::Interpolation(parts)
    }
}
