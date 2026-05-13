//! `type` alias parser. Today only string-union aliases exist.
//!
//! Grammar:
//!
//! ```text
//! type_alias := 'type' ident '=' string_lit ('|' string_lit)*
//! ```
//!
//! At least one variant is required; duplicates and emptiness are
//! reported by the type checker. The string-literal payload uses the
//! plain (no-interpolation) form: variants are compile-time constants,
//! so `${...}` inside a variant string is a parse error.

use chumsky::prelude::*;

use crate::ast::{Spanned, TypeAliasDecl};

use super::{
    string::plain_string,
    util::{Extra, ident, pad, span_to_range, spanned},
};

pub(super) fn type_alias_decl<'src>()
-> impl Parser<'src, &'src str, TypeAliasDecl, Extra<'src>> + Clone {
    let kw_type = text::keyword("type").padded_by(pad());
    let eq = just('=').padded_by(pad());
    let pipe = just('|').padded_by(pad());

    let variants = spanned(plain_string())
        .padded_by(pad())
        .separated_by(pipe)
        .at_least(1)
        .collect::<Vec<Spanned<String>>>();

    kw_type
        .ignore_then(spanned(ident()).padded_by(pad()))
        .then_ignore(eq)
        .then(variants)
        .map_with(|(name, variants), e| TypeAliasDecl {
            name,
            variants,
            span: span_to_range(e.span()),
        })
}
