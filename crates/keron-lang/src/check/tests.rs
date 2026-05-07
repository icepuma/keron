//! Type checker unit tests.

use super::check;
use crate::{diagnostic::Diagnostic, parser::parse};

fn check_src(src: &str) -> Result<(), Vec<Diagnostic>> {
    let prog = parse(src).expect("parse should succeed");
    check(&prog)
}

// ---------- baseline literal/annotation matching ----------

#[test]
fn matching_string() {
    assert!(check_src(r#"val a: String = "hi""#).is_ok());
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
    let err = check_src("val a: Int = \"x\"\nval b: String = 2").expect_err("should fail");
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

// ---------- lists ----------

#[test]
fn list_int_typechecks() {
    assert!(check_src("val xs: List<Int> = [1, 2, 3]").is_ok());
}

#[test]
fn list_inferred_int() {
    assert!(check_src("val xs = [1, 2, 3]").is_ok());
}

#[test]
fn empty_list_with_annotation_typechecks() {
    assert!(check_src("val xs: List<Int> = []").is_ok());
    assert!(check_src("val xs: List<List<String>> = []").is_ok());
}

#[test]
fn empty_list_without_annotation_errors() {
    let err = check_src("val xs = []").expect_err("should fail");
    assert!(err[0].message.contains("empty list"));
}

#[test]
fn empty_list_with_non_list_annotation_errors() {
    let err = check_src("val xs: Int = []").expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("empty list"));
}

#[test]
fn heterogeneous_list_errors() {
    let err = check_src(r#"val xs = [1, "x"]"#).expect_err("should fail");
    assert!(err[0].message.contains("list element type mismatch"));
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `String`"));
}

#[test]
fn list_does_not_promote_int_to_double() {
    // Strict element typing: no promotion within a list.
    let err = check_src("val xs = [1, 2.5]").expect_err("should fail");
    assert!(err[0].message.contains("Int"));
    assert!(err[0].message.contains("Double"));
}

#[test]
fn list_annotation_mismatch() {
    let err = check_src(r"val xs: List<String> = [1]").expect_err("should fail");
    assert!(err[0].message.contains("expected `List<String>`"));
    assert!(err[0].message.contains("found `List<Int>`"));
}

#[test]
fn nested_list_typechecks() {
    assert!(check_src("val xs: List<List<Int>> = [[1, 2], [3]]").is_ok());
}

#[test]
fn nested_list_inferred() {
    assert!(check_src("val xs = [[1, 2], [3]]").is_ok());
}

#[test]
fn list_of_expressions_typechecks() {
    assert!(check_src("val xs: List<Int> = [1 + 2, 3 * 4, -5]").is_ok());
}

#[test]
fn list_with_inner_type_error() {
    let err = check_src(r#"val xs = ["a" + 1]"#).expect_err("should fail");
    assert!(err[0].message.contains("String"));
}
