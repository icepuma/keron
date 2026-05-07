//! chumsky-based parser for keron source.

mod block;
mod expr;
mod fn_decl;
mod reconcile;
mod string;
#[cfg(test)]
mod tests;
mod types;
mod util;

use chumsky::prelude::*;

use crate::{
    ast::{Expr, Item, Program, Spanned, ValDecl},
    diagnostic::Diagnostic,
};

use self::{
    expr::expr,
    fn_decl::fn_decl,
    reconcile::reconcile_decl,
    types::type_annotation,
    util::{Extra, ident, pad, span_to_range, spanned},
};

/// Parse keron source into a [`Program`].
///
/// # Errors
/// Returns one or more [`Diagnostic`]s when the source has syntax errors.
pub fn parse(src: &str) -> Result<Program, Vec<Diagnostic>> {
    let result = program().parse(src);
    if result.has_errors() {
        Err(result.errors().map(rich_to_diagnostic).collect())
    } else {
        Ok(result
            .into_output()
            .unwrap_or(Program { items: Vec::new() }))
    }
}

fn rich_to_diagnostic(r: &Rich<'_, char>) -> Diagnostic {
    let span = *r.span();
    Diagnostic::new(span.start()..span.end(), r.to_string())
}

fn program<'src>() -> impl Parser<'src, &'src str, Program, Extra<'src>> {
    item()
        .repeated()
        .collect::<Vec<_>>()
        .map(|items| Program { items })
        .padded_by(pad())
        .then_ignore(end())
}

fn item<'src>() -> impl Parser<'src, &'src str, Item, Extra<'src>> {
    let e = expr();
    // Top-level expression statements are restricted to expressions
    // beginning with `if` or `for` — those are the constructs that
    // produce a `Void` value (and so the only ones whose top-level
    // use as a statement is meaningful). Gating this with a `peek`
    // for the leading keyword also keeps normal declarations' error
    // messages crisp: errors inside `val x = …` aren't merged with a
    // generic "expression here" alternative.
    let void_stmt = choice((
        text::keyword("if").rewind().ignored(),
        text::keyword("for").rewind().ignored(),
    ))
    .ignore_then(e.clone())
    .map(Item::ExprStmt);
    choice((
        val_decl(e.clone()).map(Item::Val),
        fn_decl(e.clone()).map(Item::Fn),
        reconcile_decl(e).map(Item::Reconcile),
        void_stmt,
    ))
    .padded_by(pad())
}

pub(super) fn val_decl<'src, P>(
    expr: P,
) -> impl Parser<'src, &'src str, ValDecl, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let kw_val = text::keyword("val").padded_by(pad());
    let colon = just(':').padded_by(pad());
    let eq = just('=').padded_by(pad());
    let annotation = colon.ignore_then(spanned(type_annotation())).or_not();

    kw_val
        .ignore_then(spanned(ident()))
        .then(annotation)
        .then_ignore(eq)
        .then(expr)
        .map_with(|((name, ty), value), e| ValDecl {
            name,
            ty,
            value,
            span: span_to_range(e.span()),
        })
}
