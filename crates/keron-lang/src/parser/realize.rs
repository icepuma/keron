//! Parser for `realize` declarations.
//!
//! `realize EXPR` is a top-level item that promotes a resource value
//! (or list of resources) into the apply queue. The checker validates
//! the expression's type; the parser only requires that an expression
//! follows the keyword.

use chumsky::prelude::*;

use crate::ast::{Expr, RealizeDecl, Spanned};

use super::util::{Extra, pad, span_to_range};

pub(super) fn realize_decl<'src, P>(
    expr: P,
) -> impl Parser<'src, &'src str, RealizeDecl, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    text::keyword("realize")
        .padded_by(pad())
        .ignore_then(expr)
        .map_with(|expr_node, e| RealizeDecl {
            expr: expr_node,
            span: span_to_range(e.span()),
        })
}
