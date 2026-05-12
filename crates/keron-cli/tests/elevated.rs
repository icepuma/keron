//! Integration tests for the elevated-rights flow.
//!
//! Real `sudo` would prompt for a password and consult sudoers,
//! which can't run in CI. Instead, the tests write a shell-script
//! spy that mimics `sudo`'s argv shape — `<spy> <exe>
//! __apply-elevated <payload>` — and `exec`s `argv[1..]`. The
//! library exposes `KERON_TEST_ELEVATOR` (in `elevated::mod.rs`)
//! that short-circuits the real PATH probe, so the spy script gets
//! used in place of `sudo` / `doas` / `pkexec`. The chown-back is
//! exercised by aiming at the calling user's own uid/gid — the
//! syscall executes but the value doesn't change, which is enough to
//! pin the wiring without requiring root.

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
        let root = env::temp_dir().join(format!(
            "keron-elevated-it-{tag}-{}-{n}",
            std::process::id()
        ));
        if root.exists() {
            let _ = fs::remove_dir_all(&root);
        }
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        Self { root }
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

/// Compose a /bin/sh script that stands in for `sudo`. Drops its
/// own program name and `exec`s `"$@"` — same shape as `sudo
/// <command...>` running `<command...>` after a successful prompt.
///
/// Optionally chmods a directory passed via `KERON_TEST_UNLOCK_DIR`
/// before exec — the test arranges a directory that's 0500 (so the
/// plan-time writability probe classifies it as elevated) but the
/// fake elevator "grants" write access just before the elevated
/// child runs, mimicking what real root would have done implicitly.
fn write_spy_elevator(proj: &TempProject) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = proj.root.join("fake_elevator.sh");
    fs::write(
        &p,
        "#!/bin/sh\n\
         if [ -n \"$KERON_TEST_UNLOCK_DIR\" ]; then\n\
           chmod 0700 \"$KERON_TEST_UNLOCK_DIR\"\n\
         fi\n\
         exec \"$@\"\n",
    )
    .unwrap();
    let mut perm = fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&p, perm).unwrap();
    p
}

/// Path to the freshly-built `keron` binary. `CARGO_BIN_EXE_<name>`
/// is set by cargo at compile time and points at the binary produced
/// by the *same* package as this integration test, which is exactly
/// the artifact we want to drive.
fn keron_binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_keron"))
}

#[test]
fn elevated_subset_runs_under_spy_and_chowns_back() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let proj = TempProject::new("e2e");

    let plain_target = proj.write("plain-target", "x");
    let plain_link = proj.root.join("plain-link");

    // Protected directory: 0500 means write-denied to the owner
    // (us) too, so the writability probe classifies as elevated.
    let protected_dir = proj.root.join("protected");
    fs::create_dir_all(&protected_dir).unwrap();
    let protected_target = proj.write("elevated-target", "x");
    let protected_link = protected_dir.join("elevated-link");
    let mut perm = fs::metadata(&protected_dir).unwrap().permissions();
    perm.set_mode(0o500);
    fs::set_permissions(&protected_dir, perm).unwrap();

    let manifest = format!(
        "reconcile {{\n  symlink(from = \"{}\", to = \"{}\");\n  symlink(from = \"{}\", to = \"{}\");\n}}\n",
        plain_link.display(),
        plain_target.display(),
        protected_link.display(),
        protected_target.display(),
    );
    let entry = proj.write("entry.keron", &manifest);

    let spy = write_spy_elevator(&proj);
    let keron = keron_binary_path();

    let my_uid = fs::metadata(&proj.root).unwrap().uid();
    let my_group = fs::metadata(&proj.root).unwrap().gid();

    // SUDO_UID/SUDO_GID must be unset: if the surrounding shell ran
    // under sudo, leftovers would trip the direct-elevation refusal.
    let output = Command::new(&keron)
        .args(["apply", "--execute"])
        .arg(&entry)
        .env("KERON_ALLOW_TEST_OVERRIDES", "1")
        .env("KERON_TEST_ELEVATOR", &spy)
        .env("KERON_TEST_UNLOCK_DIR", &protected_dir)
        .env_remove("SUDO_UID")
        .env_remove("SUDO_GID")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(b"yes\n").unwrap();
            child.wait_with_output()
        })
        .expect("running keron");

    // Restore directory permissions so Drop can clean up regardless
    // of what asserts below do.
    let mut perm = fs::metadata(&protected_dir).unwrap().permissions();
    perm.set_mode(0o700);
    fs::set_permissions(&protected_dir, perm).unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("--- keron stdout ---\n{stdout}");
    if !stderr.is_empty() {
        println!("--- keron stderr ---\n{stderr}");
    }
    assert!(
        output.status.success(),
        "keron exited non-zero: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );

    assert!(
        plain_link.is_symlink(),
        "plain symlink should be created: stdout=\n{stdout}",
    );

    assert!(
        protected_link.is_symlink(),
        "elevated symlink should be created: stdout=\n{stdout}",
    );
    let elev_meta = fs::symlink_metadata(&protected_link).unwrap();
    assert_eq!(
        elev_meta.uid(),
        my_uid,
        "elevated symlink should be owned by the calling user, not root"
    );
    assert_eq!(elev_meta.gid(), my_group);

    assert!(
        stdout.contains("Apply complete"),
        "missing unprivileged summary: {stdout}",
    );
    assert!(
        stdout.contains("elevated apply complete"),
        "missing elevated child output: {stdout}",
    );
}
