//! Type-check coverage for the `keys` module — `ssh_key` and
//! `gpg_key`. The shape these tests pin down: the encrypted material
//! params (`ssh_key(private = …)` and `gpg_key(key = …)`) are
//! `Type::Secret`, so a bare String literal must fail to typecheck.

use super::check_src;

#[test]
fn ssh_key_accepts_secret_for_private() {
    assert!(
        check_src(
            "val k: SshKey = ssh_key(\n\
             \tprivate_path = \"/p\",\n\
             \tpublic_path = \"/p.pub\",\n\
             \tprivate = secret(\"op://k/test\"),\n\
             \tpublic = \"ssh-ed25519 AAAA u@h\",\n\
             )",
        )
        .is_ok()
    );
}

#[test]
fn ssh_key_rejects_raw_string_for_private() {
    // The whole point of the `Secret`-typed param: a bare String
    // literal carrying private-key material would silently dilute the
    // audit trail. The typechecker must surface this as a type error.
    let err = check_src(
        "val k: SshKey = ssh_key(\n\
         \tprivate_path = \"/p\",\n\
         \tpublic_path = \"/p.pub\",\n\
         \tprivate = \"-----BEGIN ...\",\n\
         \tpublic = \"ssh-ed25519 AAAA u@h\",\n\
         )",
    )
    .expect_err("raw String for `private` should fail");
    assert!(
        err.iter().any(
            |d| d.message.contains("expected `Secret`") && d.message.contains("found `String`")
        ),
        "got: {err:?}",
    );
}

#[test]
fn ssh_key_widens_to_resource() {
    // Same one-way subtyping rule as `Symlink` / `Template`: a
    // specific `SshKey` fits a `Resource` slot.
    assert!(
        check_src(
            "val r: Resource = ssh_key(\n\
             \tprivate_path = \"/p\",\n\
             \tpublic_path = \"/p.pub\",\n\
             \tprivate = secret(\"op://k/test\"),\n\
             \tpublic = \"ssh-ed25519 AAAA u@h\",\n\
             )",
        )
        .is_ok()
    );
}

#[test]
fn gpg_key_accepts_secret_for_key() {
    assert!(
        check_src(
            "val g: GpgKey = gpg_key(fingerprint = \"ABCD1234\", key = secret(\"op://k/gpg\"))",
        )
        .is_ok()
    );
}

#[test]
fn gpg_key_rejects_raw_string_for_key() {
    let err = check_src(
        "val g: GpgKey = gpg_key(fingerprint = \"ABCD1234\", key = \"-----BEGIN PGP...\")",
    )
    .expect_err("raw String for `key` should fail");
    assert!(
        err.iter().any(
            |d| d.message.contains("expected `Secret`") && d.message.contains("found `String`")
        ),
        "got: {err:?}",
    );
}

#[test]
fn gpg_key_widens_to_resource() {
    assert!(
        check_src(
            "val r: Resource = gpg_key(fingerprint = \"ABCD1234\", key = secret(\"op://k/gpg\"))",
        )
        .is_ok()
    );
}

#[test]
fn reconcile_accepts_ssh_key_and_gpg_key() {
    // Both kinds must be reconcilable via the standard `is_reconcilable`
    // rule that drives the `reconcile` statement's RHS typing.
    assert!(
        check_src(
            "val k: SshKey = ssh_key(\n\
             \tprivate_path = \"/p\",\n\
             \tpublic_path = \"/p.pub\",\n\
             \tprivate = secret(\"op://k/ssh\"),\n\
             \tpublic = \"ssh-ed25519 AAAA u@h\",\n\
             )\n\
             val g: GpgKey = gpg_key(fingerprint = \"ABCD\", key = secret(\"op://k/gpg\"))\n\
             reconcile k\n\
             reconcile g",
        )
        .is_ok()
    );
}
