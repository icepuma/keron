//! Type-annotation parser. Supports the four primitives plus a single
//! generic constructor `List<T>` (recursively nestable).

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
            text::keyword("Symlink").to(Type::Symlink),
            text::keyword("File").to(Type::File),
            text::keyword("Directory").to(Type::Directory),
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
        choice((list, map, primitive))
    })
}
