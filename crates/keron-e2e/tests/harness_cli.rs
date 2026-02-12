#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use keron_e2e::harness::{run_apply, to_lua_path, write_file};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn run(root: &Path, flags: &[&str]) -> keron_e2e::harness::RunResult {
    let mut run_flags = vec!["--verbose"];
    run_flags.extend_from_slice(flags);
    let output = run_apply(root, &run_flags, &[]).expect("run keron apply");
    println!("{}", output.transcript());
    output
}

fn run_with_env(
    root: &Path,
    flags: &[&str],
    env_overrides: &[(String, String)],
) -> keron_e2e::harness::RunResult {
    let mut run_flags = vec!["--verbose"];
    run_flags.extend_from_slice(flags);
    let output =
        run_apply(root, &run_flags, env_overrides).expect("run keron apply with env overrides");
    println!("{}", output.transcript());
    output
}

fn to_lua_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
fn host_package_manager_fixture() -> (&'static str, [String; 3]) {
    (
        "apt",
        [
            "cowsay".to_string(),
            "fortune-mod".to_string(),
            "toilet".to_string(),
        ],
    )
}

#[cfg(target_os = "macos")]
fn host_package_manager_fixture() -> (&'static str, [String; 3]) {
    (
        "brew",
        [
            "cowsay".to_string(),
            "toilet".to_string(),
            "figlet".to_string(),
        ],
    )
}

#[cfg(target_os = "windows")]
fn host_package_manager_fixture() -> (&'static str, [String; 3]) {
    (
        "winget",
        [
            "BurntSushi.ripgrep.MSVC".to_string(),
            "sharkdp.fd".to_string(),
            "dandavison.delta".to_string(),
        ],
    )
}

fn write_multi_manifest_templates(
    root: &Path,
    manager: &str,
    packages: [&str; 3],
) -> (PathBuf, PathBuf, PathBuf) {
    let out_dir = root.join("out");
    let base_guarded_packages = guarded_package_block(manager, packages[0]);
    let dev_guarded_packages = guarded_package_block(manager, packages[1]);
    let workstation_guarded_packages = guarded_package_block(manager, packages[2]);

    let base_src = root.join("files/base.tmpl");
    let dev_src = root.join("files/dev.tmpl");
    let workstation_src = root.join("files/workstation.tmpl");
    let base_dest = out_dir.join("base.conf");
    let dev_dest = out_dir.join("dev.conf");
    let workstation_dest = out_dir.join("workstation.conf");

    write_file(&base_src, "layer={{layer}}\n").expect("write base template");
    write_file(&dev_src, "layer={{layer}}\n").expect("write dev template");
    write_file(&workstation_src, "layer={{layer}}\n").expect("write workstation template");

    write_file(
        &root.join("base.lua"),
        &format!(
            r#"
{}
template("{}", "{}", {{
  mkdirs = true,
  force = true,
  elevate = true,
  vars = {{ layer = "base" }}
}})
"#,
            base_guarded_packages,
            to_lua_path(&base_src),
            to_lua_path(&base_dest)
        ),
    )
    .expect("write base manifest");

    write_file(
        &root.join("dev.lua"),
        &format!(
            r#"
depends_on("./base.lua")
{}
template("{}", "{}", {{
  mkdirs = true,
  force = true,
  elevate = true,
  vars = {{ layer = "dev" }}
}})
"#,
            dev_guarded_packages,
            to_lua_path(&dev_src),
            to_lua_path(&dev_dest)
        ),
    )
    .expect("write dev manifest");

    write_file(
        &root.join("workstation.lua"),
        &format!(
            r#"
depends_on("./dev.lua")
{}
template("{}", "{}", {{
  mkdirs = true,
  force = true,
  elevate = true,
  vars = {{ layer = "workstation" }}
}})
"#,
            workstation_guarded_packages,
            to_lua_path(&workstation_src),
            to_lua_path(&workstation_dest)
        ),
    )
    .expect("write workstation manifest");

    (base_dest, dev_dest, workstation_dest)
}

fn guarded_package_block(current_manager: &str, package: &str) -> String {
    let package_lua = to_lua_string(package);
    let placeholder = "keron-unused-package".to_string();

    let linux_package = if current_manager == "apt" {
        package_lua.clone()
    } else {
        placeholder.clone()
    };
    let macos_package = if current_manager == "brew" {
        package_lua.clone()
    } else {
        placeholder.clone()
    };
    let windows_package = if current_manager == "winget" {
        package_lua
    } else {
        placeholder
    };

    format!(
        r#"
if is_linux() then
  install_packages("apt", {{ "{linux_package}" }}, {{ state = "present", elevate = true }})
elseif is_macos() then
  install_packages("brew", {{ "{macos_package}" }}, {{ state = "present", elevate = true }})
elseif is_windows() then
  install_packages("winget", {{ "{windows_package}" }}, {{ state = "present", elevate = true }})
end
"#
    )
}

fn assert_plan_includes_manifest_order_and_packages(
    output: &keron_e2e::harness::RunResult,
    manager: &str,
    packages: [&str; 3],
) {
    let base_pos = output
        .stdout
        .find("base.lua")
        .expect("expected base.lua in verbose plan output");
    let dev_pos = output
        .stdout
        .find("dev.lua")
        .expect("expected dev.lua in verbose plan output");
    let workstation_pos = output
        .stdout
        .find("workstation.lua")
        .expect("expected workstation.lua in verbose plan output");
    assert!(
        base_pos < dev_pos && dev_pos < workstation_pos,
        "{}",
        output.transcript()
    );

    assert!(
        output.stdout.contains("install package") || output.stdout.contains("package up to date"),
        "{}",
        output.transcript()
    );
    assert!(
        output.stdout.contains(&format!("via {manager}")),
        "{}",
        output.transcript()
    );
    assert!(
        output.stdout.contains(packages[0]),
        "{}",
        output.transcript()
    );
    assert!(
        output.stdout.contains(packages[1]),
        "{}",
        output.transcript()
    );
    assert!(
        output.stdout.contains(packages[2]),
        "{}",
        output.transcript()
    );
}

fn assert_rendered_layers(base_dest: &Path, dev_dest: &Path, workstation_dest: &Path) {
    assert_eq!(
        fs::read_to_string(base_dest).expect("read base destination"),
        "layer=base\n"
    );
    assert_eq!(
        fs::read_to_string(dev_dest).expect("read dev destination"),
        "layer=dev\n"
    );
    assert_eq!(
        fs::read_to_string(workstation_dest).expect("read workstation destination"),
        "layer=workstation\n"
    );
}

#[test]
fn dry_run_clean_returns_0() {
    let temp = TempDir::new().expect("tempdir");
    write_file(&temp.path().join("main.lua"), "-- no-op").expect("write manifest");

    let output = run(temp.path(), &[]);

    assert_eq!(output.exit_code, 0, "{}", output.transcript());
    assert!(
        output.stdout.contains("Nothing to do."),
        "{}",
        output.transcript()
    );
}

#[test]
fn dry_run_drift_returns_2_and_reports_diff() {
    let temp = TempDir::new().expect("tempdir");
    let src = temp.path().join("files/config");
    let dest = temp.path().join("out/config");
    write_file(&src, "hello\n").expect("write source");
    write_file(
        &temp.path().join("main.lua"),
        &format!(
            "link(\"{}\", \"{}\", {{ mkdirs = true }})",
            to_lua_path(&src),
            to_lua_path(&dest)
        ),
    )
    .expect("write manifest");

    let output = run(temp.path(), &[]);

    assert_eq!(output.exit_code, 2, "{}", output.transcript());
    assert!(
        output.stdout.contains("create link"),
        "{}",
        output.transcript()
    );
    assert!(output.stdout.contains("#1"), "{}", output.transcript());
    assert!(
        !dest.exists(),
        "dry-run should not mutate destination: {}\n{}",
        dest.display(),
        output.transcript()
    );
}

#[test]
fn dry_run_error_returns_1() {
    let temp = TempDir::new().expect("tempdir");
    write_file(&temp.path().join("a.lua"), "depends_on(\"./b.lua\")").expect("write a.lua");
    write_file(&temp.path().join("b.lua"), "depends_on(\"./a.lua\")").expect("write b.lua");

    let output = run(temp.path(), &[]);

    assert_eq!(output.exit_code, 1, "{}", output.transcript());
    assert!(
        output.stdout.contains("dependency cycle detected"),
        "{}",
        output.transcript()
    );
}

#[test]
fn execute_success_returns_0_and_applies_content_diff() {
    let temp = TempDir::new().expect("tempdir");
    let src = temp.path().join("files/profile.tmpl");
    let dest = temp.path().join("out/profile.conf");
    write_file(&src, "name={{name}}\n").expect("write template source");
    write_file(&dest, "name=stale\n").expect("write stale destination");
    let stale = fs::read_to_string(&dest).expect("read stale destination");
    write_file(
        &temp.path().join("main.lua"),
        &format!(
            r#"
template("{}", "{}", {{
  mkdirs = true,
  force = true,
  vars = {{ name = "sam" }}
}})
"#,
            to_lua_path(&src),
            to_lua_path(&dest)
        ),
    )
    .expect("write manifest");

    let plan_output = run(temp.path(), &[]);
    assert_eq!(plan_output.exit_code, 2, "{}", plan_output.transcript());
    assert!(
        plan_output.stdout.contains("rerender template"),
        "{}",
        plan_output.transcript()
    );

    let apply_output = run(temp.path(), &["--execute"]);
    assert_eq!(apply_output.exit_code, 0, "{}", apply_output.transcript());

    let rendered = fs::read_to_string(&dest).expect("read rendered destination");
    assert_eq!(rendered, "name=sam\n");
    assert_ne!(rendered, stale);

    let clean_output = run(temp.path(), &[]);
    assert_eq!(clean_output.exit_code, 0, "{}", clean_output.transcript());
}

#[test]
fn execute_failure_returns_1_and_stops_followups() {
    let temp = TempDir::new().expect("tempdir");
    let src = temp.path().join("files/profile.tmpl");
    let dest = temp.path().join("out/profile.conf");
    write_file(&src, "name=sam\n").expect("write template source");
    write_file(
        &temp.path().join("main.lua"),
        &format!(
            r#"
{}
template("{}", "{}", {{
  mkdirs = true,
  force = true
}})
"#,
            if cfg!(windows) {
                r#"cmd("cmd", {"/C", "exit", "7"})"#
            } else {
                r#"cmd("sh", {"-c", "exit 7"})"#
            },
            to_lua_path(&src),
            to_lua_path(&dest)
        ),
    )
    .expect("write manifest");

    let output = run(temp.path(), &["--execute"]);

    assert_eq!(output.exit_code, 1, "{}", output.transcript());
    assert!(
        output.stdout.contains("failed command"),
        "{}",
        output.transcript()
    );
    assert!(
        !dest.exists(),
        "destination should not be written after fail-fast failure: {}\n{}",
        dest.display(),
        output.transcript()
    );
}

#[test]
fn proton_pass_secret_works_with_pass_cli_shim() {
    let temp = TempDir::new().expect("tempdir");
    let src = temp.path().join("files/profile.tmpl");
    let dest = temp.path().join("out/profile.conf");
    let manifest = temp.path().join("main.lua");
    let bin_dir = temp.path().join("bin");
    let pass_cli = if cfg!(windows) {
        bin_dir.join("pass-cli.cmd")
    } else {
        bin_dir.join("pass-cli")
    };

    fs::create_dir_all(&bin_dir).expect("create shim dir");
    let script = if cfg!(windows) {
        r#"@echo off
if "%~1" neq "item" exit /b 65
if "%~2" neq "view" exit /b 65
set "uri=%~3"
if /I not "%uri:~0,7%"=="pass://" exit /b 66
<nul set /p =proton-user
exit /b 0
"#
    } else {
        r#"#!/usr/bin/env bash
if [ "$#" -ne 3 ]; then
  echo "unexpected arg count: $#" >&2
  exit 64
fi
if [ "$1" != "item" ] || [ "$2" != "view" ]; then
  echo "unexpected command: $1 $2" >&2
  exit 65
fi
case "$3" in
  pass://*) ;;
  *)
    echo "unexpected uri: $3" >&2
    exit 66
    ;;
esac
printf 'proton-user'
"#
    };
    write_file(&pass_cli, script).expect("write pass-cli shim");

    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&pass_cli)
            .expect("read pass-cli metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&pass_cli, permissions).expect("chmod pass-cli");
    }

    write_file(&src, "username={{username}}\n").expect("write template source");
    write_file(
        &manifest,
        &format!(
            r#"
template("{}", "{}", {{
  mkdirs = true,
  force = true,
  vars = {{ username = secret("pp://Personal/test/username") }}
}})
"#,
            to_lua_path(&src),
            to_lua_path(&dest)
        ),
    )
    .expect("write manifest");

    let existing_path = std::env::var("PATH").unwrap_or_default();
    let path_separator = if cfg!(windows) { ';' } else { ':' };
    let shim_path = format!("{}{}{}", bin_dir.display(), path_separator, existing_path);
    let env_overrides = vec![("PATH".to_string(), shim_path)];

    let output = run_with_env(temp.path(), &["--execute"], &env_overrides);
    assert_eq!(output.exit_code, 0, "{}", output.transcript());
    assert_eq!(
        fs::read_to_string(&dest).expect("read rendered destination"),
        "username=proton-user\n"
    );
}

#[test]
fn multi_manifest_dependency_chain_executes_end_to_end() {
    let temp = TempDir::new().expect("tempdir");
    let (manager, [base_package, dev_package, workstation_package]) =
        host_package_manager_fixture();
    let (base_dest, dev_dest, workstation_dest) = write_multi_manifest_templates(
        temp.path(),
        manager,
        [&base_package, &dev_package, &workstation_package],
    );

    let plan_output = run(temp.path(), &[]);
    assert_eq!(plan_output.exit_code, 2, "{}", plan_output.transcript());
    assert_plan_includes_manifest_order_and_packages(
        &plan_output,
        manager,
        [&base_package, &dev_package, &workstation_package],
    );

    let apply_output = run(temp.path(), &["--execute"]);
    assert!(
        apply_output.exit_code == 0 || apply_output.exit_code == 1,
        "{}",
        apply_output.transcript()
    );

    let clean_output = run(temp.path(), &[]);
    if apply_output.exit_code == 0 {
        assert_rendered_layers(&base_dest, &dev_dest, &workstation_dest);
        assert_eq!(clean_output.exit_code, 0, "{}", clean_output.transcript());
    } else {
        assert!(
            apply_output.stdout.contains("failed"),
            "{}",
            apply_output.transcript()
        );
        assert_eq!(clean_output.exit_code, 2, "{}", clean_output.transcript());
    }
}
