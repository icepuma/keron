//! `if`/`else` expression parser tests.

use super::expr_of;
use crate::{
    ast::{Expr, Literal},
    parser::parse,
};

fn unwrap_if(e: Expr) -> (Expr, Expr, Expr) {
    let Expr::If {
        cond,
        then_branch,
        else_branch,
    } = e
    else {
        panic!("expected if expression");
    };
    (cond.node, then_branch.node, else_branch.node)
}

#[test]
fn simple_if_else_parses() {
    let e = expr_of("val r = if true { 1 } else { 2 }");
    let (cond, then_b, else_b) = unwrap_if(e.node);
    assert_eq!(cond, Expr::Literal(Literal::Boolean(true)));
    assert_eq!(then_b, Expr::Literal(Literal::Int(1)));
    assert_eq!(else_b, Expr::Literal(Literal::Int(2)));
}

#[test]
fn if_else_with_string_branches() {
    let e = expr_of(r#"val r = if false { "a" } else { "b" }"#);
    assert!(matches!(e.node, Expr::If { .. }));
}

#[test]
fn else_if_chain_is_right_associative() {
    // `if a { 1 } else if b { 2 } else { 3 }` — the else branch of the
    // outer if is itself another if.
    let e = expr_of("val r = if true { 1 } else if false { 2 } else { 3 }");
    let (_, _, outer_else) = unwrap_if(e.node);
    assert!(
        matches!(outer_else, Expr::If { .. }),
        "outer else should be a nested if"
    );
}

#[test]
fn deeply_chained_else_if() {
    let src = "val r = if true { 1 } else if true { 2 } else if true { 3 } else { 4 }";
    let e = expr_of(src);
    // Walk down else branches: each should be an If until the final.
    let mut current = e.node;
    let mut depth = 0;
    while let Expr::If { else_branch, .. } = current {
        depth += 1;
        current = else_branch.node;
    }
    // 3 nested ifs (the else-ifs); final `else { 4 }` is a literal.
    assert_eq!(depth, 3);
    assert_eq!(current, Expr::Literal(Literal::Int(4)));
}

#[test]
fn if_inside_arithmetic() {
    // `1 + if true { 2 } else { 3 }` — the if is the rhs atom of `+`.
    let e = expr_of("val r = 1 + if true { 2 } else { 3 }");
    let Expr::Binary { rhs, .. } = e.node else {
        panic!("expected binary");
    };
    assert!(matches!(rhs.node, Expr::If { .. }));
}

#[test]
fn if_with_arithmetic_branches() {
    let e = expr_of("val r = if true { 1 + 2 } else { 3 * 4 }");
    let (_, then_b, else_b) = unwrap_if(e.node);
    assert!(matches!(then_b, Expr::Binary { .. }));
    assert!(matches!(else_b, Expr::Binary { .. }));
}

#[test]
fn if_with_function_call_branches() {
    let src = r"
        fn one(): Int { 1 }
        fn two(): Int { 2 }
        val r = if true { one() } else { two() }
    ";
    assert!(parse(src).is_ok());
}

#[test]
fn if_in_fn_body() {
    let src = "fn pick(b: Boolean): Int { if b { 1 } else { 0 } }";
    assert!(parse(src).is_ok());
}

#[test]
fn nested_if_in_then_branch() {
    let e = expr_of("val r = if true { if false { 1 } else { 2 } } else { 3 }");
    let (_, then_b, _) = unwrap_if(e.node);
    assert!(matches!(then_b, Expr::If { .. }));
}

#[test]
fn rejects_if_without_else() {
    assert!(parse("val r = if true { 1 }").is_err());
}

#[test]
fn rejects_if_without_then_block() {
    assert!(parse("val r = if true 1 else { 2 }").is_err());
}

#[test]
fn rejects_if_without_else_block() {
    assert!(parse("val r = if true { 1 } else 2").is_err());
}

#[test]
fn rejects_empty_then_block() {
    assert!(parse("val r = if true { } else { 2 }").is_err());
}

#[test]
fn rejects_val_named_if() {
    assert!(parse("val if = 1").is_err());
}

#[test]
fn rejects_val_named_else() {
    assert!(parse("val else = 1").is_err());
}

#[test]
fn rejects_fn_named_if() {
    assert!(parse("fn if(): Int { 1 }").is_err());
}
