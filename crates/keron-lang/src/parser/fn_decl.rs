//! `fn` declaration parser.
//!
//! Grammar:
//!
//! ```text
//! fn_decl := 'fn' ident '(' params? ')' ':' type block
//! params  := param (',' param)* ','?
//! param   := ident ':' type ('=' expr)?
//! ```
//!
//! The function body is a regular [`crate::ast::Block`]; whether the
//! trailing expression is required is a type-checker concern (it is
//! required when the return type is non-`Void`).
//!
//! Required-after-default and duplicate-param checks also live in the
//! type checker, not here.

use chumsky::prelude::*;

use crate::ast::{Expr, FnDecl, Param, Spanned};

use super::{
    block::block,
    types::type_annotation,
    util::{Extra, ident, pad, span_to_range, spanned},
};

pub(super) fn fn_decl<'src, P>(expr: P) -> impl Parser<'src, &'src str, FnDecl, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let kw_fn = text::keyword("fn").padded_by(pad());
    let colon = just(':').padded_by(pad());
    let lparen = just('(').padded_by(pad());
    let rparen = just(')').padded_by(pad());

    let param_list = param(expr.clone())
        .separated_by(just(',').padded_by(pad()))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(lparen, rparen);

    kw_fn
        .ignore_then(spanned(ident()))
        .then(param_list)
        .then_ignore(colon)
        .then(spanned(type_annotation()).padded_by(pad()))
        .then(block(expr))
        .map_with(|(((name, params), return_type), body), e| FnDecl {
            name,
            params,
            return_type,
            body,
            span: span_to_range(e.span()),
        })
}

fn param<'src, P>(expr: P) -> impl Parser<'src, &'src str, Param, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let colon = just(':').padded_by(pad());
    let eq = just('=').padded_by(pad());

    spanned(ident())
        .padded_by(pad())
        .then_ignore(colon)
        .then(spanned(type_annotation()).padded_by(pad()))
        .then(eq.ignore_then(expr).or_not())
        .map_with(|((name, ty), default), e| Param {
            name,
            ty,
            default,
            span: span_to_range(e.span()),
        })
}
