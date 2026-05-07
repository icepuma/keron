//! Primitive literal + annotation matching baseline.

use super::check_src;

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
