//! `realize` declaration tests.

use super::check_src;

// ---------- well-typed realizations ----------

#[test]
fn realize_symlink_call_typechecks() {
    assert!(check_src(r#"realize symlink(from = "a", to = "b")"#).is_ok());
}

#[test]
fn realize_file_call_typechecks() {
    assert!(check_src(r#"realize file(path = "x", content = "y")"#).is_ok());
}

#[test]
fn realize_directory_call_typechecks() {
    assert!(check_src(r#"realize directory(path = "x")"#).is_ok());
}

#[test]
fn realize_symlink_var_typechecks() {
    let src = r#"
        val s: Symlink = symlink(from = "a", to = "b")
        realize s
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn realize_list_of_symlinks_typechecks() {
    let src = r#"realize [symlink(from = "a", to = "b"), symlink(from = "c", to = "d")]"#;
    assert!(check_src(src).is_ok());
}

#[test]
fn realize_empty_list_with_annotation_typechecks() {
    let src = r"
        val xs: List<Symlink> = []
        realize xs
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn realize_user_fn_returning_symlink_typechecks() {
    let src = r#"
        fn make(): Symlink {
            symlink(from = "a", to = "b")
        }
        realize make()
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn realize_mixed_kinds_via_repeated_directives() {
    let src = r#"
        val s: Symlink = symlink(from = "a", to = "b")
        val f: File = file(path = "x", content = "y")
        val d: Directory = directory(path = "p")
        realize s
        realize f
        realize d
    "#;
    assert!(check_src(src).is_ok());
}

// ---------- error cases ----------

#[test]
fn realize_int_errors() {
    let err = check_src("realize 42").expect_err("should fail");
    assert!(
        err[0]
            .message
            .contains("`realize` expects a resource or list of resources")
    );
    assert!(err[0].message.contains("found `Int`"));
}

#[test]
fn realize_string_errors() {
    let err = check_src(r#"realize "hello""#).expect_err("should fail");
    assert!(err[0].message.contains("found `String`"));
}

#[test]
fn realize_list_of_int_errors() {
    let err = check_src("realize [1, 2, 3]").expect_err("should fail");
    assert!(err[0].message.contains("found `List<Int>`"));
}

#[test]
fn realize_map_errors() {
    let err = check_src(r#"realize {"a": 1}"#).expect_err("should fail");
    assert!(err[0].message.contains("found `Map<String, Int>`"));
}

#[test]
fn realize_double_errors() {
    let err = check_src("realize 3.14").expect_err("should fail");
    assert!(err[0].message.contains("found `Double`"));
}

#[test]
fn realize_unknown_var_errors() {
    let err = check_src("realize nope").expect_err("should fail");
    assert!(err[0].message.contains("unknown variable `nope`"));
}

#[test]
fn realize_forward_reference_errors() {
    let src = r#"
        realize x
        val x: Symlink = symlink(from = "a", to = "b")
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("unknown variable `x`"));
}

#[test]
fn realize_int_var_errors() {
    let src = r"
        val n: Int = 1
        realize n
    ";
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("found `Int`"));
}

#[test]
fn realize_list_of_string_errors() {
    let src = r#"
        val xs: List<String> = ["a", "b"]
        realize xs
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("found `List<String>`"));
}

#[test]
fn realize_arg_type_mismatch_propagates() {
    let err = check_src(r#"realize symlink(from = 1, to = "b")"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `String`"));
    assert!(err[0].message.contains("found `Int`"));
}
