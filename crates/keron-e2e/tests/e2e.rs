#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use tempfile::TempDir;

static BUILD_KERON: OnceLock<()> = OnceLock::new();

fn keron_bin() -> PathBuf {
    let mut path = std::env::current_exe().expect("current test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("keron{}", std::env::consts::EXE_SUFFIX))
}

fn ensure_keron_built() {
    BUILD_KERON.get_or_init(|| {
        let status = Command::new("cargo")
            .arg("build")
            .arg("-q")
            .arg("-p")
            .arg("keron")
            .status()
            .expect("build keron binary");
        assert!(status.success(), "failed to build keron binary");
    });
}

fn run_apply(root: &Path, flags: &[&str]) -> Output {
    let bin = keron_bin();
    ensure_keron_built();

    let mut cmd = Command::new(bin);
    cmd.env("NO_PAGER", "1");
    cmd.arg("apply");
    cmd.arg(root);
    cmd.args(flags);
    cmd.output().expect("apply command runs")
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(1)
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn to_lua_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace('"', "\\\"")
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent directories");
    }
    fs::write(path, content).expect("write file");
}

#[test]
fn apply_detects_drift_in_tempdir() {
    let temp = TempDir::new().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    let src = temp.path().join("files/config");
    let dest = temp.path().join("out/config");

    write_file(&src, "hello\n");

    let script = format!(
        "link(\"{}\", \"{}\", {{ mkdirs = true }})",
        to_lua_path(&src),
        to_lua_path(&dest)
    );
    write_file(&manifest, &script);

    let output = run_apply(temp.path(), &[]);
    assert_eq!(
        exit_code(&output),
        1,
        "apply should report drift before execute"
    );
}

#[test]
fn apply_supports_render_flags() {
    let temp = TempDir::new().expect("tempdir");
    let manifest = temp.path().join("main.lua");
    let src = temp.path().join("files/config");
    let dest = temp.path().join("out/config");

    write_file(&src, "hello\n");
    let script = format!(
        "link(\"{}\", \"{}\", {{ mkdirs = true }})",
        to_lua_path(&src),
        to_lua_path(&dest)
    );
    write_file(&manifest, &script);

    let output = run_apply(temp.path(), &["--color", "never", "--verbose"]);
    assert_eq!(exit_code(&output), 1, "apply should still report drift");
}

#[test]
fn apply_reports_dependency_cycle() {
    let temp = TempDir::new().expect("tempdir");
    write_file(&temp.path().join("a.lua"), "depends_on(\"./b.lua\")");
    write_file(&temp.path().join("b.lua"), "depends_on(\"./a.lua\")");

    let output = run_apply(temp.path(), &[]);
    assert_eq!(
        exit_code(&output),
        1,
        "cycle should be reported as an apply error"
    );
    assert!(
        stdout_text(&output).contains("dependency cycle detected"),
        "expected cycle details in output, got:\n{}",
        stdout_text(&output)
    );
}

#[test]
fn apply_reports_missing_dependency_path() {
    let temp = TempDir::new().expect("tempdir");
    write_file(
        &temp.path().join("main.lua"),
        "depends_on(\"./missing.lua\")",
    );

    let output = run_apply(temp.path(), &[]);
    assert_eq!(
        exit_code(&output),
        1,
        "missing dependency should be reported as an apply error"
    );
    assert!(
        stdout_text(&output).contains("missing manifest"),
        "expected missing dependency details in output, got:\n{}",
        stdout_text(&output)
    );
}

#[test]
fn apply_execute_verbose_warns_for_force_replacements() {
    let temp = TempDir::new().expect("tempdir");
    let src = temp.path().join("files/profile.tmpl");
    let dest = temp.path().join("out/profile.conf");
    let manifest = temp.path().join("template.lua");

    write_file(&src, "name=sam\n");
    write_file(&dest, "stale=1\n");

    let script = format!(
        r#"
template("{}", "{}", {{
  mkdirs = true,
  force = true
}})
"#,
        to_lua_path(&src),
        to_lua_path(&dest)
    );
    write_file(&manifest, &script);

    let output = run_apply(temp.path(), &["--execute", "--verbose", "--color", "never"]);
    assert_eq!(exit_code(&output), 0, "force update should succeed");
    assert!(
        stderr_text(&output).contains("warning: plan includes force=true replacements"),
        "expected force warning in stderr, got:\n{}",
        stderr_text(&output)
    );
}

#[test]
fn apply_renders_template_into_destination() {
    let temp = TempDir::new().expect("tempdir");
    let src = temp.path().join("files/profile.tmpl");
    let dest = temp.path().join("out/profile.conf");
    let manifest = temp.path().join("template.lua");

    write_file(&src, "name={{name}}\nshell={{ shell }}\n");
    let script = format!(
        r#"
template("{}", "{}", {{
  mkdirs = true,
  force = true,
  vars = {{ name = "sam", shell = "/bin/zsh" }}
}})
"#,
        to_lua_path(&src),
        to_lua_path(&dest)
    );
    write_file(&manifest, &script);

    let plan_output = run_apply(temp.path(), &[]);
    assert_eq!(
        exit_code(&plan_output),
        1,
        "template should report pending change before apply"
    );

    let apply_output = run_apply(temp.path(), &["--execute"]);
    assert_eq!(exit_code(&apply_output), 0);

    let rendered = fs::read_to_string(&dest).expect("destination rendered");
    assert_eq!(rendered, "name=sam\nshell=/bin/zsh\n");

    let clean_plan_output = run_apply(temp.path(), &[]);
    assert_eq!(
        exit_code(&clean_plan_output),
        0,
        "apply should be clean after render"
    );
}

#[cfg(unix)]
#[test]
fn unix_apply_execute_link_is_idempotent() {
    let temp = TempDir::new().expect("tempdir");
    let src = temp.path().join("files/zshrc");
    let dest = temp.path().join("home/.zshrc");
    let manifest = temp.path().join("base.lua");

    write_file(&src, "export TEST=1\n");

    let script = format!(
        "link(\"{}\", \"{}\", {{ mkdirs = true, force = true }})",
        to_lua_path(&src),
        to_lua_path(&dest)
    );
    write_file(&manifest, &script);

    let apply_output = run_apply(temp.path(), &["--execute"]);
    assert_eq!(exit_code(&apply_output), 0);

    let link_target = fs::read_link(&dest).expect("dest should be a symlink");
    assert_eq!(
        normalize_for_compare(&link_target, &dest),
        normalize_abs(&src)
    );

    let plan_output = run_apply(temp.path(), &[]);
    assert_eq!(
        exit_code(&plan_output),
        0,
        "apply should be clean after apply"
    );
}

#[test]
fn apply_execute_stops_after_first_runtime_failure() {
    let temp = TempDir::new().expect("tempdir");
    let src = temp.path().join("files/profile.tmpl");
    let dest = temp.path().join("out/profile.conf");
    let manifest = temp.path().join("main.lua");
    write_file(&src, "name=sam\n");

    let fail_command = if cfg!(windows) {
        r#"cmd("cmd", {"/C", "exit", "1"})"#
    } else {
        r#"cmd("sh", {"-c", "exit 1"})"#
    };

    let script = format!(
        r#"
{fail_command}
template("{}", "{}", {{
  mkdirs = true,
  force = true
}})
"#,
        to_lua_path(&src),
        to_lua_path(&dest)
    );
    write_file(&manifest, &script);

    let output = run_apply(temp.path(), &["--execute"]);
    assert_eq!(
        exit_code(&output),
        1,
        "apply should fail from command resource"
    );
    assert!(
        !dest.exists(),
        "template should not run after fail-fast command failure"
    );
}

#[cfg(unix)]
#[test]
fn unix_apply_execute_force_replaces_dangling_symlink() {
    let temp = TempDir::new().expect("tempdir");
    let src = temp.path().join("files/zshrc");
    let dest = temp.path().join("home/.zshrc");
    let manifest = temp.path().join("main.lua");
    let dangling_target = temp.path().join("home/missing-zshrc");
    write_file(&src, "export TEST=1\n");
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).expect("create destination parent");
    }
    std::os::unix::fs::symlink(&dangling_target, &dest).expect("create dangling symlink");

    let script = format!(
        "link(\"{}\", \"{}\", {{ force = true }})",
        to_lua_path(&src),
        to_lua_path(&dest)
    );
    write_file(&manifest, &script);

    let output = run_apply(temp.path(), &["--execute"]);
    assert_eq!(
        exit_code(&output),
        0,
        "force=true should replace dangling symlink"
    );

    let link_target = fs::read_link(&dest).expect("destination should be a symlink");
    assert_eq!(
        normalize_for_compare(&link_target, &dest),
        normalize_abs(&src)
    );
}

#[cfg(unix)]
fn normalize_for_compare(path: &Path, link_path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_abs(path)
    } else {
        normalize_abs(&link_path.parent().expect("link parent").join(path))
    }
}

#[cfg(unix)]
fn normalize_abs(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
