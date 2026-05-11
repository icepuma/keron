//! Cross-platform end-to-end tests for `keron apply`.
//!
//! Drives the real `keron` binary against realistic dotfile fixtures
//! and asserts the resulting filesystem state. Runs on every OS the
//! GitHub Actions matrix covers (Linux, macOS, Windows); per-platform
//! quirks are gated with `cfg` blocks.
//!
//! Conventions:
//!
//! - Each test isolates its own filesystem state via a per-process
//!   tempdir under `env::temp_dir()` and treats that as the test's
//!   fake `$HOME` — every symlink keron creates from `${env("HOME")}/...`
//!   lands inside the tempdir, so the host's real `$HOME` is never
//!   touched.
//! - The `keron` binary is built on demand the first time a test
//!   needs it (cargo doesn't auto-build other crates' bins for
//!   integration tests).
//! - Package tests use the `KERON_TEST_BREW_PACKAGES` cache seam +
//!   `KERON_TEST_PACKAGE_BIN_*` install seam from `keron-apply`, so
//!   no real package manager is ever invoked.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// Owns a unique tempdir for one test. Doubles as the fake `$HOME`
/// the keron child uses to resolve `env("HOME")` interpolations in
/// fixture manifests. Cleaned up on Drop, even if a test panics —
/// which keeps repeated local runs from accumulating stale state.
struct E2eHome {
    path: PathBuf,
}

impl E2eHome {
    fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("keron-e2e-{tag}-{}-{n}", std::process::id()));
        if path.exists() {
            let _ = fs::remove_dir_all(&path);
        }
        fs::create_dir_all(&path).expect("create fake HOME");
        let canonical = fs::canonicalize(&path).expect("canonicalize fake HOME");
        Self {
            path: strip_unc_prefix(canonical),
        }
    }
}

/// On Windows `fs::canonicalize` returns a path with the `\\?\`
/// extended-length prefix. That prefix specifically rejects forward
/// slashes, but keron manifests use `/` as their path separator
/// (`"${home}/.testrc"`), so the post-interpolation path would mix
/// `\\?\` + `/` and Windows refuses to open it (`os error 123`).
/// Strip the prefix on Windows so the fake-HOME path keeps Windows
/// semantics without the UNC trap. No-op on Unix.
#[cfg(windows)]
fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\")
        .map_or_else(|| path.clone(), PathBuf::from)
}

#[cfg(not(windows))]
const fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    path
}

impl Drop for E2eHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Absolute path to the directory containing the fixture named
/// `name` under `crates/keron-cli/tests/fixtures/`.
fn fixture_dir(name: &str) -> PathBuf {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    crate_root.join("tests").join("fixtures").join(name)
}

/// Path to the freshly-built `keron` binary for this test run.
///
/// `CARGO_BIN_EXE_<name>` is set by cargo at compile time as the
/// path to the binary `<name>` produced by the *same* package as the
/// integration test. Because `tests/e2e.rs` lives in `keron-cli`
/// alongside the `keron` binary target, this constant resolves to
/// exactly the binary cargo just built — no walking, no fallback, no
/// risk of picking up a stale binary from a sibling `CARGO_TARGET_DIR`.
fn keron_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_keron"))
}

/// Build a `Command` for `keron apply` with the fake HOME wired in.
/// Each test layers additional env / args on top via the returned
/// builder, so a single helper covers both the basic-symlinks and
/// packages flows without per-test boilerplate.
fn keron_apply(fixture: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(keron_binary());
    cmd.args(["apply", "--execute"])
        .arg(fixture)
        .env("HOME", home)
        // Belt-and-suspenders: tests must never see leftover SUDO_*
        // from the surrounding shell, which would trip the
        // direct-elevation refusal path.
        .env_remove("SUDO_UID")
        .env_remove("SUDO_GID")
        // Some CI environments (notably macOS GH Actions) preset
        // these; clear so the dotfile manifest sees the test's
        // chosen HOME via `env("HOME")` only.
        .env_remove("USERPROFILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run the prepared command, feeding `stdin_text` to the prompt.
/// Echoes the captured keron stdout / stderr through the test's own
/// stdout so `cargo nextest run --success-output=immediate-final`
/// surfaces the plan + apply transcript in the CI log alongside each
/// passing test — replaces the separate "demo" step that used to run
/// keron once for visibility.
fn run(mut cmd: Command, stdin_text: &str) -> Output {
    let mut child = cmd.spawn().expect("spawning keron");
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(stdin_text.as_bytes())
        .expect("writing prompt input");
    let out = child.wait_with_output().expect("waiting on keron");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    println!("--- keron stdout ---\n{stdout}");
    if !stderr.is_empty() {
        println!("--- keron stderr ---\n{stderr}");
    }
    Output {
        success: out.status.success(),
        stdout,
        stderr,
    }
}

#[test]
fn basic_dotfiles_creates_symlinks_then_is_idempotent() {
    // On Windows, symlink creation requires either admin or
    // Developer Mode. GitHub Actions windows runners run as admin
    // so this passes; a non-admin Windows dev needs Developer Mode.
    let home = E2eHome::new("basic");
    let fixture = fixture_dir("basic-dotfiles");

    let first = run(keron_apply(&fixture, &home.path), "yes\n");
    assert!(
        first.success,
        "first run failed: stdout=\n{}\nstderr=\n{}",
        first.stdout, first.stderr,
    );
    assert!(
        first.stdout.contains("4 added"),
        "first run should add 4 symlinks: {}",
        first.stdout,
    );
    let zshrc = home.path.join(".testrc");
    assert!(zshrc.is_symlink(), "missing ~/.testrc: {}", first.stdout);
    let gitconfig = home.path.join(".testgitconfig");
    assert!(gitconfig.is_symlink(), "missing ~/.testgitconfig");
    let vimrc = home.path.join(".testvimrc");
    assert!(vimrc.is_symlink(), "missing ~/.testvimrc");
    // The executor must mkdir the parent chain (`~/.config/.../nvim/`
    // doesn't exist beforehand); the nested target pins that step.
    let nvim = home
        .path
        .join(".config")
        .join("keron-e2e")
        .join("nvim")
        .join("init.lua");
    assert!(
        nvim.is_symlink(),
        "executor should mkdir parent chain: {}",
        first.stdout,
    );

    let second = run(keron_apply(&fixture, &home.path), "yes\n");
    assert!(
        second.success,
        "second run failed: stdout=\n{}\nstderr=\n{}",
        second.stdout, second.stderr,
    );
    assert!(
        second.stdout.contains("No changes"),
        "second run should be idempotent, got: {}",
        second.stdout,
    );
}

#[test]
fn template_renders_then_is_idempotent() {
    let home = E2eHome::new("template");
    let fixture = fixture_dir("templates");

    let first = run(keron_apply(&fixture, &home.path), "yes\n");
    assert!(
        first.success,
        "first run failed: stdout=\n{}\nstderr=\n{}",
        first.stdout, first.stderr,
    );
    assert!(
        first.stdout.contains("1 added"),
        "first run should add 1 template: {}",
        first.stdout,
    );
    let rendered = home.path.join(".testgreeting");
    let content = fs::read_to_string(&rendered).expect("template file written");
    assert_eq!(
        content, "hello keron, this is a e2e fixture\n",
        "template should have placeholders substituted",
    );

    let second = run(keron_apply(&fixture, &home.path), "yes\n");
    assert!(
        second.success,
        "second run failed: stdout=\n{}\nstderr=\n{}",
        second.stdout, second.stderr,
    );
    assert!(
        second.stdout.contains("No changes"),
        "second run should be idempotent, got: {}",
        second.stdout,
    );
}

#[test]
fn package_resource_installs_then_no_ops_via_cache_seam() {
    // Real package managers can't run in CI; instead drive the
    // codepath via the `KERON_TEST_<MGR>_PACKAGES` cache seam and
    // `KERON_TEST_PACKAGE_BIN_<MGR>` install-binary seam exposed by
    // `keron-apply`.
    let home = E2eHome::new("pkg");
    let fixture = fixture_dir("packages");
    let noop = write_noop_binary(&home.path);

    let (manager_env, cache_env, bin_env) = pick_manager_env_keys();

    let mut first_cmd = keron_apply(&fixture, &home.path);
    first_cmd
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
        .env(manager_env.0, manager_env.1)
        .env(cache_env, "")
        .env(bin_env, &noop);
    let first = run(first_cmd, "yes\n");
    assert!(
        first.success,
        "first run failed: stdout=\n{}\nstderr=\n{}",
        first.stdout, first.stderr,
    );
    assert!(
        first.stdout.contains("1 added"),
        "first run should add 1 package: {}",
        first.stdout,
    );

    // Point the second-run install spy at a non-existent path: any
    // accidental install attempt surfaces as a spawn failure, pinning
    // that the NoOp classification truly skips the executor.
    let mut second_cmd = keron_apply(&fixture, &home.path);
    second_cmd
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
        .env(manager_env.0, manager_env.1)
        .env(cache_env, package_name_for_manager(manager_env.1))
        .env(bin_env, home.path.join("does-not-exist"));
    let second = run(second_cmd, "yes\n");
    assert!(
        second.success,
        "second run failed: stdout=\n{}\nstderr=\n{}",
        second.stdout, second.stderr,
    );
    assert!(
        second.stdout.contains("No changes"),
        "second run should be NoOp, got: {}",
        second.stdout,
    );
}

/// Pick a sensible default manager per host so the fixture's
/// `match manager { ... }` arm chooses something that actually
/// makes sense on that platform. Linux + macOS use `brew`, Windows
/// uses `winget`. The platform doesn't affect correctness — every
/// arm exercises the same codepath through `packages::install` —
/// but matching host conventions makes test failures easier to
/// diagnose.
const fn pick_manager_env_keys() -> (
    (&'static str, &'static str), // KERON_E2E_MANAGER → "brew"/"cargo"/"winget"
    &'static str,                 // KERON_TEST_<MGR>_PACKAGES
    &'static str,                 // KERON_TEST_PACKAGE_BIN_<MGR>
) {
    if cfg!(windows) {
        (
            ("KERON_E2E_MANAGER", "winget"),
            "KERON_TEST_WINGET_PACKAGES",
            "KERON_TEST_PACKAGE_BIN_WINGET",
        )
    } else {
        (
            ("KERON_E2E_MANAGER", "brew"),
            "KERON_TEST_BREW_PACKAGES",
            "KERON_TEST_PACKAGE_BIN_BREW",
        )
    }
}

/// Name keron's `packages` module would record in the cache, given
/// the manager keyword the manifest selected. Mirrors the fixture's
/// `match manager` arms.
fn package_name_for_manager(manager: &str) -> &'static str {
    match manager {
        "winget" => "BurntSushi.ripgrep",
        _ => "ripgrep",
    }
}

/// Write a no-op "install binary" — `/bin/true` semantics but as a
/// concrete file we control. On Unix this is a `#!/bin/sh\nexit 0`
/// script chmodded executable; on Windows a `.bat` that exits 0.
/// Used as the value of `KERON_TEST_PACKAGE_BIN_<MGR>` so the
/// executor "installs" without touching the system.
fn write_noop_binary(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        let p = dir.join("noop.bat");
        fs::write(&p, "@echo off\r\nexit /b 0\r\n").expect("write noop.bat");
        p
    } else {
        write_unix_noop(dir)
    }
}

#[cfg(unix)]
fn write_unix_noop(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("noop.sh");
    fs::write(&p, "#!/bin/sh\nexit 0\n").expect("write noop.sh");
    let mut perm = fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&p, perm).expect("chmod noop.sh");
    p
}

#[cfg(not(unix))]
fn write_unix_noop(_dir: &Path) -> PathBuf {
    unreachable!("write_unix_noop called on non-unix host")
}

#[cfg(unix)]
#[test]
fn elevated_symlink_into_protected_dir_is_owned_by_calling_user() {
    // Skipped on Windows: the runas-via-ShellExecuteExW path pops a
    // real UAC prompt that no test runner can answer.
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let home = E2eHome::new("elevated");
    let fixture_root = home.path.join("fixture");
    fs::create_dir_all(&fixture_root).unwrap();
    let target = fixture_root.join("payload");
    fs::write(&target, "x").unwrap();

    let protected = home.path.join("protected");
    fs::create_dir_all(&protected).unwrap();
    let mut perm = fs::metadata(&protected).unwrap().permissions();
    perm.set_mode(0o500);
    fs::set_permissions(&protected, perm).unwrap();
    let elevated_link = protected.join("link");

    let manifest = format!(
        "reconcile symlink(from = \"{}\", to = \"./payload\")\n",
        elevated_link.display(),
    );
    fs::write(fixture_root.join("entry.keron"), &manifest).unwrap();

    let spy = fixture_root.join("fake_elevator.sh");
    fs::write(
        &spy,
        "#!/bin/sh\n\
         if [ -n \"$KERON_TEST_UNLOCK_DIR\" ]; then\n\
           chmod 0700 \"$KERON_TEST_UNLOCK_DIR\"\n\
         fi\n\
         exec \"$@\"\n",
    )
    .unwrap();
    let mut perm = fs::metadata(&spy).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&spy, perm).unwrap();

    let my_uid = fs::metadata(&fixture_root).unwrap().uid();
    let my_group = fs::metadata(&fixture_root).unwrap().gid();

    let mut cmd = keron_apply(&fixture_root, &home.path);
    cmd.env("KERON_TEST_ELEVATOR", &spy)
        .env("KERON_TEST_UNLOCK_DIR", &protected);
    let out = run(cmd, "yes\n");

    // Restore so Drop can clean up cleanly regardless of what
    // happened above.
    let mut perm = fs::metadata(&protected).unwrap().permissions();
    perm.set_mode(0o700);
    fs::set_permissions(&protected, perm).unwrap();

    assert!(
        out.success,
        "elevated apply failed: stdout=\n{}\nstderr=\n{}",
        out.stdout, out.stderr,
    );
    assert!(elevated_link.is_symlink(), "missing elevated symlink");
    let meta = fs::symlink_metadata(&elevated_link).unwrap();
    assert_eq!(
        meta.uid(),
        my_uid,
        "elevated symlink must be owned by the calling user, not root",
    );
    assert_eq!(meta.gid(), my_group);
}
