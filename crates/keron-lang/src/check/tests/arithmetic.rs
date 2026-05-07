//! Arithmetic + concat (string and list) typing.

use super::check_src;

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
fn string_plus_int_errors() {
    // `+` is overloaded: String+String works, but String+Int still doesn't.
    let err = check_src(r#"val a = "x" + 1"#).expect_err("should fail");
    assert!(err[0].message.contains("String"));
    assert!(err[0].message.contains("Int"));
}

#[test]
fn string_plus_string_concatenates() {
    assert!(check_src(r#"val s: String = "hello" + " " + "world""#).is_ok());
    assert!(check_src(r#"val s = "a" + "b""#).is_ok());
}

#[test]
fn string_concat_does_not_satisfy_int_annotation() {
    let err = check_src(r#"val n: Int = "a" + "b""#).expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `String`"));
}

#[test]
fn boolean_plus_string_errors() {
    let err = check_src(r#"val a = true + "x""#).expect_err("should fail");
    assert!(err[0].message.contains("Boolean"));
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
