//! `struct` declaration parser.
//!
//! Grammar:
//!
//! ```text
//! struct_decl := 'struct' ident '{' field (',' field)* ','? '}'
//! field       := ident ':' type ('=' expr)?
//! ```
//!
//! A struct must have at least one field; an empty `{}` is rejected as
//! a parse error so users don't accidentally declare a unit struct
//! that they can't construct meaningfully. Duplicate field names and
//! invalid field types are reported by the type checker, not here.
//!
//! Defaults follow the fn-parameter rule: any required field must
//! precede every defaulted one. The ordering check is the type
//! checker's responsibility (so the error carries a proper span and
//! diagnostic shape), not the parser's.

use chumsky::prelude::*;

use crate::ast::{Expr, Spanned, StructDecl, StructField};

use super::{
    types::type_annotation,
    util::{Extra, ident, pad, span_to_range, spanned},
};

pub(super) fn struct_decl<'src, P>(
    expr: P,
) -> impl Parser<'src, &'src str, StructDecl, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let kw_struct = text::keyword("struct").padded_by(pad());
    let lbrace = just('{').padded_by(pad());
    let rbrace = just('}').padded_by(pad());
    let comma = just(',').padded_by(pad());

    let fields = field(expr)
        .separated_by(comma)
        .at_least(1)
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(lbrace, rbrace);

    kw_struct
        .ignore_then(spanned(ident()).padded_by(pad()))
        .then(fields)
        .try_map(|(name, fields), span| {
            if !name.node.starts_with(|c: char| c.is_ascii_uppercase()) {
                return Err(Rich::custom(
                    span,
                    "struct names must start with an uppercase letter",
                ));
            }
            Ok(StructDecl {
                name,
                fields,
                span: span_to_range(span),
            })
        })
}

fn field<'src, P>(expr: P) -> impl Parser<'src, &'src str, StructField, Extra<'src>> + Clone
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
        .map_with(|((name, ty), default), e| StructField {
            name,
            ty,
            default,
            span: span_to_range(e.span()),
        })
}
