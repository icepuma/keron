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
    /// Optional type annotation. When `None`, the type is inferred from
    /// `value` (which makes the inferred type trivially correct, so the
    /// checker has nothing to verify).
    pub ty: Option<Spanned<Type>>,
    pub value: Spanned<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Unary {
        op: UnaryOp,
        operand: Box<Spanned<Self>>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Spanned<Self>>,
        rhs: Box<Spanned<Self>>,
    },
    /// A double-quoted string with one or more `${expr}` interpolations.
    /// Strings without interpolations are stored as `Literal::String`
    /// instead, so this variant always has at least one [`StringPart::Expr`].
    Interpolation(Vec<StringPart>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Text(String),
    Expr(Box<Spanned<Expr>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
}

impl UnaryOp {
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Neg => "-",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
}

impl BinOp {
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Pow => "**",
        }
    }
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
