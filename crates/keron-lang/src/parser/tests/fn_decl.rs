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
        Item::Val(_) => panic!("expected a fn item"),
    }
}

#[test]
fn fn_no_params() {
    let f = first_fn("fn it(): Int { 1 }");
    assert_eq!(f.name.node, "it");
    assert_eq!(f.params.len(), 0);
    assert_eq!(f.body.bindings.len(), 0);
    assert_eq!(f.body.result.node, Expr::Literal(Literal::Int(1)));
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
    // No semicolons — bindings are separated by whitespace; `val` keyword
    // is the sentinel that starts each binding.
    let f = first_fn("fn area(): Int { val w = 3 val h = 4 w * h }");
    assert_eq!(f.body.bindings.len(), 2);
    assert_eq!(f.body.bindings[0].name.node, "w");
    assert_eq!(f.body.bindings[1].name.node, "h");
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
fn rejects_empty_body() {
    assert!(parse("fn f(): Int { }").is_err());
}

#[test]
fn rejects_only_locals_no_result() {
    assert!(parse("fn f(): Int { val x = 1 }").is_err());
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
