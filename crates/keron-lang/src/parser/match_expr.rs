//! `match` expression parser.
//!
//! Grammar:
//!
//! ```text
//! match_expr := 'match' expr '{' arm (',' arm)* ','? '}'
//! arm        := pattern ('if' expr)? '=>' expr
//! ```
//!
//! At least one arm is required; an empty `{}` is a parse error so
//! "non-exhaustive" diagnostics are saved for type-check time when we
//! actually know the scrutinee's type. Exhaustiveness, pattern type
//! correctness, and arm-body type uniformity all live in the type
//! checker.
//!
//! The optional `if guard` clause runs after the pattern binds, with
//! pattern bindings in scope; the arm fires only when the guard
//! returns `true`. Guarded arms do **not** count as covering for
//! exhaustiveness (the guard may always be false), so the checker
//! still requires a trailing catch-all or full literal cover.

use chumsky::prelude::*;

use crate::ast::{Expr, MatchArm, Spanned};

use super::{
    pattern::pattern,
    util::{Extra, pad, span_to_range},
};

pub(super) fn match_expr<'src, P>(
    expr: P,
) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let kw_match = text::keyword("match").padded_by(pad());
    let kw_if = text::keyword("if").padded_by(pad());
    let lbrace = just('{').padded_by(pad());
    let rbrace = just('}').padded_by(pad());
    let comma = just(',').padded_by(pad());
    let arrow = just("=>").padded_by(pad());

    let guard = kw_if.ignore_then(expr.clone()).or_not();
    let arm = pattern()
        .then(guard)
        .then_ignore(arrow)
        .then(expr.clone())
        .map_with(|((pat, guard), body), e| MatchArm {
            pattern: pat,
            guard,
            body,
            span: span_to_range(e.span()),
        });

    let arms = arm
        .separated_by(comma)
        .at_least(1)
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(lbrace, rbrace);

    kw_match
        .ignore_then(expr)
        .then(arms)
        .map_with(|(scrutinee, arms), e| Spanned {
            node: Expr::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span: span_to_range(e.span()),
        })
}
