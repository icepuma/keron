//! Operator precedence + paren-need predicates for the pretty-printer.
//!
//! The numeric levels mirror the grammar in
//! [`crate::parser::expr`] (PEMDAS-style ordering); the higher the
//! number, the tighter the operator binds. The formatter uses these
//! only to decide whether an operand needs surrounding parentheses
//! to preserve parse equivalence — it never inserts parens for
//! "readability". Users who want extra parens for their own reasons
//! can keep them by re-running the formatter after editing.

use crate::ast::{BinOp, Expr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    /// Used for the operand of a unary operator. Acts like the right
    /// side of a right-associative operator for precedence purposes.
    Unary,
}

/// Higher = tighter binding. Levels match the grammar comment at the
/// top of `crate::parser::expr`:
///
/// 1. `||`
/// 2. `&&`
/// 3. comparisons (`==`, `!=`, `<`, `<=`, `>`, `>=`)
/// 4. `??` (right-associative)
/// 5. additive (`+`, `-`, `++`)
/// 6. multiplicative (`*`, `/`)
/// 7. unary `-`
/// 8. `**` (right-associative)
#[must_use]
pub const fn binop_prec(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 3,
        BinOp::Coalesce => 4,
        BinOp::Add | BinOp::Sub | BinOp::Concat => 5,
        BinOp::Mul | BinOp::Div => 6,
        BinOp::Pow => 8,
    }
}

pub const UNARY_PREC: u8 = 7;

#[must_use]
pub const fn is_right_assoc(op: BinOp) -> bool {
    matches!(op, BinOp::Coalesce | BinOp::Pow)
}

/// Returns `true` if `child` needs surrounding parentheses when it
/// appears as the `side` operand of a parent binary operator at
/// precedence `parent_prec` (left-associative unless
/// `parent_right_assoc` is set).
///
/// Rules:
/// - Left operand of a left-assoc op: parens if child prec < parent.
/// - Right operand of a left-assoc op: parens if child prec ≤ parent.
/// - Left operand of a right-assoc op: parens if child prec ≤ parent.
/// - Right operand of a right-assoc op: parens if child prec < parent.
/// - Operand of a unary op: parens if child is a `Binary` with prec
///   ≤ unary prec (so `-(a + b)` is preserved).
#[must_use]
pub const fn child_needs_parens(
    child: &Expr,
    parent_prec: u8,
    parent_right_assoc: bool,
    side: Side,
) -> bool {
    match child {
        Expr::Binary { op, .. } => {
            let cp = binop_prec(*op);
            let strict = match side {
                Side::Left => parent_right_assoc,
                Side::Right => !parent_right_assoc,
                Side::Unary => cp <= UNARY_PREC,
            };
            if matches!(side, Side::Unary) {
                strict
            } else if strict {
                cp <= parent_prec
            } else {
                cp < parent_prec
            }
        }
        Expr::Unary { .. } if matches!(side, Side::Left) => {
            // `-x ** 2` parses as `-(x ** 2)` because `**` accepts a
            // unary on its RHS but not LHS. Wrapping a unary on the
            // LHS of any binary is never strictly required — `-a + b`
            // is `(-a) + b` already. So: no parens for unary on the
            // left of a binary.
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Literal, Span, Spanned};

    fn lit_int(n: i64) -> Spanned<Expr> {
        Spanned {
            node: Expr::Literal(Literal::Int(n)),
            span: Span::default(),
        }
    }

    fn bin(op: BinOp, l: Spanned<Expr>, r: Spanned<Expr>) -> Expr {
        Expr::Binary {
            op,
            lhs: Box::new(l),
            rhs: Box::new(r),
        }
    }

    #[test]
    fn additive_inside_multiplicative_left_needs_parens() {
        // (a + b) * c — left child Add inside parent Mul.
        let child = bin(BinOp::Add, lit_int(1), lit_int(2));
        assert!(child_needs_parens(
            &child,
            binop_prec(BinOp::Mul),
            false,
            Side::Left,
        ));
    }

    #[test]
    fn multiplicative_inside_additive_no_parens() {
        // a + (b * c) — Mul as right child of Add doesn't need parens.
        let child = bin(BinOp::Mul, lit_int(2), lit_int(3));
        assert!(!child_needs_parens(
            &child,
            binop_prec(BinOp::Add),
            false,
            Side::Right,
        ));
    }

    #[test]
    fn coalesce_right_assoc_left_child_needs_parens() {
        // (a ?? b) ?? c — `??` is right-assoc, so the left child at
        // equal prec needs parens to preserve grouping.
        let child = bin(BinOp::Coalesce, lit_int(1), lit_int(2));
        assert!(child_needs_parens(
            &child,
            binop_prec(BinOp::Coalesce),
            true,
            Side::Left,
        ));
    }

    #[test]
    fn coalesce_right_assoc_right_child_at_equal_prec_no_parens() {
        // a ?? b ?? c → a ?? (b ?? c). Right child is Coalesce at
        // equal prec; right-assoc means no parens needed.
        let child = bin(BinOp::Coalesce, lit_int(2), lit_int(3));
        assert!(!child_needs_parens(
            &child,
            binop_prec(BinOp::Coalesce),
            true,
            Side::Right,
        ));
    }

    #[test]
    fn additive_inside_unary_needs_parens() {
        // -(a + b) — unary operand Add binds looser than unary.
        let child = bin(BinOp::Add, lit_int(1), lit_int(2));
        assert!(child_needs_parens(&child, UNARY_PREC, false, Side::Unary,));
    }

    #[test]
    fn pow_inside_unary_no_parens() {
        // `-x ** 2` is `-(x ** 2)` per grammar — when emitting Pow as
        // the operand of Neg, we don't add parens because the
        // resulting `-x ** 2` re-parses to the same tree.
        let child = bin(BinOp::Pow, lit_int(2), lit_int(3));
        assert!(!child_needs_parens(&child, UNARY_PREC, false, Side::Unary,));
    }
}
