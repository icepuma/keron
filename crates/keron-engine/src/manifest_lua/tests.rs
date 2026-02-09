#![allow(clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use keron_domain::{PackageState, Resource};

use super::evaluate_manifest;

fn lua_escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn unique_missing_env_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be monotonic")
        .as_nanos();
    format!("KERON_MISSING_ENV_{nanos}")
}

#[test]
fn collects_declarative_state_from_lua() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    let dest = temp.path().join("home/.zshrc");

    let script = r#"
depends_on("./base.lua")
link("files/zshrc", "__DEST__", { mkdirs = true })
package("git", { provider = "brew", state = "present" })
packages({ "fd", "ripgrep" }, { provider = "brew" })
cmd("echo", { "hello" })
"#
    .replace("__DEST__", &lua_escape(&dest));

    fs::write(&manifest, script).expect("write manifest");

    let spec = evaluate_manifest(&manifest).expect("manifest eval");
    assert_eq!(spec.dependencies.len(), 1);
    assert_eq!(spec.resources.len(), 5);

    match &spec.resources[1] {
        Resource::Package(package) => {
            assert_eq!(package.name, "git");
            assert_eq!(package.provider_hint.as_deref(), Some("brew"));
            assert_eq!(package.state, PackageState::Present);
        }
        _ => unreachable!("expected package resource"),
    }

    match &spec.resources[2] {
        Resource::Package(package) => {
            assert_eq!(package.name, "fd");
            assert_eq!(package.provider_hint.as_deref(), Some("brew"));
        }
        _ => unreachable!("expected package resource"),
    }
}

#[test]
fn pkg_alias_is_not_supported() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r#"pkg("git")"#).expect("write manifest");

    let error = evaluate_manifest(&manifest).expect_err("pkg should fail");
    assert!(
        error.to_string().contains("pkg"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn uses_lua_5_4_runtime() {
    let lua = super::create_lua().expect("lua runtime");
    let version: String = lua.globals().get("_VERSION").expect("lua version");
    assert!(version.contains("Lua 5.4"), "unexpected version: {version}");
}

#[test]
fn parses_template_resource() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    let src = temp.path().join("files/config.tmpl");
    let dest = temp.path().join("out/config");

    fs::create_dir_all(src.parent().expect("parent")).expect("mkdir");
    fs::write(&src, "hello {{name}}").expect("write template");
    fs::write(
        &manifest,
        format!(
            r#"
template("{}", "{}", {{
  mkdirs = true,
  force = true,
  vars = {{ name = "keron" }}
}})
"#,
            lua_escape(&src),
            lua_escape(&dest)
        ),
    )
    .expect("write manifest");

    let spec = evaluate_manifest(&manifest).expect("manifest eval");
    assert_eq!(spec.resources.len(), 1);

    match &spec.resources[0] {
        Resource::Template(template) => {
            assert_eq!(template.src, src);
            assert_eq!(template.dest, dest);
            assert_eq!(template.vars.get("name").map(String::as_str), Some("keron"));
            assert!(template.force);
            assert!(template.mkdirs);
        }
        _ => unreachable!("expected template resource"),
    }
}

#[test]
fn env_function_uses_process_environment_values() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r#"cmd(env("PATH"))"#).expect("write manifest");

    let spec = evaluate_manifest(&manifest).expect("manifest eval");
    assert_eq!(spec.resources.len(), 1);

    match &spec.resources[0] {
        Resource::Command(command) => {
            let expected = std::env::var_os("PATH")
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            assert_eq!(command.binary, expected);
        }
        _ => unreachable!("expected command resource"),
    }
}

#[test]
fn env_function_errors_when_variable_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    let missing = unique_missing_env_name();

    fs::write(&manifest, format!(r#"cmd(env("{missing}"))"#)).expect("write manifest");

    let error = evaluate_manifest(&manifest).expect_err("missing env should fail");
    assert!(
        error.to_string().contains(&missing),
        "unexpected error: {error:#}"
    );
}

#[test]
fn env_function_rejects_default_argument() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r#"cmd(env("HOME", "fallback"))"#).expect("write manifest");

    let error = evaluate_manifest(&manifest).expect_err("env defaults should fail");
    assert!(
        error
            .to_string()
            .contains("env(name) expects exactly one string argument"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn is_macos_returns_bool() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r"cmd(tostring(is_macos()))").expect("write manifest");

    let spec = evaluate_manifest(&manifest).expect("manifest eval");
    assert_eq!(spec.resources.len(), 1);

    match &spec.resources[0] {
        Resource::Command(command) => {
            assert!(
                command.binary == "true" || command.binary == "false",
                "expected 'true' or 'false', got: {}",
                command.binary
            );
        }
        _ => unreachable!("expected command resource"),
    }
}

#[test]
fn is_linux_returns_bool() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r"cmd(tostring(is_linux()))").expect("write manifest");

    let spec = evaluate_manifest(&manifest).expect("manifest eval");
    assert_eq!(spec.resources.len(), 1);

    match &spec.resources[0] {
        Resource::Command(command) => {
            assert!(
                command.binary == "true" || command.binary == "false",
                "expected 'true' or 'false', got: {}",
                command.binary
            );
        }
        _ => unreachable!("expected command resource"),
    }
}

#[test]
fn is_windows_returns_bool() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r"cmd(tostring(is_windows()))").expect("write manifest");

    let spec = evaluate_manifest(&manifest).expect("manifest eval");
    assert_eq!(spec.resources.len(), 1);

    match &spec.resources[0] {
        Resource::Command(command) => {
            assert!(
                command.binary == "true" || command.binary == "false",
                "expected 'true' or 'false', got: {}",
                command.binary
            );
        }
        _ => unreachable!("expected command resource"),
    }
}

// --- secret() URI parsing unit tests ---

#[test]
fn parse_secret_uri_op_scheme() {
    let provider = crate::secrets::parse_secret_uri("op://vault/item/field").expect("parse");
    assert_eq!(provider.binary, "op");
    assert_eq!(provider.args, vec!["read", "op://vault/item/field"]);
}

#[test]
fn parse_secret_uri_bw_with_field() {
    let provider = crate::secrets::parse_secret_uri("bw://my-login/username").expect("parse");
    assert_eq!(provider.binary, "bw");
    assert_eq!(provider.args, vec!["get", "username", "my-login"]);
}

#[test]
fn parse_secret_uri_bw_default_field() {
    let provider = crate::secrets::parse_secret_uri("bw://my-login").expect("parse");
    assert_eq!(provider.binary, "bw");
    assert_eq!(provider.args, vec!["get", "password", "my-login"]);
}

#[test]
fn parse_secret_uri_pp_scheme() {
    let provider = crate::secrets::parse_secret_uri("pp://v/i/f").expect("parse");
    assert_eq!(provider.binary, "pass-cli");
    assert_eq!(provider.args, vec!["view", "pass://v/i/f"]);
}

#[test]
fn parse_secret_uri_rejects_missing_scheme() {
    let error = crate::secrets::parse_secret_uri("just-a-string").expect_err("should fail");
    assert!(error.contains("invalid URI"), "unexpected error: {error}");
}

#[test]
fn parse_secret_uri_rejects_unknown_scheme() {
    let error = crate::secrets::parse_secret_uri("vault://foo").expect_err("should fail");
    assert!(
        error.contains("unsupported scheme"),
        "unexpected error: {error}"
    );
}

#[test]
fn parse_secret_uri_bw_rejects_empty_item() {
    let error = crate::secrets::parse_secret_uri("bw:///field").expect_err("should fail");
    assert!(error.contains("bw://"), "unexpected error: {error}");
}

#[test]
fn parse_secret_uri_pp_requires_full_path() {
    let error = crate::secrets::parse_secret_uri("pp://vault/item").expect_err("should fail");
    assert!(error.contains("pp://"), "unexpected error: {error}");
}

// --- secret() integration tests ---

#[test]
fn secret_rejects_unknown_scheme() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r#"cmd(secret("vault://x"))"#).expect("write manifest");

    let error = evaluate_manifest(&manifest).expect_err("should fail");
    assert!(
        error.to_string().contains("unsupported scheme"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn secret_rejects_invalid_uri() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r#"cmd(secret("no-scheme"))"#).expect("write manifest");

    let error = evaluate_manifest(&manifest).expect_err("should fail");
    assert!(
        error.to_string().contains("invalid URI"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn secret_rejects_multiple_arguments() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r#"cmd(secret("op://x", "extra"))"#).expect("write manifest");

    let error = evaluate_manifest(&manifest).expect_err("should fail");
    assert!(
        error
            .to_string()
            .contains("secret(uri) expects exactly one string argument"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn secret_errors_when_cli_not_installed() {
    // Skip this test if `op` is actually installed.
    if std::process::Command::new("op")
        .arg("--version")
        .output()
        .is_ok()
    {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r#"cmd(secret("op://vault/item/field"))"#).expect("write manifest");

    let error = evaluate_manifest(&manifest).expect_err("should fail");
    assert!(
        error.to_string().contains(r#"requires the "op" CLI"#),
        "unexpected error: {error:#}"
    );
}

#[test]
fn os_functions_enable_conditional_resources() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(
        &manifest,
        r#"
if is_macos() then cmd("mac") end
if is_linux() then cmd("linux") end
if is_windows() then cmd("windows") end
"#,
    )
    .expect("write manifest");

    let spec = evaluate_manifest(&manifest).expect("manifest eval");

    // Exactly one OS should match on any host.
    assert_eq!(
        spec.resources.len(),
        1,
        "expected exactly one resource, got: {:?}",
        spec.resources
    );

    let expected = match std::env::consts::OS {
        "macos" => "mac",
        "linux" => "linux",
        "windows" => "windows",
        other => unreachable!("unsupported OS for this test: {other}"),
    };

    match &spec.resources[0] {
        Resource::Command(command) => {
            assert_eq!(command.binary, expected);
        }
        _ => unreachable!("expected command resource"),
    }
}

#[test]
fn env_value_is_collected_as_sensitive() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    fs::write(&manifest, r#"cmd(env("PATH"))"#).expect("write manifest");

    let (_spec, _warnings, sensitive) =
        super::evaluate_manifest_with_warnings(&manifest).expect("manifest eval");

    let path = std::env::var("PATH").expect("PATH should be set");
    assert!(
        sensitive.contains(&path),
        "expected sensitive set to contain PATH value, got: {sensitive:?}"
    );
}
