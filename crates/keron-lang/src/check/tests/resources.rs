//! Resource builtin function tests: `symlink`, `file`, `directory`.

use super::check_src;

// ---------- well-typed builds ----------

#[test]
fn symlink_typechecks() {
    assert!(check_src(r#"val s: Symlink = symlink(from = "a", to = "b")"#).is_ok());
}

#[test]
fn file_typechecks() {
    assert!(check_src(r#"val f: File = file(path = "x", content = "y")"#).is_ok());
}

#[test]
fn directory_typechecks() {
    assert!(check_src(r#"val d: Directory = directory(path = "x")"#).is_ok());
}

#[test]
fn symlink_inferred_to_symlink_type() {
    assert!(check_src(r#"val s = symlink(from = "a", to = "b")"#).is_ok());
}

#[test]
fn named_args_reorder_for_symlink() {
    assert!(check_src(r#"val s: Symlink = symlink(to = "b", from = "a")"#).is_ok());
}

#[test]
fn positional_args_for_symlink() {
    assert!(check_src(r#"val s: Symlink = symlink("a", "b")"#).is_ok());
}

#[test]
fn list_of_symlinks_typechecks() {
    let src =
        r#"val xs: List<Symlink> = [symlink(from = "a", to = "b"), symlink(from = "c", to = "d")]"#;
    assert!(check_src(src).is_ok());
}

// ---------- arg validation ----------

#[test]
fn symlink_wrong_arg_type_errors() {
    let err =
        check_src(r#"val s: Symlink = symlink(from = 1, to = "x")"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `String`"));
    assert!(err[0].message.contains("found `Int`"));
}

#[test]
fn symlink_missing_arg_errors() {
    let err = check_src(r#"val s: Symlink = symlink(from = "a")"#).expect_err("should fail");
    assert!(err[0].message.contains("missing required argument `to`"));
}

#[test]
fn file_missing_arg_errors() {
    let err = check_src(r#"val f: File = file(path = "x")"#).expect_err("should fail");
    assert!(
        err[0]
            .message
            .contains("missing required argument `content`")
    );
}

#[test]
fn unknown_named_arg_for_symlink_errors() {
    let err = check_src(r#"val s: Symlink = symlink(from = "a", to = "b", what = 1)"#)
        .expect_err("should fail");
    assert!(err[0].message.contains("`symlink` has no parameter `what`"));
}

// ---------- type-system interactions ----------

#[test]
fn symlink_cannot_be_map_key() {
    let err = check_src("val m: Map<Symlink, Int> = {}").expect_err("should fail");
    assert!(err[0].message.contains("not a valid `Map` key type"));
    assert!(err[0].message.contains("Symlink"));
}

#[test]
fn file_cannot_be_map_key() {
    let err = check_src("val m: Map<File, Int> = {}").expect_err("should fail");
    assert!(err[0].message.contains("not a valid `Map` key type"));
}

#[test]
fn map_with_symlink_value_typechecks() {
    assert!(
        check_src(r#"val m: Map<String, Symlink> = {"z": symlink(from = "a", to = "b")}"#).is_ok()
    );
}

#[test]
fn symlink_returned_from_user_fn() {
    let src = r#"
        fn make(name: String): Symlink {
            symlink(from = name, to = name)
        }
        val s: Symlink = make("zshrc")
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn symlink_assigned_to_int_errors() {
    let err = check_src(r#"val n: Int = symlink(from = "a", to = "b")"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `Symlink`"));
}

// ---------- builtin/user collisions ----------

#[test]
fn user_fn_collides_with_symlink_builtin() {
    let err = check_src(r"fn symlink(): Int { 1 }").expect_err("should fail");
    assert!(err[0].message.contains("`symlink` is already defined"));
}

#[test]
fn user_val_collides_with_symlink_builtin() {
    let err = check_src(r"val symlink = 1").expect_err("should fail");
    assert!(err[0].message.contains("`symlink` is already defined"));
}

#[test]
fn user_val_collides_with_file_builtin() {
    let err = check_src(r"val file = 1").expect_err("should fail");
    assert!(err[0].message.contains("`file` is already defined"));
}

#[test]
fn user_val_collides_with_directory_builtin() {
    let err = check_src(r"val directory = 1").expect_err("should fail");
    assert!(err[0].message.contains("`directory` is already defined"));
}
