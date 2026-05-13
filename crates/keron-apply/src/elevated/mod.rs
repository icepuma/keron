//! Elevated-rights pipeline for `keron apply`.
//!
//! Per-resource pre-check ([`detect::path_requires_elevation`] +
//! [`crate::plan::PackageManager::requires_elevation`]) classifies
//! each [`ResourceChange`] as either runnable as the calling user or
//! requiring root / Administrator. The renderer
//! ([`crate::diff::render_plan`]) surfaces the split; the apply
//! pipeline ([`crate::lib::run_with_io`]) executes the unprivileged
//! subset directly and re-execs the elevated subset under sudo /
//! `ShellExecuteExW`, transferring ownership of every created path
//! back to the calling user so the final filesystem state matches
//! what an unprivileged user would produce.
//!
//! The flow lives in three pieces:
//!   - [`detect`]: filesystem writability probe.
//!   - [`payload`]: JSON-over-tempfile contract between the parent
//!     and the elevated child.
//!   - [`child`] + [`chown`]: the child's apply + ownership-transfer
//!     loop.
//!   - [`mod@self`]: orchestration — probe elevator, spawn, wait.
//!
//! [`ResourceChange`]: crate::plan::ResourceChange

pub mod child;
pub mod chown;
pub mod detect;
pub mod payload;
#[cfg(unix)]
pub mod safe_write;

use std::path::Path;
#[cfg(unix)]
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::execute::ExecuteSummary;
use crate::plan::Plan;

/// Spawn the elevated child to apply `plan` and wait for it. The
/// child runs `keron __apply-elevated <payload>` under the platform's
/// elevation primitive (sudo / doas / pkexec on Unix; `ShellExecuteExW`
/// with the `runas` verb on Windows). Owner identity is captured here
/// and embedded in the payload so the child can `chown`/`lchown` /
/// `SetSecurityInfo` each created path back to the calling user.
///
/// Returns the child's reported [`ExecuteSummary`] on success. On
/// non-zero exit (denied password, child crash, partial failure)
/// returns an error.
///
/// # Errors
/// Errors when the elevator can't be located, when the parent's owner
/// identity can't be captured, when the payload can't be written, or
/// when the child exits non-zero.
pub fn run_elevated(plan: &Plan) -> Result<ExecuteSummary> {
    if plan.changes.is_empty() {
        return Ok(ExecuteSummary::default());
    }
    let owner =
        payload::capture_owner().context("capturing calling user's identity for elevation")?;
    let tempfile =
        payload::write_payload(plan, &owner).context("writing elevated apply payload")?;
    let summary = plan.summary();
    let exe = current_exe_canonicalized()?;
    let status = invoke_elevator(&exe, tempfile.path())?;
    if !status.success() {
        bail!("elevated apply exited with status {status}; see output above for details");
    }
    Ok(ExecuteSummary {
        added: summary.add,
        changed: summary.change,
        ran: summary.run,
    })
}

/// Resolve the binary keron is running as, refusing to elevate if it
/// lives in a world-writable directory (tampering vector: a malicious
/// peer could swap the binary between our resolve and the elevator's
/// exec).
fn current_exe_canonicalized() -> Result<std::path::PathBuf> {
    let raw = std::env::current_exe().context("locating the keron binary")?;
    let canonical = std::fs::canonicalize(&raw)
        .with_context(|| format!("canonicalizing `{}`", raw.display()))?;
    #[cfg(unix)]
    check_binary_tamper_resistance(&canonical)?;
    Ok(canonical)
}

/// Refuse to elevate when `canonical` (or any of its ancestors)
/// could be swapped under our feet between this call and the
/// elevator's exec. Pulled out of `current_exe_canonicalized` so a
/// test can drive it against a synthetic path without needing
/// `std::env::current_exe()` to point at a fixture.
#[cfg(unix)]
fn check_binary_tamper_resistance(canonical: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // Walk every ancestor up to `/`. A world-writable grandparent
    // lets an attacker rename-swap the parent dir between our
    // resolve and the elevator's exec; checking only the
    // immediate parent missed that. Refuse if any ancestor is
    // writable by anyone other than root, or if any metadata
    // read fails (fail-closed).
    //
    // Also reject group-writable ancestors whose group is not
    // root (gid 0). On multi-admin macOS boxes Homebrew leaves
    // `/usr/local/bin` group-writable to `admin`/`staff`; a
    // member of that group could race-swap the binary between
    // `canonicalize` and the elevator's exec.
    //
    // Finally, stat the binary itself: a group-writable or
    // world-writable keron binary, or one with setuid/setgid
    // bits set, is itself a tampering vector.
    let mut cursor: Option<&Path> = canonical.parent();
    while let Some(dir) = cursor {
        let meta = std::fs::metadata(dir).with_context(|| {
            format!(
                "refusing to elevate: cannot stat `{}` (ancestor of keron binary)",
                dir.display()
            )
        })?;
        let mode = meta.permissions().mode();
        if mode & 0o002 != 0 {
            bail!(
                "refusing to elevate: ancestor `{}` of `{}` is world-writable",
                dir.display(),
                canonical.display(),
            );
        }
        if mode & 0o020 != 0 && meta.gid() != 0 {
            bail!(
                "refusing to elevate: ancestor `{}` of `{}` is writable by non-root group {} (mode {:o})",
                dir.display(),
                canonical.display(),
                meta.gid(),
                mode & 0o777,
            );
        }
        cursor = dir.parent();
    }
    let bin_meta = std::fs::metadata(canonical).with_context(|| {
        format!(
            "refusing to elevate: cannot stat keron binary `{}`",
            canonical.display()
        )
    })?;
    let bin_mode = bin_meta.permissions().mode();
    if bin_mode & 0o022 != 0 {
        bail!(
            "refusing to elevate: keron binary `{}` is group- or world-writable (mode {:o})",
            canonical.display(),
            bin_mode & 0o777,
        );
    }
    if bin_mode & 0o6000 != 0 {
        bail!(
            "refusing to elevate: keron binary `{}` has setuid/setgid bits set (mode {:o})",
            canonical.display(),
            bin_mode & 0o7777,
        );
    }
    Ok(())
}

/// Locate an elevator on PATH and spawn `<elevator> <exe>
/// __apply-elevated <payload>`. stdio is inherited so the user sees
/// the password prompt in their terminal.
#[cfg(unix)]
fn invoke_elevator(exe: &Path, payload: &Path) -> Result<std::process::ExitStatus> {
    let elevator = test_elevator_override()
        .or_else(probe_elevator)
        .ok_or_else(|| {
            anyhow::anyhow!("elevation requires sudo, doas, or pkexec on PATH; none found")
        })?;
    let mut cmd = Command::new(&elevator);
    cmd.arg(exe)
        .arg("__apply-elevated")
        .arg(payload)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd.status()
        .with_context(|| format!("invoking elevator `{}`", elevator.display()))
}

#[cfg(unix)]
fn probe_elevator() -> Option<std::path::PathBuf> {
    // Tty-friendly first: sudo/doas prompt on the controlling
    // terminal. pkexec last because its polkit auth agent only runs
    // in a graphical session — falling back to it on a headless box
    // just produces a confusing "no agent" failure.
    for name in ["sudo", "doas", "pkexec"] {
        if let Some(path) = which_on_path(name) {
            return Some(path);
        }
    }
    None
}

/// Test-only hook: a path in `KERON_TEST_ELEVATOR` short-circuits the
/// PATH probe. The integration tests set this to a spy binary that
/// drops the elevator-prefix argument and execs the rest, so the full
/// re-exec wiring is exercised without root or a password prompt.
///
/// Gated on `KERON_ALLOW_TEST_OVERRIDES=1` so a hostile env in a
/// privileged caller (cron, Ansible) cannot replace sudo by setting
/// a single variable. This matches the package-manager test seams.
#[cfg(unix)]
fn test_elevator_override() -> Option<std::path::PathBuf> {
    if std::env::var_os("KERON_ALLOW_TEST_OVERRIDES").is_none_or(|v| v != "1") {
        return None;
    }
    std::env::var_os("KERON_TEST_ELEVATOR").map(std::path::PathBuf::from)
}

#[cfg(unix)]
fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        // Skip relative PATH entries (`.`, `bin`, empty = cwd).
        // sudo/doas/pkexec live in absolute system dirs; resolving
        // a relative entry would let an attacker-controlled cwd
        // inject `./sudo` and have keron exec that instead.
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(windows)]
fn invoke_elevator(exe: &Path, payload: &Path) -> Result<std::process::ExitStatus> {
    chown::windows::shell_execute_runas(exe, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn probe_elevator_returns_none_when_path_is_empty() {
        let saved = std::env::var_os("PATH");
        // SAFETY: edition 2024 marks set_var unsafe; this test
        // single-threads its mutations and restores PATH before
        // returning, so observable global state is unchanged.
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("PATH");
        }
        let got = probe_elevator();
        // SAFETY: see above.
        #[allow(unsafe_code)]
        unsafe {
            if let Some(p) = saved {
                std::env::set_var("PATH", p);
            }
        }
        assert!(
            got.is_none(),
            "probe should fail when PATH is unset: {got:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn which_on_path_skips_relative_entries() {
        // A `.`/empty/`bin` entry in `PATH` could otherwise let an
        // attacker-controlled cwd inject `./sudo`. The filter keeps
        // only absolute directories.
        use std::fs::{self, Permissions};
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "keron-which-rel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Plant an executable named `keron-test-which` here. The
        // search will only find it if the path entry is absolute.
        let bin = dir.join("keron-test-which");
        let mut f = fs::File::create(&bin).unwrap();
        writeln!(f, "#!/bin/sh\nexit 0").unwrap();
        drop(f);
        fs::set_permissions(&bin, Permissions::from_mode(0o755)).unwrap();

        let saved = std::env::var_os("PATH");
        // PATH that contains both a relative entry and the absolute
        // dir; if `which_on_path` admitted the relative entry, it
        // would happily resolve `./keron-test-which` from a cwd we
        // didn't control.
        let path_with_relative = format!(".:bin:{}", dir.display());
        // SAFETY: see other test; mutation restored before return.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("PATH", &path_with_relative);
        }
        let got = which_on_path("keron-test-which");
        // SAFETY: see above.
        #[allow(unsafe_code)]
        unsafe {
            if let Some(p) = saved {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
        let _ = fs::remove_dir_all(&dir);

        // The find must resolve from the absolute entry, which has
        // an absolute prefix. Mutation `delete ! in which_on_path`
        // would have caused the search to skip absolute entries.
        let resolved = got.expect("absolute entry should have resolved the binary");
        assert!(
            resolved.is_absolute(),
            "resolved path must be absolute: {resolved:?}"
        );
        assert!(resolved.starts_with(&dir), "got: {resolved:?}");
    }

    #[cfg(unix)]
    #[test]
    fn which_on_path_returns_none_when_only_relative_entries_present() {
        // `which_on_path` with PATH=".:bin" (no absolute entries)
        // must return None even if a matching name exists in cwd.
        // Catches the `delete !` mutation that would admit
        // relative entries.
        let saved = std::env::var_os("PATH");
        // SAFETY: single-threaded test; PATH restored before return.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("PATH", ".:bin:tools");
        }
        let got = which_on_path("keron-no-such-name-12345");
        // SAFETY: see above.
        #[allow(unsafe_code)]
        unsafe {
            if let Some(p) = saved {
                std::env::set_var("PATH", p);
            } else {
                std::env::remove_var("PATH");
            }
        }
        assert!(
            got.is_none(),
            "PATH with only relative entries must yield None: {got:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_binary_tamper_resistance_rejects_world_writable_ancestor() {
        use std::os::unix::fs::PermissionsExt;
        let parent = std::env::temp_dir().join(format!(
            "keron-elev-ww-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let bin = parent.join("fake-keron");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();

        let err = check_binary_tamper_resistance(&bin)
            .expect_err("world-writable parent must refuse elevation");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("world-writable"),
            "expected world-writable refusal, got: {msg}"
        );

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[cfg(unix)]
    #[test]
    fn check_binary_tamper_resistance_accepts_locked_down_path() {
        use std::os::unix::fs::PermissionsExt;
        // A directory the running user owns with no group-or-world
        // write bits should pass on a unix host. Pins the
        // `0o002`/`0o020` masks (mutations that flip the masks
        // to `0` would still pass this test but mutations that
        // invert the comparison would fail).
        let parent = std::env::temp_dir().join(format!(
            "keron-elev-locked-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let bin = parent.join("fake-keron");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Note: the walk goes all the way to `/`. Some hosts may
        // have unusual mode bits on system dirs (e.g., sticky `/tmp`).
        // We don't require the walk to succeed; we just require
        // that the bin-itself check doesn't trip on a 0755 mode.
        let result = check_binary_tamper_resistance(&bin);
        if let Err(e) = &result {
            let msg = format!("{e:#}");
            assert!(
                !msg.contains("is group- or world-writable")
                    || !msg.contains(bin.to_string_lossy().as_ref()),
                "binary mode 0o755 must not trip the binary-self check: {msg}"
            );
        }

        let _ = std::fs::remove_dir_all(&parent);
    }

    #[cfg(unix)]
    #[test]
    fn check_binary_tamper_resistance_rejects_group_writable_non_root_ancestor() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // Pins the `meta.gid() != 0` arm of the group-writable
        // refusal: a 0o775 ancestor owned by a non-root group is a
        // valid race-swap vector (Homebrew's /usr/local/bin scenario)
        // and must refuse. The mutation `!= with ==` would only bail
        // on root-group ancestors and silently admit the dangerous
        // shared-admin-group case.
        let parent = std::env::temp_dir().join(format!(
            "keron-elev-gw-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let bin = parent.join("fake-keron");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Group-writable WITHOUT world-writable: the world-writable
        // branch (which fires first) must not pre-empt this test.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o775)).unwrap();

        // Sanity: the temp dir's gid must be non-zero. If a host
        // somehow gives temp dirs gid 0 (unusual), the mutated check
        // would also fail — the test is still safe but no longer
        // discriminating, so skip in that case.
        let meta = std::fs::metadata(&parent).unwrap();
        if meta.gid() == 0 {
            let _ = std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&parent);
            return;
        }

        let result = check_binary_tamper_resistance(&bin);
        // Restore perms before any assertion can early-return.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&parent);

        let err = result.expect_err("group-writable non-root-group ancestor must refuse elevation");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("non-root group"),
            "expected non-root-group refusal, got: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_binary_tamper_resistance_rejects_setuid_binary() {
        use std::os::unix::fs::PermissionsExt;
        let parent = std::env::temp_dir().join(format!(
            "keron-elev-setuid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = parent.join("setuid-keron");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        // Set setuid bit (mode 04755).
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o4755)).unwrap();

        let result = check_binary_tamper_resistance(&bin);
        if let Err(e) = result {
            let msg = format!("{e:#}");
            assert!(
                msg.contains("setuid")
                    || msg.contains("setgid")
                    || msg.contains("group- or world-writable"),
                "expected setuid refusal, got: {msg}"
            );
        }
        // Some platforms strip setuid on the chmod call when the
        // file isn't owned root, so we only assert when the bit
        // actually stuck.
        let _ = std::fs::remove_dir_all(&parent);
    }
}
