//! Call expression parser tests.

use super::expr_of;
use crate::{
    ast::{Expr, Literal},
    parser::parse,
};

fn first_call(src: &str) -> (String, Vec<crate::ast::CallArg>) {
    let e = expr_of(src);
    match e.node {
        Expr::Call { callee, args } => (callee.node, args),
        other => panic!("expected call, got {other:?}"),
    }
}

#[test]
fn call_no_args() {
    let (name, args) = first_call("val x = f()");
    assert_eq!(name, "f");
    assert_eq!(args.len(), 0);
}

#[test]
fn call_positional() {
    let (_, args) = first_call("val x = f(1, 2, 3)");
    assert_eq!(args.len(), 3);
    for a in &args {
        assert!(a.name.is_none());
    }
}

#[test]
fn call_named() {
    let (_, args) = first_call("val x = f(a = 1, b = 2)");
    assert_eq!(args.len(), 2);
    assert_eq!(args[0].name.as_ref().unwrap().node, "a");
    assert_eq!(args[1].name.as_ref().unwrap().node, "b");
}

#[test]
fn call_mixed() {
    let (_, args) = first_call("val x = f(1, b = 2)");
    assert_eq!(args.len(), 2);
    assert!(args[0].name.is_none());
    assert_eq!(args[1].name.as_ref().unwrap().node, "b");
}

#[test]
fn call_trailing_comma() {
    assert!(parse("val x = f(1, 2,)").is_ok());
    assert!(parse("val x = f(a = 1,)").is_ok());
}

#[test]
fn call_with_complex_arg() {
    let (_, args) = first_call("val x = f(1 + 2 * 3, [a, b])");
    assert_eq!(args.len(), 2);
}

#[test]
fn nested_calls() {
    let (_, args) = first_call("val x = f(g(1), h(2, 3))");
    assert_eq!(args.len(), 2);
    let Expr::Call { .. } = &args[0].value.node else {
        panic!("expected inner call");
    };
}

#[test]
fn rejects_named_lhs_not_ident() {
    assert!(parse("val x = f(1 + 2 = 3)").is_err());
}

#[test]
fn ident_alone_is_var_not_call() {
    let prog = parse("val x = a").expect("parse");
    let crate::ast::Item::Val(v) = &prog.items[0] else {
        panic!()
    };
    assert_eq!(v.value.node, Expr::Var("a".into()));
}

#[test]
fn call_inside_expression() {
    // f(1) + g(2) — both are calls combined with `+`.
    let e = expr_of("val x = f(1) + g(2)");
    let Expr::Binary { lhs, rhs, .. } = e.node else {
        panic!("expected binary at top");
    };
    let Expr::Call { .. } = lhs.node else {
        panic!("expected call on lhs");
    };
    let Expr::Call { .. } = rhs.node else {
        panic!("expected call on rhs");
    };
}

#[test]
fn call_in_list() {
    let e = expr_of("val x = [f(1), f(2)]");
    let Expr::List(items) = e.node else {
        panic!("expected list");
    };
    assert_eq!(items.len(), 2);
    let Expr::Call { .. } = &items[0].node else {
        panic!("expected call in list");
    };
}

#[test]
fn call_with_string_arg() {
    let (_, args) = first_call(r#"val x = greet("hello")"#);
    assert_eq!(args.len(), 1);
    assert_eq!(
        args[0].value.node,
        Expr::Literal(Literal::String("hello".into()))
    );
}
