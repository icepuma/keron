//! Resource builtin function tests: `symlink`, `file`, `directory`.

use super::check_src;

// ---------- well-typed builds ----------

#[test]
fn symlink_typechecks() {
    assert!(check_src(r#"val s: Symlink = symlink(from = "a", to = "b")"#).is_ok());
}

#[test]
fn template_typechecks() {
    assert!(
        check_src(
            r#"val f: Template = template(path = "x", source = "tmpl.tpl", vars = {"body": "y"})"#
        )
        .is_ok()
    );
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
fn resource_type_inside_fn_body_resolves() {
    // Pin `resolve_block_types`: a local `val` annotation with a
    // resource type lives inside the fn body's `Block`. Without
    // recursing into the body, the `Named("File")` placeholder
    // would survive and the checker would error with "unknown type".
    assert!(
        check_src(
            r#"fn make(): Symlink {
                val tmp: Template = template(path = "/p", source = "tmpl.tpl", vars = {"body": ""})
                symlink(from = "a", to = "b")
            }"#
        )
        .is_ok()
    );
}

#[test]
fn resource_type_inside_if_branch_resolves() {
    // Pin `resolve_expr_types`: type annotations inside an `if`'s
    // branch blocks reach `resolve_block_types` only via the
    // expression walker. A no-op `resolve_expr_types` would skip
    // resolution for these branches.
    assert!(
        check_src(
            r#"fn make(flag: Boolean): Symlink {
                if flag {
                    val a: Template = template(path = "/a", source = "tmpl.tpl", vars = {"body": ""})
                    symlink(from = "x", to = "y")
                } else {
                    val b: Directory = directory(path = "/b")
                    symlink(from = "u", to = "v")
                }
            }"#
        )
        .is_ok()
    );
}

#[test]
fn resource_type_inside_for_body_resolves() {
    // Similar to the `if` case — a `for` body block must also have
    // its inner annotations resolved.
    assert!(
        check_src(
            r#"fn pulse(): Void {
                for n in [1, 2] {
                    val placeholder: Template = template(path = "/x", source = "tmpl.tpl", vars = {"body": "y"})
                    reconcile placeholder
                }
            }"#
        )
        .is_ok()
    );
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
fn template_missing_arg_errors() {
    let err = check_src(r#"val f: Template = template(path = "x", source = "tmpl.tpl")"#)
        .expect_err("should fail");
    assert!(err[0].message.contains("missing required argument `vars`"));
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
fn template_cannot_be_map_key() {
    let err = check_src("val m: Map<Template, Int> = {}").expect_err("should fail");
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
    assert!(
        err[0]
            .message
            .contains("`symlink` is a builtin and cannot be redefined"),
        "got: {}",
        err[0].message,
    );
}

#[test]
fn user_val_collides_with_symlink_builtin() {
    let err = check_src(r"val symlink = 1").expect_err("should fail");
    assert!(
        err[0]
            .message
            .contains("`symlink` is a builtin and cannot be redefined"),
        "got: {}",
        err[0].message,
    );
}

#[test]
fn user_val_collides_with_template_builtin() {
    let err = check_src(r"val template = 1").expect_err("should fail");
    assert!(
        err[0]
            .message
            .contains("`template` is a builtin and cannot be redefined"),
        "got: {}",
        err[0].message,
    );
}

#[test]
fn user_val_collides_with_directory_builtin() {
    let err = check_src(r"val directory = 1").expect_err("should fail");
    assert!(
        err[0]
            .message
            .contains("`directory` is a builtin and cannot be redefined"),
        "got: {}",
        err[0].message,
    );
}

// ---------- Resource supertype ----------

#[test]
fn symlink_satisfies_resource_annotation() {
    let src = r#"val r: Resource = symlink(from = "a", to = "b")"#;
    assert!(check_src(src).is_ok());
}

#[test]
fn template_satisfies_resource_annotation() {
    let src =
        r#"val r: Resource = template(path = "p", source = "tmpl.tpl", vars = {"body": "c"})"#;
    assert!(check_src(src).is_ok());
}

#[test]
fn directory_satisfies_resource_annotation() {
    let src = r#"val r: Resource = directory(path = "p")"#;
    assert!(check_src(src).is_ok());
}

#[test]
fn mixed_resource_list_inferred_to_list_of_resource() {
    let src = r#"
        val xs = [symlink(from = "a", to = "b"), template(path = "p", source = "tmpl.tpl", vars = {"body": "c"})]
        val ys: List<Resource> = xs
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn mixed_resource_list_with_three_kinds_inferred_to_list_of_resource() {
    let src = r#"
        val xs = [
          symlink(from = "a", to = "b"),
          template(path = "p", source = "tmpl.tpl", vars = {"body": "c"}),
          directory(path = "d"),
        ]
        val ys: List<Resource> = xs
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn list_of_resource_annotation_accepts_mixed_elements() {
    let src = r#"
        val xs: List<Resource> = [
          symlink(from = "a", to = "b"),
          template(path = "p", source = "tmpl.tpl", vars = {"body": "c"}),
          directory(path = "d"),
        ]
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn list_of_symlink_subtypes_list_of_resource() {
    let src = r#"
        val xs: List<Symlink> = [symlink(from = "a", to = "b")]
        val ys: List<Resource> = xs
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_accepts_resource_var() {
    let src = r#"
        val r: Resource = symlink(from = "a", to = "b")
        reconcile r
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_accepts_list_of_resource() {
    let src = r#"
        val xs: List<Resource> = [
          symlink(from = "a", to = "b"),
          template(path = "p", source = "tmpl.tpl", vars = {"body": "c"}),
        ]
        reconcile xs
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_chain_mixes_kinds_via_resource() {
    let src = r#"
        val s: Symlink = symlink(from = "a", to = "b")
        val f: Template = template(path = "p", source = "tmpl.tpl", vars = {"body": "c"})
        val d: Directory = directory(path = "d")
        reconcile s -> f -> d
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn resource_does_not_narrow_to_symlink() {
    // Subtyping is one-way: a `Resource` cannot silently re-acquire a
    // specific kind. To enforce this, the tests below assert each
    // narrowing direction is rejected at check time. (See
    // `crates/keron-lang/src/check/mod.rs::is_subtype` for the rule.)
    let src = r#"
        val r: Resource = symlink(from = "a", to = "b")
        val s: Symlink = r
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("expected `Symlink`"));
    assert!(err[0].message.contains("found `Resource`"));
}

#[test]
fn resource_does_not_narrow_to_template() {
    let src = r#"
        val r: Resource = template(path = "p", source = "p.tmpl", vars = {})
        val f: Template = r
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("expected `Template`"));
    assert!(err[0].message.contains("found `Resource`"));
}

#[test]
fn resource_does_not_narrow_to_directory() {
    let src = r#"
        val r: Resource = directory(path = "p")
        val d: Directory = r
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("expected `Directory`"));
    assert!(err[0].message.contains("found `Resource`"));
}

#[test]
fn list_of_resource_does_not_narrow_to_list_of_symlink() {
    let src = r#"
        val xs: List<Resource> = [symlink(from = "a", to = "b")]
        val ys: List<Symlink> = xs
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("expected `List<Symlink>`"));
    assert!(err[0].message.contains("found `List<Resource>`"));
}

#[test]
fn symlink_does_not_narrow_to_template() {
    // Specific resource kinds remain distinct from each other.
    let src = r#"
        val s: Symlink = symlink(from = "a", to = "b")
        val f: Template = s
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("expected `Template`"));
    assert!(err[0].message.contains("found `Symlink`"));
}

#[test]
fn fn_returning_resource_from_symlink_typechecks() {
    let src = r#"
        fn make(name: String): Resource {
            symlink(from = name, to = name)
        }
        val r: Resource = make("zshrc")
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn fn_param_resource_accepts_specific_kinds() {
    // Top-level call expressions aren't legal; the `if true { … }`
    // wrapper turns the call into a Void-typed expression statement.
    let src = r#"
        fn install(r: Resource): Void {
            reconcile r
        }
        val s: Symlink = symlink(from = "a", to = "b")
        if true { install(s) }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn fn_param_list_of_resource_accepts_mixed_arg() {
    let src = r#"
        fn install(rs: List<Resource>): Void {
            reconcile rs
        }
        if true {
          install([
            symlink(from = "a", to = "b"),
            template(path = "p", source = "tmpl.tpl", vars = {"body": "c"}),
          ])
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn list_with_int_and_resource_still_errors() {
    // Subtyping only lifts among the resource singletons.
    let src = r#"val xs = [symlink(from = "a", to = "b"), 1]"#;
    assert!(check_src(src).is_err());
}

#[test]
fn resource_concat_with_list_of_resource_typechecks() {
    let src = r#"
        val a: List<Symlink> = [symlink(from = "a", to = "b")]
        val b: List<Template> = [template(path = "p", source = "tmpl.tpl", vars = {"body": "c"})]
        val all: List<Resource> = a ++ b
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn concat_lifts_heterogeneous_resource_lists_in_synthesis() {
    // Mirrors `list_type`: synthesising `[sym] ++ [file]` produces
    // `List<Resource>` so the binding can be used wherever a list of
    // resources is expected.
    let src = r#"
        val a: List<Symlink> = [symlink(from = "a", to = "b")]
        val b: List<Template> = [template(path = "p", source = "tmpl.tpl", vars = {"body": "c"})]
        val all = a ++ b
        reconcile all
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn concat_resource_list_with_int_list_still_errors() {
    let src = r#"
        val a: List<Symlink> = [symlink(from = "a", to = "b")]
        val b: List<Int> = [1]
        val all = a ++ b
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("matching `List<T>`"));
}

#[test]
fn empty_list_with_resource_annotation_typechecks() {
    let src = r"
        val xs: List<Resource> = []
        reconcile xs
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn map_with_resource_value_annotation_accepts_mix() {
    // The map's value bidirectional check pushes `Resource` into each
    // entry; `Symlink`, `File`, `Directory` all satisfy that slot.
    let src = r#"
        val m: Map<String, Resource> = {
          "shell": symlink(from = "df/zsh", to = "~/.zshrc"),
          "motd": template(path = "/etc/motd", source = "tmpl.tpl", vars = {"body": "welcome"}),
          "data": directory(path = "~/data"),
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn for_over_list_of_resource_binds_resource() {
    // The loop var inherits the element type — `Resource` here — so
    // its body sees a value compatible with any resource slot.
    let src = r#"
        val rs: List<Resource> = [
          symlink(from = "a", to = "b"),
          template(path = "p", source = "tmpl.tpl", vars = {"body": "c"}),
        ]
        for r in rs {
          reconcile r
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn if_branches_with_specific_resources_satisfy_resource_annotation() {
    // Bidirectional check pushes `Resource` into both branches; each
    // resolves via the one-way `Symlink|File <: Resource` rule.
    let src = r#"
        val use_zsh: Boolean = true
        val r: Resource = if use_zsh {
          symlink(from = "df/zsh", to = "~/.zshrc")
        } else {
          template(path = "/etc/motd", source = "tmpl.tpl", vars = {"body": "welcome"})
        }
        reconcile r
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn if_synthesis_of_mismatched_resource_branches_still_errors() {
    // Without an annotation, `if_type` strict-equates the branches.
    // Symlink ≠ File, so the synthesis path fails — users wanting the
    // unified type must annotate against `Resource`.
    let src = r#"
        val r = if true {
          symlink(from = "a", to = "b")
        } else {
          template(path = "p", source = "tmpl.tpl", vars = {"body": "c"})
        }
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("mismatched types"));
}
