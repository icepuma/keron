//! `use` declaration parser.
//!
//! Grammar:
//!
//! ```text
//! use_decl    := 'from' plain_string 'use' name_list
//! name_list   := ident (',' ident)* ','?
//! plain_string := '"' (escape | char_no_quote_no_backslash_no_dollar)* '"'
//! ```
//!
//! The path string is plain — it accepts the same escape sequences as
//! the regular string literal but rejects `${` interpolations. Import
//! paths must be statically resolvable; an interpolated path is a
//! parse error.

use chumsky::prelude::*;

use crate::ast::UseDecl;

use super::{
    string::plain_string,
    util::{Extra, ident, pad, span_to_range, spanned},
};

pub(super) fn use_decl<'src>() -> impl Parser<'src, &'src str, UseDecl, Extra<'src>> + Clone {
    let kw_from = text::keyword("from").padded_by(pad());
    let kw_use = text::keyword("use").padded_by(pad());
    let comma = just(',').padded_by(pad());

    let names = spanned(ident())
        .separated_by(comma)
        .at_least(1)
        .allow_trailing()
        .collect::<Vec<_>>();

    kw_from
        .ignore_then(spanned(plain_string()).padded_by(pad()))
        .then_ignore(kw_use)
        .then(names)
        .map_with(|(source, names), e| UseDecl {
            source,
            names,
            span: span_to_range(e.span()),
        })
}
