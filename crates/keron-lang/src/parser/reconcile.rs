//! Parser for `reconcile` declarations.
//!
//! Three surface forms collapse into the same AST shape:
//!
//! ```text
//! reconcile_decl  := "reconcile" ( reconcile_block | chain )
//! reconcile_block := "{" chain ( ";" chain )* ";"? "}"
//! chain           := expr ( "->" expr )*
//! ```
//!
//! The inline form parses a single chain (length 1 = the original
//! `reconcile <expr>`). The block form groups multiple chains; chains
//! are separated by `;` (a trailing `;` is permitted) and the visual
//! "one chain per line" arrangement is convention. The `->` operator
//! only appears in this context — there is no general expression-level
//! chain operator.

use chumsky::prelude::*;

use crate::ast::{Expr, ReconcileDecl, Spanned};

use super::util::{Extra, pad, span_to_range};

pub(super) fn reconcile_decl<'src, P>(
    expr: P,
) -> impl Parser<'src, &'src str, ReconcileDecl, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let arrow = just("->").padded_by(pad());

    let chain = expr
        .clone()
        .then(arrow.ignore_then(expr).repeated().collect::<Vec<_>>())
        .map(|(head, rest)| {
            let mut steps = Vec::with_capacity(rest.len() + 1);
            steps.push(head);
            steps.extend(rest);
            steps
        });

    let semi = just(';').padded_by(pad());

    let block_form = chain
        .clone()
        .separated_by(semi.clone())
        .at_least(1)
        .allow_trailing()
        .collect::<Vec<_>>()
        .padded_by(pad())
        .delimited_by(just('{').padded_by(pad()), just('}').padded_by(pad()));

    // The inline form is guarded so it cannot match when the body
    // starts with `{`. Without this guard, a malformed block like
    // `reconcile { }` would silently fall back to `inline_form` and
    // succeed as the (un-reconcilable) empty map literal `{}`. With
    // it, `{` commits to the block form so a malformed block surfaces
    // as a parse error.
    let inline_form = just('{')
        .not()
        .rewind()
        .ignore_then(chain)
        .map(|steps| vec![steps]);

    text::keyword("reconcile")
        .padded_by(pad())
        .ignore_then(choice((block_form, inline_form)))
        .map_with(|chains, e| ReconcileDecl {
            chains,
            span: span_to_range(e.span()),
        })
}
