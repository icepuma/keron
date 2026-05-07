//! Comparison operator parser tests.

use super::expr_of;
use crate::ast::{BinOp, Expr};

fn binop_of(src: &str) -> BinOp {
    let e = expr_of(src);
    let Expr::Binary { op, .. } = e.node else {
        panic!("expected binary expression");
    };
    op
}

#[test]
fn eq_parses() {
    assert_eq!(binop_of("val r = 1 == 2"), BinOp::Eq);
}

#[test]
fn neq_parses() {
    assert_eq!(binop_of("val r = 1 != 2"), BinOp::Neq);
}

#[test]
fn lt_parses() {
    assert_eq!(binop_of("val r = 1 < 2"), BinOp::Lt);
}

#[test]
fn le_parses() {
    assert_eq!(binop_of("val r = 1 <= 2"), BinOp::Le);
}

#[test]
fn gt_parses() {
    assert_eq!(binop_of("val r = 1 > 2"), BinOp::Gt);
}

#[test]
fn ge_parses() {
    assert_eq!(binop_of("val r = 1 >= 2"), BinOp::Ge);
}

#[test]
fn comparison_binds_looser_than_addition() {
    // `1 + 2 < 3` parses as `(1 + 2) < 3`, so the outer op is `<`.
    assert_eq!(binop_of("val r = 1 + 2 < 3"), BinOp::Lt);
}

#[test]
fn comparison_binds_looser_than_multiplication() {
    assert_eq!(binop_of("val r = 1 * 2 == 2"), BinOp::Eq);
}

#[test]
fn comparison_le_not_eaten_as_lt_then_eq() {
    // If parser tried `<` before `<=`, `a <= b` would parse as `(a <
    // (= b))` and fail. Asserting the result is `<=` proves the
    // longest-token-first ordering works.
    assert_eq!(binop_of("val r = 1 <= 2"), BinOp::Le);
}

#[test]
fn comparison_ge_not_eaten_as_gt_then_eq() {
    assert_eq!(binop_of("val r = 1 >= 2"), BinOp::Ge);
}

#[test]
fn comparison_in_if_cond() {
    let e = expr_of("val r = if 1 < 2 { 3 } else { 4 }");
    let Expr::If { cond, .. } = e.node else {
        panic!("expected if");
    };
    assert!(matches!(cond.node, Expr::Binary { op: BinOp::Lt, .. }));
}

#[test]
fn string_comparison_parses() {
    assert_eq!(binop_of(r#"val r = "a" < "b""#), BinOp::Lt);
}

#[test]
fn boolean_equality_parses() {
    assert_eq!(binop_of("val r = true == false"), BinOp::Eq);
}

#[test]
fn parenthesized_comparison_parses() {
    let e = expr_of("val r = (1 < 2)");
    assert!(matches!(e.node, Expr::Binary { op: BinOp::Lt, .. }));
}
