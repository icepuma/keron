//! End-to-end test for package-manager wiring: a manifest with a
//! `brew(...)` resource gets classified against the cache, installed
//! via the executor, and on a second run becomes `NoOp` because the
//! cache now reports the package as installed.
//!
//! No real brew is invoked — `KERON_TEST_BREW_PACKAGES` pins the
//! cache state and `KERON_TEST_PACKAGE_BIN_BREW` swaps the install
//! binary for `/usr/bin/true` so the install "succeeds" without
//! touching the system. Same trick that the executor's unit tests
//! use; here we drive it through the full CLI binary.

#![cfg(unix)]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct TempProject {
    root: PathBuf,
}

impl TempProject {
    fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!("keron-pkg-it-{tag}-{}-{n}", std::process::id()));
        if root.exists() {
            let _ = fs::remove_dir_all(&root);
        }
        fs::create_dir_all(&root).unwrap();
        Self {
            root: fs::canonicalize(root).unwrap(),
        }
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let p = self.root.join(name);
        fs::write(&p, content).unwrap();
        p
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Path to the freshly-built `keron` binary. `CARGO_BIN_EXE_<name>`
/// is set by cargo at compile time and points at the binary produced
/// by the *same* package as this integration test, which is exactly
/// the artifact we want to drive.
fn keron_binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_keron"))
}

#[test]
fn package_installs_when_absent_and_noops_when_present() {
    let proj = TempProject::new("brew-flow");
    let entry = proj.write("entry.keron", "reconcile brew(\"ripgrep\")\n");
    let keron = keron_binary_path();

    // First run: cache reports nothing installed; ripgrep is
    // classified Create; install spy succeeds.
    let first = Command::new(&keron)
        .args(["apply", "--execute"])
        .arg(&entry)
        .env("KERON_TEST_BREW_PACKAGES", "")
        .env("KERON_TEST_PACKAGE_BIN_BREW", "/usr/bin/true")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(b"yes\n").unwrap();
            child.wait_with_output()
        })
        .expect("first keron run");
    let stdout1 = String::from_utf8_lossy(&first.stdout);
    let stderr1 = String::from_utf8_lossy(&first.stderr);
    println!("--- keron stdout (first run) ---\n{stdout1}");
    if !stderr1.is_empty() {
        println!("--- keron stderr (first run) ---\n{stderr1}");
    }
    assert!(
        first.status.success(),
        "first run failed: {:?}\nstdout:\n{stdout1}\nstderr:\n{stderr1}",
        first.status,
    );
    assert!(
        stdout1.contains("will be created"),
        "first run should show Create: {stdout1}",
    );
    assert!(
        stdout1.contains("1 added"),
        "first run should report 1 added: {stdout1}",
    );

    // Second run: cache says ripgrep is now installed; plan
    // classifies as NoOp; no install attempt.
    let second = Command::new(&keron)
        .args(["apply", "--execute"])
        .arg(&entry)
        .env("KERON_TEST_BREW_PACKAGES", "ripgrep")
        .env("KERON_TEST_PACKAGE_BIN_BREW", "/usr/bin/false")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(b"yes\n").unwrap();
            child.wait_with_output()
        })
        .expect("second keron run");
    let stdout2 = String::from_utf8_lossy(&second.stdout);
    let stderr2 = String::from_utf8_lossy(&second.stderr);
    println!("--- keron stdout (second run) ---\n{stdout2}");
    if !stderr2.is_empty() {
        println!("--- keron stderr (second run) ---\n{stderr2}");
    }
    assert!(
        second.status.success(),
        "second run failed: {:?}\nstdout:\n{stdout2}\nstderr:\n{stderr2}",
        second.status,
    );
    assert!(
        stdout2.contains("No changes"),
        "second run should be NoOp: {stdout2}",
    );
    // /usr/bin/false would have exited the install non-zero; that
    // this run still succeeds proves we never attempted to install.
}

#[test]
fn duplicate_packages_in_one_plan_install_once() {
    // Two `brew(...)` resources for the same package in the same
    // plan: the cache's "mark to install" semantics make the second
    // one NoOp, so the install spy is invoked once. We can't count
    // spy invocations directly without a marker file — instead we
    // assert the diff shows "1 to add", not "2 to add".
    let proj = TempProject::new("dupe");
    let entry = proj.write(
        "entry.keron",
        "reconcile {\n  brew(\"ripgrep\");\n  brew(\"ripgrep\");\n}\n",
    );
    let keron = keron_binary_path();
    let output = Command::new(&keron)
        .args(["apply"])
        .arg(&entry)
        .env("KERON_TEST_BREW_PACKAGES", "")
        .env("KERON_TEST_PACKAGE_BIN_BREW", "/usr/bin/true")
        .output()
        .expect("keron run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("--- keron stdout ---\n{stdout}");
    if !stderr.is_empty() {
        println!("--- keron stderr ---\n{stderr}");
    }
    assert!(
        output.status.success(),
        "dupe plan failed: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        stdout.contains("1 to add"),
        "duplicate package should count once: {stdout}",
    );
}
