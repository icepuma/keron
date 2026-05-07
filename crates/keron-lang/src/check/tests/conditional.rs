//! `if`/`else` expression type-checker tests.

use super::check_src;

// ---------- well-typed ----------

#[test]
fn if_with_int_branches_typechecks() {
    assert!(check_src("val r: Int = if true { 1 } else { 2 }").is_ok());
}

#[test]
fn if_inferred_type_matches_branches() {
    assert!(check_src("val r = if true { 1 } else { 2 }").is_ok());
}

#[test]
fn if_with_string_branches_typechecks() {
    assert!(check_src(r#"val r: String = if true { "a" } else { "b" }"#).is_ok());
}

#[test]
fn if_with_double_branches_typechecks() {
    assert!(check_src("val r: Double = if true { 1.5 } else { 2.5 }").is_ok());
}

#[test]
fn if_with_boolean_branches_typechecks() {
    assert!(check_src("val r: Boolean = if true { true } else { false }").is_ok());
}

#[test]
fn else_if_chain_typechecks() {
    let src = "val r: Int = if true { 1 } else if false { 2 } else { 3 }";
    assert!(check_src(src).is_ok());
}

#[test]
fn if_with_resource_branches_typechecks() {
    let src = r#"
        val pick: Boolean = true
        val r: Symlink = if pick {
            symlink(from = "a", to = "b")
        } else {
            symlink(from = "c", to = "d")
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn if_in_fn_body_typechecks() {
    let src = "fn classify(n: Int): Int { if true { n } else { 0 } }";
    assert!(check_src(src).is_ok());
}

#[test]
fn if_with_var_cond_typechecks() {
    let src = r"
        val flag: Boolean = true
        val r: Int = if flag { 1 } else { 2 }
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn if_with_arithmetic_branches_typechecks() {
    assert!(check_src("val r: Int = if true { 1 + 2 } else { 3 * 4 }").is_ok());
}

#[test]
fn if_with_call_branches_typechecks() {
    let src = r"
        fn one(): Int { 1 }
        fn two(): Int { 2 }
        val r: Int = if true { one() } else { two() }
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn if_inside_arithmetic_typechecks() {
    assert!(check_src("val r: Int = 10 + if true { 1 } else { 2 }").is_ok());
}

#[test]
fn if_branches_can_be_empty_lists_with_annotation() {
    // Bidirectional checking pushes the expected `List<Int>` into both
    // branches, allowing empty list literals.
    assert!(check_src("val r: List<Int> = if true { [] } else { [1, 2] }").is_ok());
}

#[test]
fn if_branches_can_be_empty_maps_with_annotation() {
    let src = r#"val r: Map<String, Int> = if true { {} } else { {"a": 1} }"#;
    assert!(check_src(src).is_ok());
}

// ---------- error cases ----------

#[test]
fn if_non_boolean_cond_errors() {
    let err = check_src("val r: Int = if 1 { 2 } else { 3 }").expect_err("should fail");
    assert!(err[0].message.contains("expected `Boolean`"));
    assert!(err[0].message.contains("found `Int`"));
}

#[test]
fn if_string_cond_errors() {
    let err = check_src(r#"val r: Int = if "yes" { 1 } else { 2 }"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `Boolean`"));
    assert!(err[0].message.contains("found `String`"));
}

#[test]
fn if_branch_type_mismatch_errors() {
    let err = check_src(r#"val r = if true { 1 } else { "two" }"#).expect_err("should fail");
    assert!(
        err[0]
            .message
            .contains("`if` branches have mismatched types")
    );
    assert!(err[0].message.contains("`then` is `Int`"));
    assert!(err[0].message.contains("`else` is `String`"));
}

#[test]
fn if_int_double_branches_do_not_promote() {
    // No implicit Int->Double promotion in if branches, mirroring vals.
    let err = check_src("val r = if true { 1 } else { 2.5 }").expect_err("should fail");
    assert!(err[0].message.contains("mismatched types"));
}

#[test]
fn if_with_annotation_mismatch_at_then_errors() {
    let err = check_src(r#"val r: Int = if true { "x" } else { 1 }"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `String`"));
}

#[test]
fn if_with_annotation_mismatch_at_else_errors() {
    let err = check_src(r#"val r: Int = if true { 1 } else { "x" }"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `String`"));
}

#[test]
fn else_if_chain_branch_mismatch_errors() {
    let src = r#"val r = if true { 1 } else if false { "x" } else { 3 }"#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("mismatched types"));
}

#[test]
fn if_cond_uses_unknown_var_errors() {
    let err = check_src("val r: Int = if nope { 1 } else { 2 }").expect_err("should fail");
    assert!(err[0].message.contains("unknown variable `nope`"));
}

#[test]
fn realize_if_resource_typechecks() {
    let src = r#"
        val use_zsh: Boolean = true
        realize if use_zsh {
            symlink(from = "~/df/zshrc", to = "~/.zshrc")
        } else {
            symlink(from = "~/df/bashrc", to = "~/.bashrc")
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn realize_if_int_errors() {
    let err = check_src("realize if true { 1 } else { 2 }").expect_err("should fail");
    assert!(
        err[0]
            .message
            .contains("`realize` expects a resource or list of resources")
    );
    assert!(err[0].message.contains("found `Int`"));
}
