//! AST nodes for keron source.

use core::ops::Range;

pub type Span = Range<usize>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Val(ValDecl),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValDecl {
    pub name: Spanned<String>,
    pub ty: Spanned<Type>,
    pub value: Spanned<Literal>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    String,
    Int,
    Boolean,
    Double,
}

impl Type {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::Int => "Int",
            Self::Boolean => "Boolean",
            Self::Double => "Double",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    String(String),
    Int(i64),
    Boolean(bool),
    Double(f64),
}

impl Literal {
    #[must_use]
    pub const fn type_of(&self) -> Type {
        match self {
            Self::String(_) => Type::String,
            Self::Int(_) => Type::Int,
            Self::Boolean(_) => Type::Boolean,
            Self::Double(_) => Type::Double,
        }
    }
}
