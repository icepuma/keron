//! `reconcile` declaration tests.

use super::check_src;

// ---------- well-typed realizations ----------

#[test]
fn reconcile_symlink_call_typechecks() {
    assert!(check_src(r#"reconcile symlink(from = "a", to = "b")"#).is_ok());
}

#[test]
fn reconcile_file_call_typechecks() {
    assert!(check_src(r#"reconcile file(path = "x", content = "y")"#).is_ok());
}

#[test]
fn reconcile_directory_call_typechecks() {
    assert!(check_src(r#"reconcile directory(path = "x")"#).is_ok());
}

#[test]
fn reconcile_symlink_var_typechecks() {
    let src = r#"
        val s: Symlink = symlink(from = "a", to = "b")
        reconcile s
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_list_of_symlinks_typechecks() {
    let src = r#"reconcile [symlink(from = "a", to = "b"), symlink(from = "c", to = "d")]"#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_empty_list_with_annotation_typechecks() {
    let src = r"
        val xs: List<Symlink> = []
        reconcile xs
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_user_fn_returning_symlink_typechecks() {
    let src = r#"
        fn make(): Symlink {
            symlink(from = "a", to = "b")
        }
        reconcile make()
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_mixed_kinds_via_repeated_directives() {
    let src = r#"
        val s: Symlink = symlink(from = "a", to = "b")
        val f: File = file(path = "x", content = "y")
        val d: Directory = directory(path = "p")
        reconcile s
        reconcile f
        reconcile d
    "#;
    assert!(check_src(src).is_ok());
}

// ---------- error cases ----------

#[test]
fn reconcile_int_errors() {
    let err = check_src("reconcile 42").expect_err("should fail");
    assert!(
        err[0]
            .message
            .contains("`reconcile` expects a resource or list of resources")
    );
    assert!(err[0].message.contains("found `Int`"));
}

#[test]
fn reconcile_string_errors() {
    let err = check_src(r#"reconcile "hello""#).expect_err("should fail");
    assert!(err[0].message.contains("found `String`"));
}

#[test]
fn reconcile_list_of_int_errors() {
    let err = check_src("reconcile [1, 2, 3]").expect_err("should fail");
    assert!(err[0].message.contains("found `List<Int>`"));
}

#[test]
fn reconcile_map_errors() {
    let err = check_src(r#"reconcile {"a": 1}"#).expect_err("should fail");
    assert!(err[0].message.contains("found `Map<String, Int>`"));
}

#[test]
fn reconcile_double_errors() {
    let err = check_src("reconcile 3.14").expect_err("should fail");
    assert!(err[0].message.contains("found `Double`"));
}

#[test]
fn reconcile_unknown_var_errors() {
    let err = check_src("reconcile nope").expect_err("should fail");
    assert!(err[0].message.contains("unknown variable `nope`"));
}

#[test]
fn reconcile_forward_reference_errors() {
    let src = r#"
        reconcile x
        val x: Symlink = symlink(from = "a", to = "b")
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("unknown variable `x`"));
}

#[test]
fn reconcile_int_var_errors() {
    let src = r"
        val n: Int = 1
        reconcile n
    ";
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("found `Int`"));
}

#[test]
fn reconcile_list_of_string_errors() {
    let src = r#"
        val xs: List<String> = ["a", "b"]
        reconcile xs
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("found `List<String>`"));
}

#[test]
fn reconcile_arg_type_mismatch_propagates() {
    let err = check_src(r#"reconcile symlink(from = 1, to = "b")"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `String`"));
    assert!(err[0].message.contains("found `Int`"));
}
