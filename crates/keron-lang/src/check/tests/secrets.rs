//! `Secret` marker type: `secret(...)` and `unwrap_secret(...)` plus
//! the strict subtyping/operator rules that prevent a secret from
//! flowing into a String sink without an explicit unwrap.

use super::check_src;

#[test]
fn unwrap_secret_returns_string() {
    // Canonical use site: a secret round-trips through
    // `unwrap_secret` and lands in a String slot.
    assert!(
        check_src(
            "val token: Secret = secret(\"op://Vault/Item/field\")\n\
             val text: String = unwrap_secret(token)",
        )
        .is_ok()
    );
}

#[test]
fn secret_is_not_assignable_to_string() {
    // The whole point of the marker type: a `Secret` cannot silently
    // flow into a `String` slot. The user must `unwrap_secret`.
    let err = check_src("val s: String = secret(\"op://Vault/Item/x\")").expect_err("should fail");
    assert!(
        err.iter().any(
            |d| d.message.contains("expected `String`") && d.message.contains("found `Secret`")
        ),
        "got: {err:?}",
    );
}

#[test]
fn secret_in_interpolation_is_rejected() {
    // Interpolation is a string sink — it must reject `Secret` for
    // the same reason it rejects nullable: the user has to opt in.
    // The diagnostic is the generic "type mismatch" message because
    // interpolation only accepts `String` (not nullable, not Secret),
    // and the failure surfaces there. Either shape is acceptable as
    // long as it points the user at the wrong type.
    let err = check_src(
        "val tok: Secret = secret(\"op://x/y/z\")\n\
         val s: String = \"hi ${tok}\"",
    )
    .expect_err("interpolation of Secret should fail");
    assert!(
        err.iter().any(|d| d.message.contains("Secret")),
        "got: {err:?}",
    );
}

#[test]
fn secret_in_concat_is_rejected() {
    let err = check_src(
        "val tok: Secret = secret(\"op://x/y/z\")\n\
         val s: String = tok + \"-suffix\"",
    )
    .expect_err("Secret + String should fail");
    assert!(
        err.iter().any(|d| d.message.contains("Secret")),
        "got: {err:?}",
    );
}

#[test]
fn secret_eq_secret_is_boolean() {
    // Two secrets can be compared; produces a Boolean. Useful for
    // configs that branch on "are these two refs the same secret"
    // (e.g. validating consistency across multiple resources).
    assert!(
        check_src(
            "val a: Secret = secret(\"op://x/y/z\")\n\
             val b: Secret = secret(\"op://x/y/z\")\n\
             val same: Boolean = a == b",
        )
        .is_ok()
    );
}

#[test]
fn secret_eq_string_is_rejected() {
    // The escape hatch is `unwrap_secret`, not `==`. Cross-type
    // equality with a String literal would let a user probe the
    // value via the type system.
    let err = check_src(
        "val tok: Secret = secret(\"op://x/y/z\")\n\
         val matches: Boolean = tok == \"hunter2\"",
    )
    .expect_err("Secret == String should fail");
    assert!(
        err.iter().any(|d| d.message.contains("Secret")),
        "got: {err:?}",
    );
}

#[test]
fn map_key_cannot_be_secret() {
    // Secret keys would have to be hashed/serialized to be used as
    // map keys, and round-tripping through string-shaped paths
    // negates the whole marker-type contract. Reuse the existing
    // `Map` key restriction.
    let err = check_src("val m: Map<Secret, Int> = {}").expect_err("Secret map key should fail");
    assert!(
        err.iter()
            .any(|d| d.message.contains("not a valid `Map` key type")),
        "got: {err:?}",
    );
}
