//! Parser for `reconcile` declarations.
//!
//! Three surface forms collapse into the same AST shape:
//!
//! ```text
//! reconcile_decl  := "reconcile" ( reconcile_block | chain )
//! reconcile_block := "{" chain+ "}"
//! chain           := expr ( "->" expr )*
//! ```
//!
//! The inline form parses a single chain (length 1 = the original
//! `reconcile <expr>`). The block form groups multiple chains, one
//! after another with no separator — like every other statement
//! block in the language (statement braces have no separator; data
//! braces, i.e. maps, use commas). Expressions are self-terminating,
//! so adjacent chains parse deterministically; the visual "one chain
//! per line" arrangement is convention, and a multi-line `->` chain
//! may break before or after the arrow (whitespace is free). The `->`
//! operator only appears in this context — there is no general
//! expression-level chain operator.
//!
//! Two whitespace-insensitivity hazards to know about (both surface
//! as type errors, never as silent misbehavior, and neither occurs
//! with ordinary call-shaped steps): a bare-variable step followed by
//! a parenthesized step merges into a call (`foo` ␤ `(bar)` reads as
//! `foo(bar)`), and a step followed by a line starting with a binary
//! operator merges into one expression (`a` ␤ `- b` reads as
//! `a - b`).
//!
//! A stray `;` between chains — the pre-0.6 separator — gets a
//! targeted diagnostic instead of a generic parse error, and parsing
//! continues so every occurrence is reported in one run.

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

    // Recognize the removed `;` separator explicitly: emit a targeted
    // diagnostic but keep parsing, so a file migrated by hand reports
    // every stray semicolon in a single run.
    let stray_semi = just(';').padded_by(pad()).validate(|_, e, emitter| {
        emitter.emit(Rich::custom(
            e.span(),
            "`;` chain separators were removed; put each chain on its own line",
        ));
    });

    let block_form = chain
        .clone()
        .then_ignore(stray_semi.repeated())
        .repeated()
        .at_least(1)
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
