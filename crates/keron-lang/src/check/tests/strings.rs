//! String interpolation typing.

use super::check_src;

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

#[test]
fn interpolating_a_list_is_a_check_error() {
    let src = "val xs: List<String> = [\"a\", \"b\"]\nval s: String = \"items: ${xs}\"";
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("cannot interpolate"));
    assert!(err[0].message.contains("List"));
}

#[test]
fn interpolating_a_map_is_a_check_error() {
    let src = "val m: Map<String, Int> = {\"a\": 1}\nval s: String = \"${m}\"";
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("cannot interpolate"));
}

#[test]
fn interpolating_int_bool_double_typechecks() {
    assert!(check_src(r#"val s: String = "${1} ${true} ${2.5}""#).is_ok());
}
