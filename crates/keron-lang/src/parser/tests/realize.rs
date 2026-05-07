//! `realize` declaration parser tests.

use super::ok;
use crate::{
    ast::{Expr, Item, RealizeDecl},
    parser::parse,
};

fn first_realize(src: &str) -> RealizeDecl {
    let prog = ok(src);
    prog.items
        .into_iter()
        .find_map(|i| match i {
            Item::Realize(r) => Some(r),
            _ => None,
        })
        .expect("expected at least one realize item")
}

#[test]
fn realize_call_expr_parses() {
    let r = first_realize(r#"realize symlink(from = "a", to = "b")"#);
    assert!(matches!(r.expr.node, Expr::Call { .. }));
}

#[test]
fn realize_list_parses() {
    let r =
        first_realize(r#"realize [symlink(from = "a", to = "b"), symlink(from = "c", to = "d")]"#);
    let Expr::List(items) = r.expr.node else {
        panic!("expected list");
    };
    assert_eq!(items.len(), 2);
}

#[test]
fn realize_var_parses() {
    let r = first_realize("val x: Symlink = symlink(from = \"a\", to = \"b\")\nrealize x");
    assert!(matches!(r.expr.node, Expr::Var(_)));
}

#[test]
fn realize_user_fn_call_parses() {
    let src = "
        fn make(): Symlink {
            symlink(from = \"a\", to = \"b\")
        }
        realize make()
    ";
    let r = first_realize(src);
    assert!(matches!(r.expr.node, Expr::Call { .. }));
}

#[test]
fn rejects_realize_without_expr() {
    assert!(parse("realize").is_err());
}

#[test]
fn rejects_val_named_realize() {
    assert!(parse("val realize = 1").is_err());
}

#[test]
fn rejects_fn_named_realize() {
    assert!(parse("fn realize(): Int { 1 }").is_err());
}

#[test]
fn rejects_val_named_capitalized_type() {
    assert!(parse("val Symlink = 1").is_err());
    assert!(parse("val File = 1").is_err());
    assert!(parse("val Directory = 1").is_err());
}

#[test]
fn rejects_fn_named_capitalized_type() {
    assert!(parse("fn Symlink(): Int { 1 }").is_err());
}

#[test]
fn multiple_realize_decls_parse() {
    let prog = ok("
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: File = file(path = \"p\", content = \"c\")
        realize a
        realize b
    ");
    assert_eq!(prog.items.len(), 4);
    let realize_count = prog
        .items
        .iter()
        .filter(|i| matches!(i, Item::Realize(_)))
        .count();
    assert_eq!(realize_count, 2);
}
