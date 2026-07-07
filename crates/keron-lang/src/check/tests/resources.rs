//! Resource builtin function tests: `symlink`, `template`.

use super::check_src;

// ---------- well-typed builds ----------

#[test]
fn symlink_typechecks() {
    assert!(check_src(r#"val s: Symlink = symlink(source = "b", target = "a")"#).is_ok());
}

#[test]
fn template_typechecks() {
    assert!(
        check_src(
            r#"val f: Template = template(source = "tmpl.tpl", target = "x", vars = {"body": "y"})"#
        )
        .is_ok()
    );
}

#[test]
fn symlink_inferred_to_symlink_type() {
    assert!(check_src(r#"val s = symlink(source = "b", target = "a")"#).is_ok());
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
                val tmp: Template = template(source = "tmpl.tpl", target = "/p", vars = {"body": ""})
                symlink(source = "b", target = "a")
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
                    val a: Template = template(source = "tmpl.tpl", target = "/a", vars = {"body": ""})
                    symlink(source = "y", target = "x")
                } else {
                    val b: Template = template(source = "tmpl.tpl", target = "/b", vars = {"body": ""})
                    symlink(source = "v", target = "u")
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
                    val placeholder: Template = template(source = "tmpl.tpl", target = "/x", vars = {"body": "y"})
                }
            }"#
        )
        .is_ok()
    );
}

#[test]
fn resource_type_inside_top_level_reconcile_expr_resolves() {
    assert!(
        check_src(
            r#"if true {
                val f: Template = template(source = "tmpl.tpl", target = "/p", vars = {"body": ""})
                reconcile f
            }"#
        )
        .is_ok()
    );
}

#[test]
fn resource_type_inside_reconcile_decl_expr_resolves() {
    assert!(
        check_src(
            r#"reconcile if true {
                val f: Template = template(source = "tmpl.tpl", target = "/p", vars = {"body": ""})
                f
            } else {
                template(source = "tmpl.tpl", target = "/q", vars = {"body": ""})
            }"#
        )
        .is_ok()
    );
}

#[test]
fn recursive_struct_type_errors_without_overflowing() {
    let err = check_src("struct Node { next: Node? }\n").expect_err("should fail");
    assert!(
        err.iter()
            .any(|d| d.message.contains("recursive type `Node`")),
        "got: {err:?}",
    );
}

#[test]
fn named_args_reorder_for_symlink() {
    assert!(check_src(r#"val s: Symlink = symlink(source = "b", target = "a")"#).is_ok());
}

#[test]
fn positional_args_for_symlink() {
    assert!(check_src(r#"val s: Symlink = symlink("a", "b")"#).is_ok());
}

#[test]
fn list_of_symlinks_typechecks() {
    let src = r#"val xs: List<Symlink> = [symlink(source = "b", target = "a"), symlink(source = "d", target = "c")]"#;
    assert!(check_src(src).is_ok());
}

// ---------- arg validation ----------

#[test]
fn symlink_wrong_arg_type_errors() {
    let err = check_src(r#"val s: Symlink = symlink(source = "x", target = 1)"#)
        .expect_err("should fail");
    assert!(err[0].message.contains("expected `String`"));
    assert!(err[0].message.contains("found `Int`"));
}

#[test]
fn symlink_missing_arg_errors() {
    let err = check_src(r#"val s: Symlink = symlink(source = "a")"#).expect_err("should fail");
    assert!(
        err[0]
            .message
            .contains("missing required argument `target`")
    );
}

#[test]
fn template_missing_arg_errors() {
    let err = check_src(r#"val f: Template = template(source = "tmpl.tpl", target = "x")"#)
        .expect_err("should fail");
    assert!(err[0].message.contains("missing required argument `vars`"));
}

#[test]
fn unknown_named_arg_for_symlink_errors() {
    let err = check_src(r#"val s: Symlink = symlink(source = "b", what = 1, target = "a")"#)
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
        check_src(r#"val m: Map<String, Symlink> = {"z": symlink(source = "b", target = "a")}"#)
            .is_ok()
    );
}

#[test]
fn symlink_returned_from_user_fn() {
    let src = r#"
        fn make(name: String): Symlink {
            symlink(source = name, target = name)
        }
        val s: Symlink = make("zshrc")
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn symlink_assigned_to_int_errors() {
    let err =
        check_src(r#"val n: Int = symlink(source = "b", target = "a")"#).expect_err("should fail");
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

// ---------- Resource supertype ----------

#[test]
fn symlink_satisfies_resource_annotation() {
    let src = r#"val r: Resource = symlink(source = "b", target = "a")"#;
    assert!(check_src(src).is_ok());
}

#[test]
fn template_satisfies_resource_annotation() {
    let src =
        r#"val r: Resource = template(source = "tmpl.tpl", target = "p", vars = {"body": "c"})"#;
    assert!(check_src(src).is_ok());
}

#[test]
fn mixed_resource_list_inferred_to_list_of_resource() {
    let src = r#"
        val xs = [symlink(source = "b", target = "a"), template(source = "tmpl.tpl", target = "p", vars = {"body": "c"})]
        val ys: List<Resource> = xs
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn list_of_resource_annotation_accepts_mixed_elements() {
    let src = r#"
        val xs: List<Resource> = [
          symlink(source = "b", target = "a"),
          template(source = "tmpl.tpl", target = "p", vars = {"body": "c"}),
        ]
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn list_of_symlink_subtypes_list_of_resource() {
    let src = r#"
        val xs: List<Symlink> = [symlink(source = "b", target = "a")]
        val ys: List<Resource> = xs
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_accepts_resource_var() {
    let src = r#"
        val r: Resource = symlink(source = "b", target = "a")
        reconcile r
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_accepts_list_of_resource() {
    let src = r#"
        val xs: List<Resource> = [
          symlink(source = "b", target = "a"),
          template(source = "tmpl.tpl", target = "p", vars = {"body": "c"}),
        ]
        reconcile xs
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn reconcile_chain_mixes_kinds_via_resource() {
    let src = r#"
        val s: Symlink = symlink(source = "b", target = "a")
        val f: Template = template(source = "tmpl.tpl", target = "p", vars = {"body": "c"})
        reconcile s -> f
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
        val r: Resource = symlink(source = "b", target = "a")
        val s: Symlink = r
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("expected `Symlink`"));
    assert!(err[0].message.contains("found `Resource`"));
}

#[test]
fn resource_does_not_narrow_to_template() {
    let src = r#"
        val r: Resource = template(source = "p.tmpl", target = "p", vars = {})
        val f: Template = r
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("expected `Template`"));
    assert!(err[0].message.contains("found `Resource`"));
}

#[test]
fn list_of_resource_does_not_narrow_to_list_of_symlink() {
    let src = r#"
        val xs: List<Resource> = [symlink(source = "b", target = "a")]
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
        val s: Symlink = symlink(source = "b", target = "a")
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
            symlink(source = name, target = name)
        }
        val r: Resource = make("zshrc")
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn fn_param_resource_accepts_specific_kinds() {
    let src = r#"
        fn install(r: Resource): Resource {
            r
        }
        val s: Symlink = symlink(source = "b", target = "a")
        reconcile install(s)
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn fn_param_list_of_resource_accepts_mixed_arg() {
    let src = r#"
        fn install(rs: List<Resource>): List<Resource> {
            rs
        }
        reconcile install([
            symlink(source = "b", target = "a"),
            template(source = "tmpl.tpl", target = "p", vars = {"body": "c"}),
        ])
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn list_with_int_and_resource_still_errors() {
    // Subtyping only lifts among the resource singletons.
    let src = r#"val xs = [symlink(source = "b", target = "a"), 1]"#;
    assert!(check_src(src).is_err());
}

#[test]
fn resource_concat_with_list_of_resource_typechecks() {
    let src = r#"
        val a: List<Symlink> = [symlink(source = "b", target = "a")]
        val b: List<Template> = [template(source = "tmpl.tpl", target = "p", vars = {"body": "c"})]
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
        val a: List<Symlink> = [symlink(source = "b", target = "a")]
        val b: List<Template> = [template(source = "tmpl.tpl", target = "p", vars = {"body": "c"})]
        val all = a ++ b
        reconcile all
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn concat_resource_list_with_int_list_still_errors() {
    let src = r#"
        val a: List<Symlink> = [symlink(source = "b", target = "a")]
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
    // entry; `Symlink` and `Template` both satisfy that slot.
    let src = r#"
        val m: Map<String, Resource> = {
          "shell": symlink(source = "~/.zshrc", target = "df/zsh"),
          "motd": template(source = "tmpl.tpl", target = "/etc/motd", vars = {"body": "welcome"}),
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
          symlink(source = "b", target = "a"),
          template(source = "tmpl.tpl", target = "p", vars = {"body": "c"}),
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
          symlink(source = "~/.zshrc", target = "df/zsh")
        } else {
          template(source = "tmpl.tpl", target = "/etc/motd", vars = {"body": "welcome"})
        }
        reconcile r
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn if_synthesis_of_mixed_resource_branches_lifts_to_resource() {
    // `if` branches share one value slot, so they use the same join
    // as list elements: heterogeneous resource singletons lift to
    // `Resource` instead of strict-equating.
    let src = r#"
        val r: Resource = if true {
          symlink(source = "b", target = "a")
        } else {
          template(source = "tmpl.tpl", target = "p", vars = {"body": "c"})
        }
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn if_synthesis_of_unjoinable_branches_still_errors() {
    // The join only unifies resource singletons; unrelated types keep
    // the mismatched-branches error.
    let src = r#"
        val r = if true {
          symlink(source = "b", target = "a")
        } else {
          7
        }
    "#;
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("mismatched types"));
}
