//! Patterns used in `match` arms.
//!
//! Grammar:
//!
//! ```text
//! pattern    := '_'                                                -- wildcard
//!            |  literal                                            -- literal
//!            |  CapName '{' field_pat (',' field_pat)* ','? '}'    -- struct destructure
//!            |  ident                                              -- bind (lowercase)
//! literal    := string_lit | int_lit | double_lit | 'true' | 'false'
//! field_pat  := ident (':' pattern)?
//! ```
//!
//! Distinguishing `Bind` from `Struct`:
//! - A capitalized identifier followed by `{` is a struct pattern.
//! - A capitalized identifier without `{` is rejected (struct patterns
//!   without fields aren't supported in v1; `_` is the catch-all).
//! - A lowercase identifier is a bind.
//!
//! `_` is recognised here but not added to the ordinary keyword set;
//! it remains usable in patterns only.

use chumsky::prelude::*;

use crate::ast::{Literal, Pattern, Spanned, StructPatternField};

use super::{
    string::plain_string,
    util::{Extra, ident, pad, span_to_range, spanned},
};

pub(super) fn pattern<'src>() -> impl Parser<'src, &'src str, Spanned<Pattern>, Extra<'src>> + Clone
{
    recursive(|pat| {
        let wildcard = just('_').padded_by(pad()).map_with(|_, e| Spanned {
            node: Pattern::Wildcard,
            span: span_to_range(e.span()),
        });

        let lit = literal_pattern();
        let struct_or_bind = struct_or_bind_pattern(pat);

        choice((wildcard, lit, struct_or_bind))
    })
}

fn literal_pattern<'src>() -> impl Parser<'src, &'src str, Spanned<Pattern>, Extra<'src>> + Clone {
    let bool_lit = choice((
        text::keyword("true").to(Literal::Boolean(true)),
        text::keyword("false").to(Literal::Boolean(false)),
    ));
    let null_lit = text::keyword("null").to(Literal::Null);
    let num_lit = number_literal();
    let string_lit = plain_string().map(Literal::String);
    choice((bool_lit, null_lit, num_lit, string_lit))
        .map_with(|lit, e| Spanned {
            node: Pattern::Lit(lit),
            span: span_to_range(e.span()),
        })
        .padded_by(pad())
}

fn number_literal<'src>() -> impl Parser<'src, &'src str, Literal, Extra<'src>> + Clone {
    let neg = just('-').or_not();
    let int_part = text::int(10);
    let frac = just('.').then(text::digits(10));
    neg.then(int_part)
        .then(frac.or_not())
        .to_slice()
        .try_map(|s: &str, span| {
            if s.contains('.') {
                let value = s
                    .parse::<f64>()
                    .map_err(|e| Rich::custom(span, e.to_string()))?;
                if value.is_finite() {
                    Ok(Literal::Double(value))
                } else {
                    Err(Rich::custom(span, "double literal is out of range"))
                }
            } else {
                s.parse::<i64>()
                    .map(Literal::Int)
                    .map_err(|e| Rich::custom(span, e.to_string()))
            }
        })
}

/// `Capitalized { … }` is a struct pattern; a bare lowercase
/// identifier is a bind. We commit to the struct branch when the
/// leading ident is uppercase AND followed by `{`; otherwise we treat
/// the leading ident as a bind.
fn struct_or_bind_pattern<'src, P>(
    pat: P,
) -> impl Parser<'src, &'src str, Spanned<Pattern>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Pattern>, Extra<'src>> + Clone + 'src,
{
    let lbrace = just('{').padded_by(pad());
    let rbrace = just('}').padded_by(pad());
    let comma = just(',').padded_by(pad());

    let field = spanned(ident())
        .padded_by(pad())
        .then(just(':').padded_by(pad()).ignore_then(pat).or_not())
        .map_with(|(name, sub), e| StructPatternField {
            name,
            pattern: sub,
            span: span_to_range(e.span()),
        });

    let fields = field
        .separated_by(comma)
        .at_least(1)
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(lbrace, rbrace);

    spanned(ident())
        .padded_by(pad())
        .then(fields.or_not())
        .try_map(|(name, maybe_fields), span| {
            let range = span_to_range(span);
            match maybe_fields {
                Some(fields) if starts_with_uppercase(&name.node) => Ok(Spanned {
                    node: Pattern::Struct { name, fields },
                    span: range,
                }),
                Some(_) => Err(Rich::custom(
                    span,
                    "struct pattern names must start with an uppercase letter",
                )),
                None if starts_with_bind_char(&name.node) => Ok(Spanned {
                    node: Pattern::Bind(name.node),
                    span: range,
                }),
                None => Err(Rich::custom(
                    span,
                    "bare pattern bindings must start with a lowercase letter or `_`",
                )),
            }
        })
}

fn starts_with_uppercase(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

fn starts_with_bind_char(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_lowercase())
}
