//! keron-lang: frontend for the keron language.

pub mod ast;
pub mod check;
pub mod diagnostic;
pub mod format;
pub mod lex;
pub mod parser;
pub mod trivia;

pub use ast::{
    BinOp, Block, CallArg, Comment, CommentAttachment, CommentMap, Expr, FnDecl, ForPattern,
    IntrinsicId, Item, Literal, MapEntry, MatchArm, Param, Pattern, Program, ReconcileDecl, Span,
    Spanned, Stmt, StringPart, StructDecl, StructField, StructLiteralField, StructPatternField,
    Type, TypeAliasDecl, UnaryOp, UseDecl, ValDecl,
};
pub use check::{
    CheckOutput, FnEnv, FnSig, ImportedSymbols, ParamSig, StructEnv, StructSig, check_module,
    check_module_full, resolve_type_names,
};
pub use diagnostic::Diagnostic;
pub use format::format;
pub use lex::{MultilineClose, is_multiline_close, multiline_open, raw_multiline_open_at};
pub use parser::parse;
pub use trivia::extract_comments;

/// Parse `src` and additionally extract every comment, attached to
/// its nearest AST node.
///
/// The `Program` returned is byte-identical to what [`parse`] would
/// return — `parse_with_comments` exists so the formatter can
/// round-trip comments without changing the parser's shape.
///
/// # Errors
///
/// Returns the parser's diagnostics unchanged when `src` is not a
/// valid keron program.
pub fn parse_with_comments(src: &str) -> Result<(Program, CommentMap), Vec<Diagnostic>> {
    let program = parse(src)?;
    let comments = extract_comments(src, &program);
    Ok((program, comments))
}
