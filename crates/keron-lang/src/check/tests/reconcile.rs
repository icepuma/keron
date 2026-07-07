//! `reconcile` declaration tests.

use super::check_src;

// ---------- well-typed realizations ----------

#[test]
fn reconcile_symlink_call_typechecks() {
    assert!(check_src(r#"reconcile symlink(source = "b", target = "a")"#).is_ok());
}

#[test]
fn reconcile_file_call_typechecks() {
    assert!(
        check_src(r#"reconcile template(source = "tmpl.tpl", target = "x", vars = {"body": "y"})"#)
            .is_ok()
    );
}

#[test]
fn reconcile_symlink_var_typechecks() {
    let src = r#"
        val s: Symlink = symlink(source = "b", target = "a")
        reconcile s
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_list_of_symlinks_typechecks() {
    let src =
        r#"reconcile [symlink(source = "b", target = "a"), symlink(source = "d", target = "c")]"#;
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
            symlink(source = "b", target = "a")
        }
        reconcile make()
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_mixed_kinds_via_repeated_directives() {
    let src = r#"
        val s: Symlink = symlink(source = "b", target = "a")
        val f: Template = template(source = "tmpl.tpl", target = "x", vars = {"body": "y"})
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
        val x: Symlink = symlink(source = "b", target = "a")
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
    let err = check_src(r#"reconcile symlink(source = "b", target = 1)"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `String`"));
    assert!(err[0].message.contains("found `Int`"));
}

// ---------- chain (`->`) and block forms ----------

#[test]
fn reconcile_chain_of_resources_typechecks() {
    let src = r#"
        val a: Symlink = symlink(source = "y", target = "x")
        val b: Template = template(source = "tmpl.tpl", target = "p", vars = {"body": "c"})
        reconcile a -> b
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_block_of_single_steps_typechecks() {
    let src = r#"
        val a: Symlink = symlink(source = "y", target = "x")
        val b: Template = template(source = "tmpl.tpl", target = "p", vars = {"body": "c"})
        reconcile {
          a
          b
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_block_with_chain_steps_typechecks() {
    let src = r#"
        val a: Symlink = symlink(source = "y", target = "x")
        val b: Symlink = symlink(source = "q", target = "p")
        val c: Symlink = symlink(source = "v", target = "u")
        reconcile {
          a
          b -> c
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_chain_with_non_reconcilable_step_at_head_errors() {
    let src = r#"
        val a: Symlink = symlink(source = "y", target = "x")
        reconcile 42 -> a
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("found `Int`")));
}

#[test]
fn reconcile_chain_with_non_reconcilable_step_in_middle_errors() {
    let src = r#"
        val a: Symlink = symlink(source = "y", target = "x")
        val b: Symlink = symlink(source = "q", target = "p")
        reconcile a -> "nope" -> b
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("found `String`")));
}

#[test]
fn reconcile_chain_reports_every_bad_step() {
    let src = r#"
        val a: Symlink = symlink(source = "y", target = "x")
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
        val a: Symlink = symlink(source = "y", target = "x")
        val b: Symlink = symlink(source = "q", target = "p")
        if true {
          reconcile {
            a
            b
          }
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_block_inside_for_typechecks() {
    let src = r#"
        val xs: List<Symlink> = [symlink(source = "y", target = "x")]
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
        val s: Symlink = symlink(source = "b", target = "a")
        val r: Resource = template(source = "tmpl.tpl", target = "p", vars = {"body": "c"})
        val t: Template = template(source = "tmpl.tpl", target = "q", vars = {"body": "d"})
        reconcile s -> r -> t
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_block_with_resource_step_typechecks() {
    let src = r#"
        val r: Resource = symlink(source = "b", target = "a")
        val rs: List<Resource> = [
          template(source = "tmpl.tpl", target = "p", vars = {"body": "c"}),
          template(source = "tmpl.tpl", target = "q", vars = {"body": "d"}),
        ]
        reconcile {
          r
          rs
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_chain_length_four_typechecks() {
    let src = r#"
        val a: Symlink = symlink(source = "b", target = "a")
        val b: Symlink = symlink(source = "d", target = "c")
        val c: Symlink = symlink(source = "f", target = "e")
        val d: Symlink = symlink(source = "h", target = "g")
        reconcile a -> b -> c -> d
    "#;
    assert!(check_src(src).is_ok());
}

// ---------- `reconcile` rejected in value positions ----------

#[test]
fn reconcile_in_fn_param_default_is_rejected() {
    // A `reconcile` inside a param default evaluates against a
    // throwaway sink and would be silently dropped — reject it.
    let src = "fn f(x: Int = if true { reconcile { shell(kind = \"sh\", name = \"dropped\", script = \"x\") } 1 } else { 1 }): Int { x }";
    let err = check_src(src).expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("silently dropped")));
}

#[test]
fn reconcile_in_struct_field_default_is_rejected() {
    let src = "struct S { n: Int = if true { reconcile { shell(kind = \"sh\", name = \"dropped\", script = \"x\") } 1 } else { 1 } }";
    let err = check_src(src).expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("silently dropped")));
}

#[test]
fn reconcile_in_match_guard_is_rejected() {
    let src = "val r: Int = match 1 {\n\
               n if if true { reconcile { shell(kind = \"sh\", name = \"dropped\", script = \"x\") } true } else { false } => n,\n\
               _ => 0,\n\
               }";
    let err = check_src(src).expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("silently dropped")));
}

#[test]
fn reconcile_in_top_level_if_condition_is_rejected() {
    // A top-level `if`/`for`/`match` statement is exec-void, but its
    // *condition* is a value position: a reconcile there is dropped.
    let src = "if (if true { reconcile brew(\"ripgrep\") true } else { false }) {} else {}";
    let err = check_src(src).expect_err("reconcile in top-level if cond must be rejected");
    assert!(
        err.iter().any(|d| d.message.contains("silently dropped")),
        "got: {err:?}"
    );
}

#[test]
fn reconcile_in_top_level_for_iterable_is_rejected() {
    let src = "for x in (if true { reconcile brew(\"ripgrep\") [1] } else { [] }) { reconcile brew(\"jq\") }";
    let err = check_src(src).expect_err("reconcile in top-level for iterable must be rejected");
    assert!(
        err.iter().any(|d| d.message.contains("silently dropped")),
        "got: {err:?}"
    );
}

#[test]
fn reconcile_in_top_level_if_body_is_allowed() {
    // The whole point of exec-void: a statement-level reconcile inside a
    // top-level `if` body is real and must NOT be rejected.
    assert!(
        check_src("if true { reconcile brew(\"ripgrep\") } else {}").is_ok(),
        "reconcile in a top-level if body is legitimate"
    );
}
