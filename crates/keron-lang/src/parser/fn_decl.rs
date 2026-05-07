//! `fn` declaration parser.
//!
//! Grammar:
//!
//! ```text
//! fn_decl := 'fn' ident '(' params? ')' ':' type '{' fn_body '}'
//! params  := param (',' param)* ','?
//! param   := ident ':' type ('=' expr)?
//! fn_body := val_decl* expr
//! ```
//!
//! Required-after-default and duplicate-param checks live in the
//! type checker, not here.

use chumsky::prelude::*;

use crate::ast::{Expr, FnBody, FnDecl, Param, Spanned};

use super::{
    types::type_annotation,
    util::{Extra, ident, pad, span_to_range, spanned},
    val_decl,
};

pub(super) fn fn_decl<'src, P>(expr: P) -> impl Parser<'src, &'src str, FnDecl, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let kw_fn = text::keyword("fn").padded_by(pad());
    let colon = just(':').padded_by(pad());
    let lparen = just('(').padded_by(pad());
    let rparen = just(')').padded_by(pad());
    let lbrace = just('{').padded_by(pad());
    let rbrace = just('}').padded_by(pad());

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
        .then(fn_body(expr).delimited_by(lbrace, rbrace))
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

fn fn_body<'src, P>(expr: P) -> impl Parser<'src, &'src str, FnBody, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    val_decl(expr.clone())
        .padded_by(pad())
        .repeated()
        .collect::<Vec<_>>()
        .then(expr)
        .map_with(|(bindings, result), e| FnBody {
            bindings,
            result,
            span: span_to_range(e.span()),
        })
}
