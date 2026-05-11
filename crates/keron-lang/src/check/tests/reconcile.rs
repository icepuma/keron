//! `reconcile` declaration tests.

use super::check_src;

// ---------- well-typed realizations ----------

#[test]
fn reconcile_symlink_call_typechecks() {
    assert!(check_src(r#"reconcile symlink(from = "a", to = "b")"#).is_ok());
}

#[test]
fn reconcile_file_call_typechecks() {
    assert!(
        check_src(r#"reconcile template(path = "x", source = "tmpl.tpl", vars = {"body": "y"})"#)
            .is_ok()
    );
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
        val f: Template = template(path = "x", source = "tmpl.tpl", vars = {"body": "y"})
        reconcile s
        reconcile f
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
    // The map-literal form `{...}` after `reconcile` is committed to
    // the block grammar at parse time, so a map can only reach the
    // checker via a `val` binding.
    let src = r#"
        val m: Map<String, Int> = {"a": 1}
        reconcile m
    "#;
    let err = check_src(src).expect_err("should fail");
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

// ---------- chain (`->`) and block forms ----------

#[test]
fn reconcile_chain_of_resources_typechecks() {
    let src = r#"
        val a: Symlink = symlink(from = "x", to = "y")
        val b: Template = template(path = "p", source = "tmpl.tpl", vars = {"body": "c"})
        reconcile a -> b
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_block_of_single_steps_typechecks() {
    let src = r#"
        val a: Symlink = symlink(from = "x", to = "y")
        val b: Template = template(path = "p", source = "tmpl.tpl", vars = {"body": "c"})
        reconcile {
          a;
          b
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_block_with_chain_steps_typechecks() {
    let src = r#"
        val a: Symlink = symlink(from = "x", to = "y")
        val b: Symlink = symlink(from = "p", to = "q")
        val c: Symlink = symlink(from = "u", to = "v")
        reconcile {
          a;
          b -> c
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_chain_with_non_reconcilable_step_at_head_errors() {
    let src = r#"
        val a: Symlink = symlink(from = "x", to = "y")
        reconcile 42 -> a
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("found `Int`")));
}

#[test]
fn reconcile_chain_with_non_reconcilable_step_in_middle_errors() {
    let src = r#"
        val a: Symlink = symlink(from = "x", to = "y")
        val b: Symlink = symlink(from = "p", to = "q")
        reconcile a -> "nope" -> b
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("found `String`")));
}

#[test]
fn reconcile_chain_reports_every_bad_step() {
    let src = r#"
        val a: Symlink = symlink(from = "x", to = "y")
        reconcile 1 -> a -> "two" -> 3.0
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("found `Int`")));
    assert!(err.iter().any(|d| d.message.contains("found `String`")));
    assert!(err.iter().any(|d| d.message.contains("found `Double`")));
}

#[test]
fn reconcile_block_inside_if_typechecks() {
    let src = r#"
        val a: Symlink = symlink(from = "x", to = "y")
        val b: Symlink = symlink(from = "p", to = "q")
        if true {
          reconcile {
            a;
            b
          }
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_block_inside_for_typechecks() {
    let src = r#"
        val xs: List<Symlink> = [symlink(from = "x", to = "y")]
        for s in xs {
          reconcile {
            s
          }
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_chain_with_resource_step_typechecks() {
    let src = r#"
        val s: Symlink = symlink(from = "a", to = "b")
        val r: Resource = template(path = "p", source = "tmpl.tpl", vars = {"body": "c"})
        val t: Template = template(path = "q", source = "tmpl.tpl", vars = {"body": "d"})
        reconcile s -> r -> t
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_block_with_resource_step_typechecks() {
    let src = r#"
        val r: Resource = symlink(from = "a", to = "b")
        val rs: List<Resource> = [
          template(path = "p", source = "tmpl.tpl", vars = {"body": "c"}),
          template(path = "q", source = "tmpl.tpl", vars = {"body": "d"}),
        ]
        reconcile {
          r;
          rs
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_chain_length_four_typechecks() {
    let src = r#"
        val a: Symlink = symlink(from = "a", to = "b")
        val b: Symlink = symlink(from = "c", to = "d")
        val c: Symlink = symlink(from = "e", to = "f")
        val d: Symlink = symlink(from = "g", to = "h")
        reconcile a -> b -> c -> d
    "#;
    assert!(check_src(src).is_ok());
}
