//! keron-lang: frontend for the keron language.

pub mod ast;
pub mod check;
pub mod diagnostic;
pub mod parser;

pub use ast::{
    BinOp, Block, CallArg, Expr, FnDecl, ForPattern, IntrinsicId, Item, Literal, MapEntry,
    MatchArm, Param, Pattern, Program, ReconcileDecl, Span, Spanned, Stmt, StringPart, StructDecl,
    StructField, StructPatternField, Type, TypeAliasDecl, UnaryOp, UseDecl, ValDecl,
};
pub use check::{
    FnEnv, FnSig, ImportedSymbols, ParamSig, StructEnv, StructSig, check, check_module,
    resolve_type_names,
};
pub use diagnostic::Diagnostic;
pub use parser::parse;
