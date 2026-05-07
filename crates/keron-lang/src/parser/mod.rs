//! chumsky-based parser for keron source.

mod expr;
mod string;
#[cfg(test)]
mod tests;
mod util;

use chumsky::prelude::*;

use crate::{
    ast::{Item, Program, ValDecl},
    diagnostic::Diagnostic,
};

use self::{
    expr::expr,
    util::{Extra, ident, pad, span_to_range, spanned, type_annotation},
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
    val_decl().map(Item::Val).padded_by(pad())
}

fn val_decl<'src>() -> impl Parser<'src, &'src str, ValDecl, Extra<'src>> {
    let kw_val = text::keyword("val").padded_by(pad());
    let colon = just(':').padded_by(pad());
    let eq = just('=').padded_by(pad());
    let annotation = colon.ignore_then(spanned(type_annotation())).or_not();

    kw_val
        .ignore_then(spanned(ident()))
        .then(annotation)
        .then_ignore(eq)
        .then(expr())
        .map_with(|((name, ty), value), e| ValDecl {
            name,
            ty,
            value,
            span: span_to_range(e.span()),
        })
}
