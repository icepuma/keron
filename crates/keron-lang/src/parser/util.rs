//! Shared parser combinators: padding, identifiers, type annotations,
//! span conversion.

use chumsky::prelude::*;

use crate::ast::{Span, Spanned};

pub(super) type Extra<'src> = extra::Err<Rich<'src, char>>;

const KEYWORDS: &[&str] = &[
    "val", "true", "false", "String", "Int", "Boolean", "Double", "List",
];

pub(super) fn ident<'src>() -> impl Parser<'src, &'src str, String, Extra<'src>> + Clone {
    text::ident().try_map(|s: &str, span| {
        if KEYWORDS.contains(&s) {
            Err(Rich::custom(span, format!("`{s}` is a reserved keyword")))
        } else {
            Ok(s.to_string())
        }
    })
}

pub(super) fn spanned<'src, T, P>(p: P) -> impl Parser<'src, &'src str, Spanned<T>, Extra<'src>>
where
    P: Parser<'src, &'src str, T, Extra<'src>>,
{
    p.map_with(|node, e| Spanned {
        node,
        span: span_to_range(e.span()),
    })
}

pub(super) fn pad<'src>() -> impl Parser<'src, &'src str, (), Extra<'src>> + Clone {
    let comment = just('#')
        .then(any().filter(|c: &char| *c != '\n').repeated())
        .ignored();
    let ws = any().filter(|c: &char| c.is_whitespace()).ignored();
    choice((ws, comment)).repeated().ignored()
}

pub(super) const fn span_to_range(s: SimpleSpan<usize>) -> Span {
    s.start..s.end
}
