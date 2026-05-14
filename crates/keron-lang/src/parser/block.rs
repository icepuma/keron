//! Block parser.
//!
//! Grammar:
//!
//! ```text
//! block    := '{' stmt* expr? '}'
//! stmt     := val_decl | reconcile_decl
//! ```
//!
//! Statements are restricted to `val` declarations and `reconcile`
//! directives — any other "statement-like" form would compute a value
//! and discard it, and keron has no side-effecting expressions to
//! make that meaningful. Conditional side effects appear either as
//! the trailing expression of a block (`if cond { reconcile foo }` is
//! the entire trailing slot, type `Void`) or at top level via
//! [`crate::ast::Item::ExprStmt`].
//!
//! Statement-or-trailing disambiguation is keyword-driven: any item
//! starting with `val` or `reconcile` is a statement, anything else
//! is the trailing expression. There is no `;` separator. As a
//! consequence, a block contains at most one expression.

use chumsky::prelude::*;

use crate::ast::{Block, Expr, Spanned, Stmt};

use super::{
    reconcile::reconcile_decl,
    util::{Extra, pad, span_to_range},
    val_decl,
};

pub(super) fn block<'src, P>(expr: P) -> impl Parser<'src, &'src str, Block, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let lbrace = just('{').padded_by(pad());
    let rbrace = just('}').padded_by(pad());

    let stmt = choice((
        val_decl(expr.clone()).map(Stmt::Val),
        reconcile_decl(expr.clone()).map(Stmt::Reconcile),
    ))
    .padded_by(pad());

    let body = stmt
        .repeated()
        .collect::<Vec<_>>()
        .then(expr.or_not())
        .map(|(stmts, trailing)| (stmts, trailing));

    body.delimited_by(lbrace, rbrace)
        .map_with(|(stmts, trailing), e| Block {
            stmts,
            trailing,
            span: span_to_range(e.span()),
        })
        .labelled("block")
}
