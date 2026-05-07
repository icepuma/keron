//! Type checker for keron AST.
//!
//! Arithmetic operators (`+ - * / **` and unary `-`) require numeric
//! operands. Mixed `Int`/`Double` operands promote to `Double`; pure-Int
//! operands stay `Int`. `String`/`Boolean` in arithmetic is a type
//! error. Val annotations are strict — there is no implicit Int→Double
//! widening at the annotation site (use a Double literal or a Double
//! sub-expression to obtain one).

use crate::{
    ast::{BinOp, Expr, Item, Program, Spanned, StringPart, Type, UnaryOp},
    diagnostic::Diagnostic,
};

/// Validate that each `val` declaration's expression is well-typed
/// and matches its annotation when one is present.
///
/// # Errors
/// Returns one [`Diagnostic`] per failing declaration. A declaration's
/// first sub-expression error short-circuits subsequent checks for that
/// declaration; sibling declarations are still checked.
pub fn check(program: &Program) -> Result<(), Vec<Diagnostic>> {
    let mut diags = Vec::new();
    for item in &program.items {
        match item {
            Item::Val(v) => match expr_type(&v.value) {
                Err(d) => diags.push(d),
                Ok(got) => {
                    if let Some(annot) = &v.ty
                        && annot.node != got
                    {
                        diags.push(Diagnostic::new(
                            v.value.span.clone(),
                            format!(
                                "type mismatch: expected `{}`, found `{}`",
                                annot.node.name(),
                                got.name()
                            ),
                        ));
                    }
                }
            },
        }
    }
    if diags.is_empty() { Ok(()) } else { Err(diags) }
}

fn expr_type(e: &Spanned<Expr>) -> Result<Type, Diagnostic> {
    match &e.node {
        Expr::Literal(lit) => Ok(lit.type_of()),
        Expr::Unary { op, operand } => {
            let inner = expr_type(operand)?;
            match (op, inner) {
                (UnaryOp::Neg, Type::Int | Type::Double) => Ok(inner),
                (UnaryOp::Neg, t) => Err(Diagnostic::new(
                    e.span.clone(),
                    format!(
                        "unary `{}` requires `Int` or `Double`, found `{}`",
                        op.symbol(),
                        t.name()
                    ),
                )),
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let lt = expr_type(lhs)?;
            let rt = expr_type(rhs)?;
            arithmetic_result(*op, lt, rt).ok_or_else(|| {
                Diagnostic::new(
                    e.span.clone(),
                    format!(
                        "`{}` requires `Int` or `Double` operands, found `{}` and `{}`",
                        op.symbol(),
                        lt.name(),
                        rt.name()
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
    }
}

const fn arithmetic_result(_op: BinOp, lhs: Type, rhs: Type) -> Option<Type> {
    match (lhs, rhs) {
        (Type::Int, Type::Int) => Some(Type::Int),
        (Type::Double, Type::Double | Type::Int) | (Type::Int, Type::Double) => Some(Type::Double),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn check_src(src: &str) -> Result<(), Vec<Diagnostic>> {
        let prog = parse(src).expect("parse should succeed");
        check(&prog)
    }

    #[test]
    fn matching_string() {
        assert!(check_src(r#"val a: String = "hi""#).is_ok());
    }

    #[test]
    fn inferred_type_passes() {
        assert!(check_src(r#"val a = "hi""#).is_ok());
        assert!(check_src("val n = 1").is_ok());
        assert!(check_src("val b = true").is_ok());
        assert!(check_src("val d = 0.25").is_ok());
    }

    #[test]
    fn inferred_decls_skip_typecheck_even_alongside_mismatches() {
        let err = check_src("val a = 1\nval b: Int = \"x\"").expect_err("should fail");
        assert_eq!(err.len(), 1);
    }

    #[test]
    fn matching_int() {
        assert!(check_src("val a: Int = 1").is_ok());
    }

    #[test]
    fn matching_boolean() {
        assert!(check_src("val a: Boolean = true").is_ok());
    }

    #[test]
    fn matching_double() {
        assert!(check_src("val a: Double = 1.5").is_ok());
    }

    #[test]
    fn int_assigned_to_string() {
        let err = check_src("val a: String = 1").expect_err("should fail");
        assert_eq!(err.len(), 1);
        assert!(err[0].message.contains("expected `String`"));
        assert!(err[0].message.contains("found `Int`"));
    }

    #[test]
    fn double_assigned_to_int() {
        let err = check_src("val a: Int = 1.5").expect_err("should fail");
        assert!(err[0].message.contains("expected `Int`"));
    }

    #[test]
    fn boolean_assigned_to_double() {
        let err = check_src("val a: Double = true").expect_err("should fail");
        assert!(err[0].message.contains("expected `Double`"));
    }

    #[test]
    fn collects_multiple_errors() {
        let src = "val a: Int = \"x\"\nval b: String = 2";
        let err = check_src(src).expect_err("should fail");
        assert_eq!(err.len(), 2);
    }

    #[test]
    fn mismatch_span_points_at_value() {
        let src = "val a: Int = \"x\"";
        let err = check_src(src).expect_err("should fail");
        assert_eq!(&src[err[0].span.clone()], "\"x\"");
    }

    // ---------- arithmetic ----------

    #[test]
    fn int_plus_int_is_int() {
        assert!(check_src("val a: Int = 1 + 2").is_ok());
    }

    #[test]
    fn double_plus_double_is_double() {
        assert!(check_src("val a: Double = 1.0 + 2.5").is_ok());
    }

    #[test]
    fn int_plus_double_promotes_to_double() {
        assert!(check_src("val a: Double = 1 + 2.5").is_ok());
        assert!(check_src("val a: Double = 1.5 + 2").is_ok());
    }

    #[test]
    fn int_plus_double_does_not_satisfy_int_annotation() {
        let err = check_src("val a: Int = 1 + 2.5").expect_err("should fail");
        assert!(err[0].message.contains("expected `Int`"));
        assert!(err[0].message.contains("found `Double`"));
    }

    #[test]
    fn val_annotated_double_rejects_pure_int_expr() {
        // Strict: no implicit Int→Double widening at annotation site.
        let err = check_src("val a: Double = 1 + 2").expect_err("should fail");
        assert!(err[0].message.contains("expected `Double`"));
    }

    #[test]
    fn unary_neg_on_int_is_int() {
        assert!(check_src("val a: Int = -5").is_ok());
    }

    #[test]
    fn unary_neg_on_double_is_double() {
        assert!(check_src("val a: Double = -1.5").is_ok());
    }

    #[test]
    fn arithmetic_on_string_errors() {
        let err = check_src(r#"val a = "x" + 1"#).expect_err("should fail");
        assert!(err[0].message.contains('`'));
        assert!(err[0].message.contains("String"));
    }

    #[test]
    fn arithmetic_on_boolean_errors() {
        let err = check_src("val a = true + 1").expect_err("should fail");
        assert!(err[0].message.contains("Boolean"));
    }

    #[test]
    fn unary_neg_on_string_errors() {
        let err = check_src(r#"val a = -"x""#).expect_err("should fail");
        assert!(err[0].message.contains("String"));
    }

    #[test]
    fn all_operators_typecheck_int() {
        for op in ["+", "-", "*", "/", "**"] {
            let src = format!("val a: Int = 2 {op} 3");
            assert!(check_src(&src).is_ok(), "op {op} should be Int");
        }
    }

    #[test]
    fn parens_preserve_typing() {
        assert!(check_src("val a: Int = (1 + 2) * 3").is_ok());
        assert!(check_src("val a: Double = (1.0 + 2) * 3").is_ok());
    }

    #[test]
    fn nested_arithmetic_errors_at_offending_subexpr() {
        let err = check_src(r#"val a = 1 + ("x" * 2)"#).expect_err("should fail");
        assert_eq!(err.len(), 1);
    }

    // ---------- string interpolation ----------

    #[test]
    fn interpolation_typechecks_as_string() {
        assert!(check_src(r#"val a: String = "n = ${1 + 2}""#).is_ok());
        assert!(check_src(r#"val a = "${true} ${1.0 * 2}""#).is_ok());
    }

    #[test]
    fn interpolation_does_not_satisfy_int_annotation() {
        let err = check_src(r#"val n: Int = "x = ${1}""#).expect_err("should fail");
        assert!(err[0].message.contains("expected `Int`"));
        assert!(err[0].message.contains("found `String`"));
    }

    #[test]
    fn interpolation_inner_type_error_propagates() {
        let err = check_src(r#"val a = "${"x" + 1}""#).expect_err("should fail");
        assert_eq!(err.len(), 1);
        assert!(err[0].message.contains("String"));
    }

    #[test]
    fn nested_interpolations_all_typecheck() {
        assert!(check_src(r#"val a = "${"inner ${42}"}""#).is_ok());
    }
}
