//! keron-lang: frontend for the keron language.

pub mod ast;
pub mod check;
pub mod diagnostic;
pub mod parser;

pub use ast::{
    BinOp, Block, CallArg, Expr, FnDecl, ForPattern, IntrinsicId, Item, Literal, MapEntry, Param,
    Program, ReconcileDecl, Span, Spanned, Stmt, StringPart, Type, UnaryOp, UseDecl, ValDecl,
};
pub use check::{FnEnv, FnSig, ImportedSymbols, ParamSig, check, check_module};
pub use diagnostic::Diagnostic;
pub use parser::parse;
