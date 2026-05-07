//! Type checker for keron AST.
//!
//! Bidirectional: when a `val` carries an annotation, the expected
//! type is pushed down via [`check_expr`]; otherwise [`expr_type`]
//! synthesises bottom-up. Two constructs use the expected type
//! non-trivially: list literals (so `[]` or `[e1, …]` can be checked
//! against a `List<T>` annotation without first knowing `T`) and the
//! `++` operator (so `[] ++ [1]` works under a `List<Int>`
//! annotation). Every other node falls back to synth-then-equality.
//!
//! Arithmetic operators (`- * / **` and unary `-`) require numeric
//! operands. `+` is overloaded: numeric like the others, plus
//! `String + String → String`. `Boolean` in arithmetic is a type
//! error. Mixed `Int`/`Double` operands promote to `Double`. Val
//! annotations are strict — there is no implicit `Int→Double`
//! widening at the annotation site.
//!
//! Lists are strictly homogeneous: every element must have exactly
//! the same type (no `Int`→`Double` promotion within a list).
//!
//! `++` is list concat: both operands must be `List<T>` with the same
//! element type. There is no scalar-to-list lifting.

use crate::{
    ast::{BinOp, Expr, Item, Program, Spanned, StringPart, Type, UnaryOp},
    diagnostic::Diagnostic,
};

/// Validate every `val` declaration's expression and annotation.
///
/// # Errors
/// Returns one [`Diagnostic`] per failing declaration. Sub-expression
/// errors short-circuit the rest of *that* declaration; sibling
/// declarations are still checked.
pub fn check(program: &Program) -> Result<(), Vec<Diagnostic>> {
    let mut diags = Vec::new();
    for item in &program.items {
        match item {
            Item::Val(v) => {
                let result = v.ty.as_ref().map_or_else(
                    || expr_type(&v.value).map(drop),
                    |annot| check_expr(&v.value, &annot.node),
                );
                if let Err(d) = result {
                    diags.push(d);
                }
            }
        }
    }
    if diags.is_empty() { Ok(()) } else { Err(diags) }
}

/// Checking-mode judgment: verify `e` has type `expected`. Pushes the
/// expected type into list literals and `++` so empty-list contexts
/// resolve cleanly. Other nodes fall through to synth-then-equality.
fn check_expr(e: &Spanned<Expr>, expected: &Type) -> Result<(), Diagnostic> {
    match &e.node {
        Expr::List(items) => match expected {
            Type::List(elem_ty) => {
                for item in items {
                    check_expr(item, elem_ty)?;
                }
                Ok(())
            }
            _ if items.is_empty() => Err(Diagnostic::new(
                e.span.clone(),
                format!("type mismatch: expected `{expected}`, found empty list"),
            )),
            _ => switch_to_synth(e, expected),
        },
        Expr::Binary {
            op: BinOp::Concat,
            lhs,
            rhs,
        } if matches!(expected, Type::List(_)) => {
            check_expr(lhs, expected)?;
            check_expr(rhs, expected)?;
            Ok(())
        }
        _ => switch_to_synth(e, expected),
    }
}

fn switch_to_synth(e: &Spanned<Expr>, expected: &Type) -> Result<(), Diagnostic> {
    let got = expr_type(e)?;
    if &got == expected {
        Ok(())
    } else {
        Err(Diagnostic::new(
            e.span.clone(),
            format!("type mismatch: expected `{expected}`, found `{got}`"),
        ))
    }
}

fn expr_type(e: &Spanned<Expr>) -> Result<Type, Diagnostic> {
    match &e.node {
        Expr::Literal(lit) => Ok(lit.type_of()),
        Expr::Unary { op, operand } => {
            let inner = expr_type(operand)?;
            match (op, &inner) {
                (UnaryOp::Neg, Type::Int | Type::Double) => Ok(inner),
                (UnaryOp::Neg, t) => Err(Diagnostic::new(
                    e.span.clone(),
                    format!(
                        "unary `{}` requires `Int` or `Double`, found `{t}`",
                        op.symbol()
                    ),
                )),
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let lt = expr_type(lhs)?;
            let rt = expr_type(rhs)?;
            binop_result(*op, &lt, &rt)
                .ok_or_else(|| Diagnostic::new(e.span.clone(), binop_error(*op, &lt, &rt)))
        }
        Expr::Interpolation(parts) => {
            for part in parts {
                if let StringPart::Expr(inner) = part {
                    expr_type(inner)?;
                }
            }
            Ok(Type::String)
        }
        Expr::List(items) => list_type(e.span.clone(), items),
    }
}

fn list_type(list_span: crate::ast::Span, items: &[Spanned<Expr>]) -> Result<Type, Diagnostic> {
    let Some((first, rest)) = items.split_first() else {
        return Err(Diagnostic::new(
            list_span,
            "cannot infer type of empty list; add a `List<T>` annotation",
        ));
    };
    let elem_ty = expr_type(first)?;
    for item in rest {
        let ty = expr_type(item)?;
        if ty != elem_ty {
            return Err(Diagnostic::new(
                item.span.clone(),
                format!("list element type mismatch: expected `{elem_ty}`, found `{ty}`"),
            ));
        }
    }
    Ok(Type::List(Box::new(elem_ty)))
}

fn binop_result(op: BinOp, lhs: &Type, rhs: &Type) -> Option<Type> {
    match op {
        BinOp::Add => add_result(lhs, rhs),
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Pow => numeric_result(lhs, rhs),
        BinOp::Concat => concat_result(lhs, rhs),
    }
}

const fn add_result(lhs: &Type, rhs: &Type) -> Option<Type> {
    match (lhs, rhs) {
        (Type::String, Type::String) => Some(Type::String),
        _ => numeric_result(lhs, rhs),
    }
}

const fn numeric_result(lhs: &Type, rhs: &Type) -> Option<Type> {
    match (lhs, rhs) {
        (Type::Int, Type::Int) => Some(Type::Int),
        (Type::Double, Type::Double | Type::Int) | (Type::Int, Type::Double) => Some(Type::Double),
        _ => None,
    }
}

fn concat_result(lhs: &Type, rhs: &Type) -> Option<Type> {
    if let (Type::List(a), Type::List(b)) = (lhs, rhs)
        && a == b
    {
        Some(Type::List(a.clone()))
    } else {
        None
    }
}

fn binop_error(op: BinOp, lhs: &Type, rhs: &Type) -> String {
    let kind = match op {
        BinOp::Add => "`Int`, `Double`, or `String`",
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Pow => "`Int` or `Double`",
        BinOp::Concat => "matching `List<T>`",
    };
    format!(
        "`{}` requires {kind} operands, found `{lhs}` and `{rhs}`",
        op.symbol()
    )
}

#[cfg(test)]
mod tests;
