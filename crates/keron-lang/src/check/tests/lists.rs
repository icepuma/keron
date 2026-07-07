//! List typing, list concat, and bidirectional checking against `List<T>`.

use super::{check_src, fn_sig, param};
use crate::{
    ast::Type,
    check::{ImportedSymbols, check_module, resolve_type_names},
    parser::parse,
};

fn check(program: &crate::ast::Program) -> Result<(), Vec<crate::diagnostic::Diagnostic>> {
    check_module(program, &ImportedSymbols::default())
}

fn check_with_list_intrinsics(src: &str) -> Result<(), Vec<crate::diagnostic::Diagnostic>> {
    let mut prog = parse(src).expect("parse should succeed");
    let mut imp = ImportedSymbols::default();
    let t = Type::Generic("T".into());
    imp.fns.insert(
        "contains".into(),
        fn_sig(
            vec![
                param("x", Type::Generic("C".into())),
                param("item", t.clone()),
            ],
            Type::Boolean,
        ),
    );
    imp.fns.insert(
        "unique".into(),
        fn_sig(
            vec![param("xs", Type::List(Box::new(t.clone())))],
            Type::List(Box::new(t.clone())),
        ),
    );
    imp.fns.insert(
        "index_of".into(),
        fn_sig(
            vec![param("xs", Type::List(Box::new(t.clone()))), param("x", t)],
            Type::Nullable(Box::new(Type::Int)),
        ),
    );
    resolve_type_names(&mut prog, &imp)?;
    check_module(&prog, &imp)
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
fn list_promotes_int_and_double_to_double() {
    // Mixed numerics join to Double, mirroring arithmetic promotion.
    assert!(check_src("val xs: List<Double> = [1, 2.5]").is_ok());
}

#[test]
fn nested_list_does_not_promote() {
    // The join is top-level only — no recursion into containers.
    let err = check_src("val xs = [[1], [2.5]]").expect_err("should fail");
    assert!(err[0].message.contains("List<Int>"));
    assert!(err[0].message.contains("List<Double>"));
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
fn list_concat_promotes_int_and_double_to_double() {
    // `++` uses the same element join as the literal `[1, 2.5]`.
    assert!(check_src("val xs: List<Double> = [1] ++ [2.5]").is_ok());
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

#[test]
fn equality_list_intrinsics_accept_scalar_element_types() {
    let src = r#"val has: Boolean = contains([1, 2], 2)
                 val dedup: List<String> = unique(["a", "a"])
                 val idx: Int = index_of([true], true) ?? -1
    "#;
    assert!(check_with_list_intrinsics(src).is_ok());
}

#[test]
fn equality_list_intrinsics_reject_struct_elements() {
    let src = r"struct Point { x: Int }
                 val p: Point = Point { x: 1 }
                 val has: Boolean = contains([p], p)
    ";
    let err = check_with_list_intrinsics(src).expect_err("struct equality should fail");
    assert!(
        err[0].message.contains("supported equality"),
        "got: {:?}",
        err[0],
    );
}

#[test]
fn equality_list_intrinsics_reject_nullable_elements() {
    let src = r"val xs: List<Int?> = []
                 val idx: Int = index_of(xs, null) ?? -1
    ";
    let err = check_with_list_intrinsics(src).expect_err("nullable equality should fail");
    assert!(
        err[0].message.contains("supported equality"),
        "got: {:?}",
        err[0],
    );
}
