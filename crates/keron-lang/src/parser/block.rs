//! Block parser.
//!
//! Grammar:
//!
//! ```text
//! block    := '{' element* '}'
//! element  := val_decl | reconcile_decl | expr
//! ```
//!
//! Elements juxtapose with no separator (statement braces never have
//! one; data braces — maps — use commas). The *last* element, when it
//! is an expression, becomes the block's trailing expression (the
//! block's value); every earlier expression element becomes a
//! [`Stmt::Expr`] statement, which the type checker requires to be
//! `Void` — so a body can hold several `if`-gated `reconcile`s in
//! sequence, matching top-level multiplicity.
//!
//! Expressions are self-terminating, so adjacent elements parse
//! deterministically. The same two whitespace hazards as reconcile
//! blocks apply (a bare variable followed by `(…)` merges into a
//! call; a line starting with a binary operator continues the
//! previous expression) — both surface as type errors, never as
//! silent misbehavior.

use chumsky::prelude::*;

use crate::ast::{Block, Expr, Spanned, Stmt};

use super::{
    reconcile::reconcile_decl,
    util::{Extra, pad, span_to_range},
    val_decl,
};

enum Element {
    Stmt(Stmt),
    Expr(Spanned<Expr>),
}

pub(super) fn block<'src, P>(expr: P) -> impl Parser<'src, &'src str, Block, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let lbrace = just('{').padded_by(pad());
    let rbrace = just('}').padded_by(pad());

    let element = choice((
        val_decl(expr.clone()).map(|v| Element::Stmt(Stmt::Val(v))),
        reconcile_decl(expr.clone()).map(|r| Element::Stmt(Stmt::Reconcile(r))),
        expr.map(Element::Expr),
    ))
    .padded_by(pad());

    element
        .repeated()
        .collect::<Vec<_>>()
        .delimited_by(lbrace, rbrace)
        .map_with(|elements, e| {
            let mut stmts: Vec<Stmt> = Vec::with_capacity(elements.len());
            let mut trailing: Option<Spanned<Expr>> = None;
            let last = elements.len().checked_sub(1);
            for (i, element) in elements.into_iter().enumerate() {
                match element {
                    Element::Stmt(s) => stmts.push(s),
                    // Only the final element may be the block's value;
                    // earlier expressions are Void effect statements.
                    Element::Expr(x) if Some(i) == last => trailing = Some(x),
                    Element::Expr(x) => stmts.push(Stmt::Expr(x)),
                }
            }
            Block {
                stmts,
                trailing,
                span: span_to_range(e.span()),
            }
        })
        .labelled("block")
}
