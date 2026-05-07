//! keron-lang: frontend for the keron language.

pub mod ast;
pub mod check;
pub mod diagnostic;
pub mod parser;

pub use ast::{
    BinOp, CallArg, Expr, FnBody, FnDecl, Item, Literal, MapEntry, Param, Program, ReconcileDecl,
    Span, Spanned, StringPart, Type, UnaryOp, ValDecl,
};
pub use check::check;
pub use diagnostic::Diagnostic;
pub use parser::parse;
