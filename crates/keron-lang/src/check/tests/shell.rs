//! Shell resource builtin tests.

use super::check_src;

#[test]
fn shell_constructor_typechecks() {
    assert!(
        check_src(
            r#"val refresh: Shell = shell(kind = "sh", name = "refresh", script = "echo ok")"#
        )
        .is_ok()
    );
}

#[test]
fn shell_kind_type_allows_documented_variants() {
    for kind in ["sh", "bash", "zsh", "pwsh", "powershell"] {
        let src = format!(
            r#"val kind: ShellKind = "{kind}"
               val run: Shell = shell(kind = kind, name = "run-{kind}", script = "echo ok")"#
        );
        assert!(check_src(&src).is_ok(), "{kind} should typecheck");
    }
}

#[test]
fn shell_kind_rejects_unknown_literal() {
    let err =
        check_src(r#"val run: Shell = shell(kind = "fish", name = "refresh", script = "echo ok")"#)
            .expect_err("should fail");
    assert!(err[0].message.contains("not a variant of `ShellKind`"));
}

#[test]
fn shell_widens_to_resource() {
    assert!(
        check_src(
            r#"val run: Resource = shell(kind = "sh", name = "refresh", script = "echo ok")"#
        )
        .is_ok()
    );
}

#[test]
fn shell_mixes_with_other_resource_kinds() {
    assert!(
        check_src(
            r#"val xs: List<Resource> = [
                 shell(kind = "sh", name = "refresh", script = "echo ok"),
                 symlink(from = "a", to = "b"),
                 template(path = "p", source = "tmpl.tpl", vars = {"body": "c"}),
               ]
               reconcile xs"#
        )
        .is_ok()
    );
}

#[test]
fn resource_does_not_narrow_to_shell() {
    let err = check_src(
        r#"val r: Resource = shell(kind = "sh", name = "refresh", script = "echo ok")
           val s: Shell = r"#,
    )
    .expect_err("should fail");
    assert!(err[0].message.contains("expected `Shell`"));
    assert!(err[0].message.contains("found `Resource`"));
}

#[test]
fn shell_is_reconcilable() {
    assert!(
        check_src(
            r#"val run: Shell = shell(kind = "sh", name = "refresh", script = "echo ok")
               reconcile run"#
        )
        .is_ok()
    );
}
