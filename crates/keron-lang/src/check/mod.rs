//! Type checker for keron AST.
//!
//! Arithmetic operators (`+ - * / **` and unary `-`) require numeric
//! operands. Mixed `Int`/`Double` operands promote to `Double`; pure-Int
//! operands stay `Int`. `String`/`Boolean` in arithmetic is a type
//! error. Val annotations are strict — there is no implicit Int→Double
//! widening at the annotation site (use a Double literal or a Double
//! sub-expression to obtain one).
//!
//! Lists are strictly homogeneous: every element must have exactly the
//! same type (no `Int`→`Double` promotion within a list). An empty
//! list literal carries no element type and therefore requires a
//! `List<T>` annotation on its containing `val`.

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
            Item::Val(v) => check_val(&v.value, v.ty.as_ref(), &mut diags),
        }
    }
    if diags.is_empty() { Ok(()) } else { Err(diags) }
}

fn check_val(value: &Spanned<Expr>, annot: Option<&Spanned<Type>>, diags: &mut Vec<Diagnostic>) {
    // Empty list bypass: an empty `[]` cannot be synthesised, so it
    // takes its element type from a `List<_>` annotation.
    if let (Expr::List(items), Some(annot)) = (&value.node, annot)
        && items.is_empty()
    {
        if !matches!(annot.node, Type::List(_)) {
            diags.push(Diagnostic::new(
                value.span.clone(),
                format!("type mismatch: expected `{}`, found empty list", annot.node),
            ));
        }
        return;
    }

    match expr_type(value) {
        Err(d) => diags.push(d),
        Ok(got) => {
            if let Some(annot) = annot
                && annot.node != got
            {
                diags.push(Diagnostic::new(
                    value.span.clone(),
                    format!("type mismatch: expected `{}`, found `{}`", annot.node, got),
                ));
            }
        }
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
            arithmetic_result(*op, &lt, &rt).ok_or_else(|| {
                Diagnostic::new(
                    e.span.clone(),
                    format!(
                        "`{}` requires `Int` or `Double` operands, found `{lt}` and `{rt}`",
                        op.symbol()
                    ),
                )
            })
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

const fn arithmetic_result(_op: BinOp, lhs: &Type, rhs: &Type) -> Option<Type> {
    match (lhs, rhs) {
        (Type::Int, Type::Int) => Some(Type::Int),
        (Type::Double, Type::Double | Type::Int) | (Type::Int, Type::Double) => Some(Type::Double),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
