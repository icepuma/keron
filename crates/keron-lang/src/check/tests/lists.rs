//! List typing, list concat, and bidirectional checking against `List<T>`.

use super::check_src;
use crate::{
    check::{ImportedSymbols, check_module},
    parser::parse,
};

fn check(program: &crate::ast::Program) -> Result<(), Vec<crate::diagnostic::Diagnostic>> {
    check_module(program, &ImportedSymbols::default())
}

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
    let err = check_src("val xs = [1, 2.5]").expect_err("should fail");
    assert!(err[0].message.contains("Int"));
    assert!(err[0].message.contains("Double"));
}

#[test]
fn list_annotation_mismatch_points_at_element() {
    // With bidirectional checking, the expected element type is pushed
    // into each list item, so the error pinpoints the offending value
    // rather than reporting a whole-list mismatch.
    let err = check_src(r"val xs: List<String> = [1]").expect_err("should fail");
    assert!(err[0].message.contains("expected `String`"));
    assert!(err[0].message.contains("found `Int`"));
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

// ---------- list concat (`++`) ----------

#[test]
fn list_concat_typechecks() {
    assert!(check_src("val xs: List<Int> = [1, 2] ++ [3]").is_ok());
    assert!(check_src("val xs = [1] ++ [2, 3] ++ [4]").is_ok());
}

#[test]
fn list_concat_inferred() {
    let prog = parse(r#"val xs = ["a"] ++ ["b", "c"]"#).expect("parse");
    assert!(check(&prog).is_ok());
}

#[test]
fn list_concat_mismatched_element_types_errors() {
    let err = check_src(r#"val xs = [1] ++ ["a"]"#).expect_err("should fail");
    assert!(err[0].message.contains("matching `List<T>`"));
    assert!(err[0].message.contains("List<Int>"));
    assert!(err[0].message.contains("List<String>"));
}

#[test]
fn list_concat_with_non_list_errors() {
    let err = check_src("val xs = [1] ++ 2").expect_err("should fail");
    assert!(err[0].message.contains("matching `List<T>`"));
}

#[test]
fn list_concat_does_not_promote_int_to_double() {
    let err = check_src("val xs = [1] ++ [2.5]").expect_err("should fail");
    assert!(err[0].message.contains("List<Int>"));
    assert!(err[0].message.contains("List<Double>"));
}

#[test]
fn nested_list_concat() {
    assert!(check_src("val xs: List<List<Int>> = [[1]] ++ [[2, 3]]").is_ok());
}

#[test]
fn list_concat_chain_left_associative() {
    assert!(check_src("val xs: List<Int> = [1] ++ [2] ++ [3] ++ [4]").is_ok());
}

// ---------- bidirectional checking ----------

#[test]
fn empty_list_in_concat_with_annotation() {
    assert!(check_src("val xs: List<Int> = [] ++ [1, 2]").is_ok());
    assert!(check_src("val xs: List<Int> = [1, 2] ++ []").is_ok());
    assert!(check_src("val xs: List<String> = [] ++ []").is_ok());
}

#[test]
fn nested_empty_list_with_annotation() {
    assert!(check_src("val xs: List<List<Int>> = [[1], []]").is_ok());
    assert!(check_src("val xs: List<List<Int>> = [[], [2]]").is_ok());
    assert!(check_src("val xs: List<List<Int>> = [[], []]").is_ok());
}

#[test]
fn empty_list_concat_without_annotation_still_errors() {
    let err = check_src("val xs = [] ++ [1, 2]").expect_err("should fail");
    assert!(err[0].message.contains("empty list"));
}

#[test]
fn check_mode_propagates_element_mismatch() {
    let err = check_src(r#"val xs: List<Int> = [1, "x"]"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `String`"));
}

#[test]
fn check_mode_empty_list_against_non_list_keeps_clear_error() {
    let err = check_src("val n: Int = []").expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("empty list"));
}

#[test]
fn check_mode_nonempty_list_against_non_list_reports_list_type() {
    let err = check_src("val n: Int = [1, 2]").expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("List<Int>"));
    assert!(!err[0].message.contains("empty list"));
}

#[test]
fn check_mode_concat_against_non_list_falls_back_to_synth() {
    let src = "val n: Int = [1] ++ [2]";
    let prog = parse(src).expect("parse should succeed");
    let err = check(&prog).expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("List<Int>"));
    assert_eq!(&src[err[0].span.clone()], "[1] ++ [2]");
}
