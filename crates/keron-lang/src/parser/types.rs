//! Type-annotation parser. Supports the four value primitives, the
//! generic constructors `List<T>` and `Map<K, V>`, `Void`, and a
//! `Named` fallback for capitalized identifiers (e.g. `Symlink`)
//! that the module loader resolves against imported types.

use chumsky::prelude::*;

use crate::ast::Type;

use super::util::{Extra, pad};

pub(super) fn type_annotation<'src>() -> impl Parser<'src, &'src str, Type, Extra<'src>> + Clone {
    recursive(|ty| {
        let primitive = choice((
            text::keyword("String").to(Type::String),
            text::keyword("Int").to(Type::Int),
            text::keyword("Boolean").to(Type::Boolean),
            text::keyword("Double").to(Type::Double),
            text::keyword("Void").to(Type::Void),
        ));
        let list = text::keyword("List")
            .ignore_then(just('<').padded_by(pad()))
            .ignore_then(ty.clone())
            .then_ignore(just('>').padded_by(pad()))
            .map(|inner| Type::List(Box::new(inner)));
        let map = text::keyword("Map")
            .ignore_then(just('<').padded_by(pad()))
            .ignore_then(ty.clone())
            .then_ignore(just(',').padded_by(pad()))
            .then(ty)
            .then_ignore(just('>').padded_by(pad()))
            .map(|(k, v)| Type::Map(Box::new(k), Box::new(v)));
        // Any other identifier in type position is a `Named` placeholder;
        // the module loader replaces it with the canonical type the name
        // resolves to (e.g. `Symlink` from `std:fs`). The leading-uppercase
        // restriction keeps lowercase names — which are conventionally
        // values/fns — out of type position so the error reads cleanly
        // ("unknown type") rather than "expected `<`".
        let named = text::ident()
            .filter(|s: &&str| s.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
            .map(|s: &str| Type::Named(s.to_string()));
        choice((list, map, primitive, named))
    })
}
