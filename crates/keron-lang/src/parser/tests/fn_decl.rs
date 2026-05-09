//! Function declaration parser tests.

use super::ok;
use crate::{
    ast::{Expr, Item, Literal},
    parser::parse,
};

fn first_fn(src: &str) -> crate::ast::FnDecl {
    let prog = ok(src);
    match prog.items.into_iter().next().expect("at least one item") {
        Item::Fn(f) => f,
        Item::Use(_)
        | Item::Val(_)
        | Item::Struct(_)
        | Item::TypeAlias(_)
        | Item::Reconcile(_)
        | Item::ExprStmt(_) => {
            panic!("expected a fn item")
        }
    }
}

#[test]
fn fn_no_params() {
    let f = first_fn("fn it(): Int { 1 }");
    assert_eq!(f.name.node, "it");
    assert_eq!(f.params.len(), 0);
    assert_eq!(f.body.stmts.len(), 0);
    assert_eq!(
        f.body.trailing.as_ref().expect("trailing expr").node,
        Expr::Literal(Literal::Int(1))
    );
}

#[test]
fn fn_one_param() {
    let f = first_fn("fn double(n: Int): Int { n * 2 }");
    assert_eq!(f.params.len(), 1);
    assert_eq!(f.params[0].name.node, "n");
    assert!(f.params[0].default.is_none());
}

#[test]
fn fn_multiple_params() {
    let f = first_fn("fn add(a: Int, b: Int): Int { a + b }");
    assert_eq!(f.params.len(), 2);
    assert_eq!(f.params[0].name.node, "a");
    assert_eq!(f.params[1].name.node, "b");
}

#[test]
fn fn_with_default() {
    let f = first_fn("fn pad(s: String, n: Int = 2): String { s }");
    assert!(f.params[0].default.is_none());
    assert!(f.params[1].default.is_some());
}

#[test]
fn fn_body_with_locals() {
    // No semicolons — statements are separated by whitespace; `val` /
    // `reconcile` keywords are the sentinels that start each one.
    let f = first_fn("fn area(): Int { val w = 3 val h = 4 w * h }");
    assert_eq!(f.body.stmts.len(), 2);
    let crate::ast::Stmt::Val(w) = &f.body.stmts[0] else {
        panic!("expected val stmt");
    };
    assert_eq!(w.name.node, "w");
    let crate::ast::Stmt::Val(h) = &f.body.stmts[1] else {
        panic!("expected val stmt");
    };
    assert_eq!(h.name.node, "h");
}

#[test]
fn fn_trailing_comma_in_params() {
    assert!(parse("fn f(a: Int, b: Int,): Int { a }").is_ok());
}

#[test]
fn fn_complex_return_type() {
    let f = first_fn("fn ids(): List<List<Int>> { [[1, 2], [3]] }");
    let crate::ast::Type::List(_) = &f.return_type.node else {
        panic!("expected list return type");
    };
}

#[test]
fn empty_body_parses_for_void_fn() {
    // An empty `{ }` is a Block with no trailing — type-legal only
    // when the return type is `Void`. Whether it type-checks is the
    // checker's concern, but the parser must accept the syntax.
    assert!(parse("fn f(): Void { }").is_ok());
}

#[test]
fn fn_body_with_only_locals_parses() {
    // Like the empty case, this is type-legal only when returning Void
    // (a non-Void return needs a trailing expression). The parser
    // happily produces a Block whose trailing is None.
    let f = first_fn("fn f(): Void { val x = 1 }");
    assert_eq!(f.body.stmts.len(), 1);
    assert!(f.body.trailing.is_none());
}

#[test]
fn rejects_missing_return_type() {
    assert!(parse("fn f() { 1 }").is_err());
}

#[test]
fn rejects_fn_keyword_as_var_name() {
    assert!(parse("val fn = 1").is_err());
}

#[test]
fn fn_alongside_val_top_level() {
    let prog = ok("val x = 1\nfn f(): Int { x }\nval y = 2");
    assert_eq!(prog.items.len(), 3);
}
