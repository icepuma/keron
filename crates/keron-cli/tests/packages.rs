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

    let first = Command::new(&keron)
        .args(["apply", "--execute"])
        .arg(&entry)
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
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

    // Second run uses `/usr/bin/false` as the install spy: if the
    // planner ever attempts to install, the executor exits non-zero
    // and the test fails — proving the NoOp classification holds.
    let second = Command::new(&keron)
        .args(["apply", "--execute"])
        .arg(&entry)
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
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
}

#[test]
fn tap_qualified_brew_synthesizes_tap_resource_in_plan() {
    // A `brew("user/tap/formula")` call should expand into a `Tap`
    // change plus the package Create, both visible in the plan diff.
    let proj = TempProject::new("tap-inline");
    let entry = proj.write("entry.keron", "reconcile brew(\"icepuma/keron/keron\")\n");
    let keron = keron_binary_path();
    let output = Command::new(&keron)
        .args(["apply"])
        .arg(&entry)
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
        .env("KERON_TEST_BREW_PACKAGES", "")
        .env("KERON_TEST_BREW_TAPS", "")
        .env("KERON_TEST_PACKAGE_BIN_BREW", "/usr/bin/true")
        .output()
        .expect("keron run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "tap-inline plan failed: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        stdout.contains("tap:icepuma/keron"),
        "plan should mention the synthesized tap resource: {stdout}",
    );
    assert!(
        stdout.contains("brew:icepuma/keron/keron"),
        "plan should mention the qualified brew formula: {stdout}",
    );
    assert!(
        stdout.contains("2 to add"),
        "plan should count both tap and package: {stdout}",
    );
}

#[test]
fn tap_already_installed_classifies_as_noop() {
    // When the tap is already present and no custom URL is asserted,
    // the tap change collapses to NoOp.
    let proj = TempProject::new("tap-noop");
    let entry = proj.write("entry.keron", "reconcile brew(\"icepuma/keron/keron\")\n");
    let keron = keron_binary_path();
    let output = Command::new(&keron)
        .args(["apply"])
        .arg(&entry)
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
        .env("KERON_TEST_BREW_PACKAGES", "")
        .env("KERON_TEST_BREW_TAPS", "icepuma/keron")
        .env("KERON_TEST_PACKAGE_BIN_BREW", "/usr/bin/true")
        .output()
        .expect("keron run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    // 1 added (the package); the tap is NoOp and is counted in
    // `unchanged`, not `to add`.
    assert!(
        stdout.contains("1 to add"),
        "only the package should be added: {stdout}",
    );
    assert!(
        stdout.contains("1 unchanged"),
        "the tap should be reported as unchanged: {stdout}",
    );
}

#[test]
fn tap_url_drift_classifies_as_update() {
    // When the tap is present but its remote URL differs from the
    // manifest, the tap change is Update.
    let proj = TempProject::new("tap-drift");
    let entry = proj.write(
        "entry.keron",
        "reconcile brew(\"icepuma/keron/keron\", \"https://github.com/icepuma/keron\")\n",
    );
    let keron = keron_binary_path();
    let output = Command::new(&keron)
        .args(["apply"])
        .arg(&entry)
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
        .env("KERON_TEST_BREW_PACKAGES", "")
        .env("KERON_TEST_BREW_TAPS", "icepuma/keron")
        .env(
            "KERON_TEST_BREW_TAP_REMOTES",
            "icepuma/keron=https://old.example/keron",
        )
        .env("KERON_TEST_PACKAGE_BIN_BREW", "/usr/bin/true")
        .output()
        .expect("keron run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("1 to change"),
        "tap URL drift should produce a change action: {stdout}",
    );
}

#[test]
fn installed_brew_formula_classifies_as_noop_even_when_locally_outdated() {
    // `keron apply` only ensures presence — upgrading installed
    // packages is the user's job via the underlying manager. An
    // installed formula must therefore classify as NoOp regardless
    // of whether the local brew copy considers it outdated.
    let proj = TempProject::new("installed-noop");
    let entry = proj.write("entry.keron", "reconcile brew(\"ripgrep\")\n");
    let keron = keron_binary_path();
    let output = Command::new(&keron)
        .args(["apply"])
        .arg(&entry)
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
        .env("KERON_TEST_BREW_PACKAGES", "ripgrep")
        .env("KERON_TEST_PACKAGE_BIN_BREW", "/usr/bin/true")
        .output()
        .expect("keron run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        !stdout.contains("to add") || stdout.contains("0 to add"),
        "installed brew formula must not be reported as an add: {stdout}",
    );
    assert!(
        !stdout.contains("to change") || stdout.contains("0 to change"),
        "installed brew formula must not be reported as a change: {stdout}",
    );
}

#[test]
fn cask_resource_routes_through_cask_namespace() {
    // A `cask("alacritty")` call is a distinct resource from
    // `brew("alacritty")`; the address renders as `cask:` and the
    // installed-set lookup uses the cask namespace.
    let proj = TempProject::new("cask");
    let entry = proj.write("entry.keron", "reconcile cask(\"font-jetbrains-mono\")\n");
    let keron = keron_binary_path();
    let output = Command::new(&keron)
        .args(["apply"])
        .arg(&entry)
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
        .env("KERON_TEST_BREW_CASK_PACKAGES", "")
        .env("KERON_TEST_PACKAGE_BIN_BREW", "/usr/bin/true")
        .output()
        .expect("keron run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "cask plan failed: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        stdout.contains("cask:font-jetbrains-mono"),
        "plan should show the cask address: {stdout}",
    );
}

#[test]
fn duplicate_packages_in_one_plan_install_once() {
    // Asserts via the diff line ("1 to add", not "2 to add") rather
    // than counting spy invocations — there's no marker file in this
    // wiring, so the diff is the cheapest observable.
    let proj = TempProject::new("dupe");
    let entry = proj.write(
        "entry.keron",
        "reconcile {\n  brew(\"ripgrep\");\n  brew(\"ripgrep\");\n}\n",
    );
    let keron = keron_binary_path();
    let output = Command::new(&keron)
        .args(["apply"])
        .arg(&entry)
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
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
