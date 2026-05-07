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
        ));
        let list = text::keyword("List")
            .ignore_then(just('<').padded_by(pad()))
            .ignore_then(ty)
            .then_ignore(just('>').padded_by(pad()))
            .map(|inner| Type::List(Box::new(inner)));
        choice((list, primitive))
    })
}
