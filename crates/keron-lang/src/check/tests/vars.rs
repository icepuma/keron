//! Variable references, scope, and duplicate detection.

use super::check_src;

#[test]
fn var_resolves_to_prior_binding() {
    assert!(check_src("val a = 1\nval b: Int = a").is_ok());
    assert!(check_src("val s = \"hi\"\nval greeting: String = s").is_ok());
}

#[test]
fn var_in_arithmetic() {
    assert!(check_src("val a: Int = 3\nval b: Int = a * 2 + 1").is_ok());
}

#[test]
fn var_in_list_literal() {
    assert!(check_src("val n = 5\nval xs: List<Int> = [n, n + 1, n * 2]").is_ok());
}

#[test]
fn var_in_concat() {
    assert!(check_src("val a: List<Int> = [1, 2]\nval b: List<Int> = a ++ [3]").is_ok());
}

#[test]
fn var_in_interpolation() {
    assert!(check_src("val n = 42\nval s: String = \"answer = ${n}\"").is_ok());
}

#[test]
fn unknown_var_errors() {
    let err = check_src("val a = nope").expect_err("should fail");
    assert!(err[0].message.contains("unknown variable"));
    assert!(err[0].message.contains("nope"));
}

#[test]
fn forward_ref_errors() {
    let err = check_src("val a = b\nval b = 1").expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("unknown variable")));
}

#[test]
fn duplicate_val_errors() {
    let err = check_src("val a = 1\nval a = 2").expect_err("should fail");
    assert!(err[0].message.contains("already defined"));
    assert!(err[0].message.contains('a'));
}

#[test]
fn duplicate_val_does_not_cascade() {
    let err = check_src("val a = 1\nval a = 2\nval b: Int = a").expect_err("should fail");
    assert_eq!(err.len(), 1);
    assert!(err[0].message.contains("already defined"));
}

#[test]
fn annotated_failed_decl_still_binds() {
    // `val a: Int = "x"` fails, but `a` is bound at `Int` so downstream
    // refs don't cascade into "unknown variable".
    let err = check_src("val a: Int = \"x\"\nval b: Int = a + 1").expect_err("should fail");
    assert_eq!(err.len(), 1);
    assert!(err[0].message.contains("expected `Int`"));
}

#[test]
fn var_type_mismatch() {
    let err = check_src("val n: Int = 1\nval s: String = n").expect_err("should fail");
    assert!(err[0].message.contains("expected `String`"));
    assert!(err[0].message.contains("found `Int`"));
}
