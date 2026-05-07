//! keron-lang: frontend for the keron language.

pub mod ast;
pub mod check;
pub mod diagnostic;
pub mod parser;

pub use ast::{Item, Literal, Program, Span, Spanned, Type, ValDecl};
pub use check::check;
pub use diagnostic::Diagnostic;
pub use parser::parse;
