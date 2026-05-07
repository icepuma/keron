//! `reconcile` declaration parser tests.

use super::ok;
use crate::{
    ast::{Expr, Item, ReconcileDecl},
    parser::parse,
};

fn first_reconcile(src: &str) -> ReconcileDecl {
    let prog = ok(src);
    prog.items
        .into_iter()
        .find_map(|i| match i {
            Item::Reconcile(r) => Some(r),
            _ => None,
        })
        .expect("expected at least one reconcile item")
}

#[test]
fn reconcile_call_expr_parses() {
    let r = first_reconcile(r#"reconcile symlink(from = "a", to = "b")"#);
    assert!(matches!(r.expr.node, Expr::Call { .. }));
}

#[test]
fn reconcile_list_parses() {
    let r = first_reconcile(
        r#"reconcile [symlink(from = "a", to = "b"), symlink(from = "c", to = "d")]"#,
    );
    let Expr::List(items) = r.expr.node else {
        panic!("expected list");
    };
    assert_eq!(items.len(), 2);
}

#[test]
fn reconcile_var_parses() {
    let r = first_reconcile("val x: Symlink = symlink(from = \"a\", to = \"b\")\nreconcile x");
    assert!(matches!(r.expr.node, Expr::Var(_)));
}

#[test]
fn reconcile_user_fn_call_parses() {
    let src = "
        fn make(): Symlink {
            symlink(from = \"a\", to = \"b\")
        }
        reconcile make()
    ";
    let r = first_reconcile(src);
    assert!(matches!(r.expr.node, Expr::Call { .. }));
}

#[test]
fn rejects_reconcile_without_expr() {
    assert!(parse("reconcile").is_err());
}

#[test]
fn rejects_val_named_reconcile() {
    assert!(parse("val reconcile = 1").is_err());
}

#[test]
fn rejects_fn_named_reconcile() {
    assert!(parse("fn reconcile(): Int { 1 }").is_err());
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
fn multiple_reconcile_decls_parse() {
    let prog = ok("
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: File = file(path = \"p\", content = \"c\")
        reconcile a
        reconcile b
    ");
    assert_eq!(prog.items.len(), 4);
    let reconcile_count = prog
        .items
        .iter()
        .filter(|i| matches!(i, Item::Reconcile(_)))
        .count();
    assert_eq!(reconcile_count, 2);
}
