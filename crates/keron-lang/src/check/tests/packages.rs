//! Package-manager builtins: `brew`, `cargo`, `winget`. Each is a
//! resource constructor that returns the unified `Package` type;
//! `Package` widens to `Resource` so package and filesystem
//! resources can mix freely in lists and reconcile arms.

use super::check_src;

#[test]
fn brew_returns_a_package() {
    assert!(check_src("val ripgrep: Package = brew(\"ripgrep\")").is_ok());
}

#[test]
fn cargo_returns_a_package() {
    assert!(check_src("val sccache: Package = cargo(\"sccache\")").is_ok());
}

#[test]
fn winget_returns_a_package() {
    assert!(check_src("val pwsh: Package = winget(\"Microsoft.PowerShell\")").is_ok());
}

#[test]
fn package_widens_to_resource() {
    // Same widening rule as Symlink/Template: a `Package`
    // fits a `Resource` slot. Lets reconcile arms and list elements
    // mix package and filesystem resources without per-kind plumbing.
    assert!(check_src("val r: Resource = brew(\"git\")").is_ok());
}

#[test]
fn mixed_resource_list_lifts_package_to_resource() {
    // A list whose elements span filesystem resources and packages
    // should infer `List<Resource>` — same lifting rule the other
    // resource singletons go through.
    assert!(
        check_src(
            "val xs = [brew(\"git\"), symlink(from = \"/a\", to = \"/b\")]\n\
             val ys: List<Resource> = xs",
        )
        .is_ok()
    );
}

#[test]
fn match_lifts_package_and_filesystem_resource_to_resource() {
    assert!(
        check_src(
            "val r: Resource = match true {\n\
             true => brew(\"git\"),\n\
             _ => symlink(from = \"/a\", to = \"/b\"),\n\
             }\n",
        )
        .is_ok()
    );
}

#[test]
fn package_is_reconcilable() {
    // `reconcile` accepts anything that satisfies `is_reconcilable`;
    // `Package` is in that set so a manifest can stand on its own
    // without wrapping the result.
    assert!(check_src("reconcile brew(\"ripgrep\")\n").is_ok());
}

#[test]
fn package_does_not_subtype_string() {
    // Sanity check: the widening rule goes one way only — Package
    // satisfies Resource, but a Package value does not silently
    // flow into a String slot.
    let err = check_src("val s: String = brew(\"ripgrep\")").expect_err("should fail");
    assert!(
        err.iter()
            .any(|d| d.message.contains("expected `String`")
                && d.message.contains("found `Package`")),
        "got: {err:?}",
    );
}

#[test]
fn brew_argument_must_be_string() {
    // The signature is `brew(name: String): Package`. An Int
    // argument is a type error, not a silent coercion to "1".
    let err = check_src("val x: Package = brew(7)").expect_err("should fail");
    assert!(
        err.iter().any(|d| d.message.contains("Int")
            && (d.message.contains("expected `String`") || d.message.contains("String"))),
        "got: {err:?}",
    );
}
