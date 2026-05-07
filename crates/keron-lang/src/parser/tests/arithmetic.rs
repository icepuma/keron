//! PEMDAS / arithmetic precedence tests.

use super::expr_of;
use crate::{
    ast::{BinOp, Expr, Literal, Spanned, UnaryOp},
    parser::parse,
};

fn binop(e: &Expr) -> (BinOp, &Spanned<Expr>, &Spanned<Expr>) {
    let Expr::Binary { op, lhs, rhs } = e else {
        panic!("expected binary, got {e:?}");
    };
    (*op, lhs, rhs)
}

#[test]
fn add_two_ints() {
    let e = expr_of("val a = 1 + 2");
    let (op, lhs, rhs) = binop(&e.node);
    assert_eq!(op, BinOp::Add);
    assert_eq!(lhs.node, Expr::Literal(Literal::Int(1)));
    assert_eq!(rhs.node, Expr::Literal(Literal::Int(2)));
}

#[test]
fn add_left_associative() {
    // 1 + 2 + 3 = ((1 + 2) + 3)
    let e = expr_of("val a = 1 + 2 + 3");
    let (op, lhs, rhs) = binop(&e.node);
    assert_eq!(op, BinOp::Add);
    assert_eq!(rhs.node, Expr::Literal(Literal::Int(3)));
    let (lop, _, _) = binop(&lhs.node);
    assert_eq!(lop, BinOp::Add);
}

#[test]
fn mul_binds_tighter_than_add() {
    // 1 + 2 * 3 = 1 + (2 * 3)
    let e = expr_of("val a = 1 + 2 * 3");
    let (op, lhs, rhs) = binop(&e.node);
    assert_eq!(op, BinOp::Add);
    assert_eq!(lhs.node, Expr::Literal(Literal::Int(1)));
    let (rop, _, _) = binop(&rhs.node);
    assert_eq!(rop, BinOp::Mul);
}

#[test]
fn parens_override_precedence() {
    let e = expr_of("val a = (1 + 2) * 3");
    let (op, lhs, rhs) = binop(&e.node);
    assert_eq!(op, BinOp::Mul);
    assert_eq!(rhs.node, Expr::Literal(Literal::Int(3)));
    let (lop, _, _) = binop(&lhs.node);
    assert_eq!(lop, BinOp::Add);
}

#[test]
fn power_right_associative() {
    let e = expr_of("val a = 2 ** 3 ** 2");
    let (op, _, rhs) = binop(&e.node);
    assert_eq!(op, BinOp::Pow);
    let (rop, _, _) = binop(&rhs.node);
    assert_eq!(rop, BinOp::Pow);
}

#[test]
fn power_binds_tighter_than_unary_minus() {
    // -2 ** 2 = -(2 ** 2), per Python convention
    let e = expr_of("val a = -2 ** 2");
    let Expr::Unary { op, operand } = e.node else {
        panic!("expected unary at top");
    };
    assert_eq!(op, UnaryOp::Neg);
    let (rop, _, _) = binop(&operand.node);
    assert_eq!(rop, BinOp::Pow);
}

#[test]
fn power_rhs_can_be_unary() {
    let e = expr_of("val a = 2 ** -3");
    let (op, _, rhs) = binop(&e.node);
    assert_eq!(op, BinOp::Pow);
    let Expr::Unary { .. } = rhs.node else {
        panic!("expected unary on RHS");
    };
}

#[test]
fn parens_unary_then_pow() {
    let e = expr_of("val a = (-2) ** 3");
    let (op, lhs, _) = binop(&e.node);
    assert_eq!(op, BinOp::Pow);
    let Expr::Unary { .. } = lhs.node else {
        panic!("expected unary inside parens");
    };
}

#[test]
fn double_negation() {
    let e = expr_of("val a = --5");
    let Expr::Unary { op, operand } = e.node else {
        panic!("outer unary");
    };
    assert_eq!(op, UnaryOp::Neg);
    let Expr::Unary { .. } = operand.node else {
        panic!("inner unary");
    };
}

#[test]
fn whitespace_around_operators() {
    assert!(parse("val a = 1+2*3-4/5**6").is_ok());
    assert!(parse("val a =  1   +   2  ").is_ok());
    assert!(parse("val a =\n  1\n  + 2\n").is_ok());
}

#[test]
fn rejects_trailing_operator() {
    assert!(parse("val a = 1 +").is_err());
}

#[test]
fn rejects_double_operator() {
    assert!(parse("val a = 1 + + 2").is_err());
}

#[test]
fn rejects_unmatched_paren() {
    assert!(parse("val a = (1 + 2").is_err());
    assert!(parse("val a = 1 + 2)").is_err());
}

// ---------- ++ (concat) ----------

#[test]
fn double_plus_parses_as_concat() {
    let e = expr_of("val a = [1] ++ [2]");
    let (op, _, _) = binop(&e.node);
    assert_eq!(op, BinOp::Concat);
}

#[test]
fn concat_left_associative() {
    // [1] ++ [2] ++ [3] = (([1] ++ [2]) ++ [3])
    let e = expr_of("val a = [1] ++ [2] ++ [3]");
    let (op, lhs, _) = binop(&e.node);
    assert_eq!(op, BinOp::Concat);
    let (lop, _, _) = binop(&lhs.node);
    assert_eq!(lop, BinOp::Concat);
}

#[test]
fn plus_disambiguates_from_double_plus() {
    // Single `+` between two ints is still Add, not Concat.
    let e = expr_of("val a = 1 + 2");
    let (op, _, _) = binop(&e.node);
    assert_eq!(op, BinOp::Add);
}

#[test]
fn concat_at_same_precedence_as_plus() {
    // 1 + 2 ++ [3] = (1 + 2) ++ [3]   (left-assoc, same precedence)
    let e = expr_of("val a = 1 + 2 ++ [3]");
    let (op, lhs, _) = binop(&e.node);
    assert_eq!(op, BinOp::Concat);
    let (lop, _, _) = binop(&lhs.node);
    assert_eq!(lop, BinOp::Add);
}
