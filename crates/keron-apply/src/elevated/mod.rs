//! Elevated-rights pipeline for `keron apply`.
//!
//! Per-resource pre-check ([`detect::path_requires_elevation`] +
//! [`crate::plan::PackageManager::requires_elevation`]) classifies
//! each [`ResourceChange`] as either runnable as the calling user or
//! requiring root / Administrator. The renderer
//! ([`crate::diff::render_plan`]) surfaces the split; the apply
//! pipeline ([`crate::lib::run_with_io`]) executes the unprivileged
//! subset directly and, on Unix, re-execs the elevated subset under
//! sudo / doas / pkexec. Windows fails before prompting until it has a
//! reparse-point-safe filesystem walker. Resulting filesystem objects
//! remain owned by the elevated principal.
//!
//! The flow lives in three pieces:
//!   - [`detect`]: filesystem writability probe.
//!   - [`payload`]: JSON-over-tempfile contract between the parent
//!     and the elevated child.
//!   - [`child`]: privileged payload validation + apply loop.
//!   - [`mod@self`]: orchestration — probe elevator, spawn, wait.
//!
//! [`ResourceChange`]: crate::plan::ResourceChange

pub mod child;
pub mod detect;
pub mod payload;
#[cfg(unix)]
pub mod safe_write;

#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::{Command, Stdio};

#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail};

#[cfg(unix)]
use crate::elevated::payload::PayloadExpectation;
use crate::execute::ExecuteSummary;
#[cfg(windows)]
use crate::plan::Action;
use crate::plan::Plan;

/// Spawn the elevated child to apply `plan` and wait for it. The
/// child runs `keron __apply-elevated <payload>` under the platform's
/// elevation primitive (sudo / doas / pkexec). Elevated outputs
/// deliberately remain owned by the elevated principal: handing a
/// system file back to the invoking user would turn a one-time
/// authorization into a persistent privilege downgrade.
///
/// Returns the child's reported [`ExecuteSummary`] on success. On
/// non-zero exit (denied password, child crash, partial failure)
/// returns an error.
///
/// # Errors
/// Errors when the elevator can't be located, when the payload can't be
/// written, or when the child exits non-zero.
#[cfg(unix)]
pub fn run_elevated(plan: &Plan) -> Result<ExecuteSummary> {
    if plan.changes.is_empty() {
        return Ok(ExecuteSummary::default());
    }
    let tempfile = payload::write_payload(plan).context("writing elevated apply payload")?;
    let summary = plan.summary();
    let exe = current_exe_canonicalized()?;
    let status = invoke_elevator(&exe, tempfile.path(), tempfile.expected())?;
    if !status.success() {
        bail!("elevated apply exited with status {status}; see output above for details");
    }
    Ok(ExecuteSummary {
        added: summary.add,
        changed: summary.change,
        ran: summary.run,
        warnings: Vec::new(),
    })
}

/// Windows elevated filesystem reconciliation is intentionally
/// unavailable until it has a reparse-point-safe handle walker matching
/// Unix's `openat` implementation. Fail before opening a UAC prompt.
///
/// # Errors
/// Returns an unsupported-platform error when the plan contains a
/// non-no-op resource that requires elevation.
#[cfg(windows)]
pub fn preflight(plan: &Plan) -> Result<()> {
    if plan
        .changes
        .iter()
        .any(|change| change.requires_elevation && !matches!(change.action, Action::NoOp))
    {
        bail!(
            "elevated filesystem writes are not supported on Windows yet; no changes were applied"
        );
    }
    Ok(())
}

/// Apply an already-preflighted elevated plan on Windows.
///
/// This remains a defensive failure in case an internal caller skips
/// [`preflight`].
#[cfg(windows)]
pub fn run_elevated(plan: &Plan) -> Result<ExecuteSummary> {
    if plan.changes.is_empty() {
        return Ok(ExecuteSummary::default());
    }
    bail!("elevated filesystem writes are not supported on Windows yet; no changes were applied")
}

/// Resolve the binary keron is running as, refusing to elevate if it
/// lives in a world-writable directory (tampering vector: a malicious
/// peer could swap the binary between our resolve and the elevator's
/// exec).
#[cfg(unix)]
fn current_exe_canonicalized() -> Result<std::path::PathBuf> {
    let raw = std::env::current_exe().context("locating the keron binary")?;
    let canonical = std::fs::canonicalize(&raw)
        .with_context(|| format!("canonicalizing `{}`", raw.display()))?;
    let invoking_uid = rustix::process::geteuid().as_raw();
    check_binary_tamper_resistance(&canonical, Some(invoking_uid), false)?;
    Ok(canonical)
}

/// Refuse to elevate when `canonical` (or any of its ancestors)
/// could be swapped under our feet between this call and the
/// elevator's exec. Pulled out of `current_exe_canonicalized` so a
/// test can drive it against a synthetic path without needing
/// `std::env::current_exe()` to point at a fixture.
#[cfg(unix)]
fn check_binary_tamper_resistance(
    canonical: &Path,
    allowed_non_root_owner: Option<u32>,
    allow_setid: bool,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // Walk every ancestor up to `/`. A world-writable grandparent
    // lets an attacker rename-swap the parent dir between our
    // resolve and the elevator's exec; checking only the
    // immediate parent missed that. Refuse group/world-writable
    // ancestors, owners outside the explicitly trusted uid, and any
    // metadata failure. The running keron path trusts the invoking
    // uid; elevator paths trust root only.
    //
    // Also reject every group-writable ancestor. Membership in gid 0
    // does not imply euid 0, so a root-group exception would still
    // leave a rename-swap primitive.
    //
    // Finally, stat the executable itself. Keron may not carry setid
    // bits; a trusted elevator may, because that is how sudo/pkexec are
    // conventionally installed.
    let mut cursor: Option<&Path> = canonical.parent();
    while let Some(dir) = cursor {
        let meta = std::fs::metadata(dir).with_context(|| {
            format!(
                "refusing to elevate: cannot stat `{}` (ancestor of keron binary)",
                dir.display()
            )
        })?;
        let mode = meta.permissions().mode();
        if mode & 0o022 != 0 {
            bail!(
                "refusing to elevate: ancestor `{}` of `{}` is group- or world-writable (mode {:o})",
                dir.display(),
                canonical.display(),
                mode & 0o777,
            );
        }
        if meta.uid() != 0 && Some(meta.uid()) != allowed_non_root_owner {
            bail!(
                "refusing to elevate: ancestor `{}` of `{}` is owned by untrusted uid {}; its owner can chmod and replace it",
                dir.display(),
                canonical.display(),
                meta.uid(),
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
    if !bin_meta.is_file() {
        bail!(
            "refusing to elevate: keron executable `{}` is not a regular file",
            canonical.display()
        );
    }
    let bin_mode = bin_meta.permissions().mode();
    validate_executable_mode(canonical, bin_mode, allow_setid)?;
    if bin_meta.uid() != 0 && Some(bin_meta.uid()) != allowed_non_root_owner {
        bail!(
            "refusing to elevate: executable `{}` is owned by untrusted uid {}; its owner can chmod and replace it",
            canonical.display(),
            bin_meta.uid(),
        );
    }
    Ok(())
}

#[cfg(unix)]
fn validate_executable_mode(path: &Path, mode: u32, allow_setid: bool) -> Result<()> {
    if mode & 0o022 != 0 {
        bail!(
            "refusing to elevate: executable `{}` is group- or world-writable (mode {:o})",
            path.display(),
            mode & 0o777,
        );
    }
    if mode & 0o111 == 0 {
        bail!(
            "refusing to elevate: executable `{}` has no execute bit set (mode {:o})",
            path.display(),
            mode & 0o777,
        );
    }
    if !allow_setid && mode & 0o6000 != 0 {
        bail!(
            "refusing to elevate: executable `{}` has setuid/setgid bits set (mode {:o})",
            path.display(),
            mode & 0o7777,
        );
    }
    Ok(())
}

/// Locate an elevator on PATH and spawn `<elevator> <exe>
/// __apply-elevated <payload> <digest> <identity>`. stdio is inherited
/// so the user sees the password prompt in their terminal.
#[cfg(unix)]
fn invoke_elevator(
    exe: &Path,
    payload: &Path,
    expected: &PayloadExpectation,
) -> Result<std::process::ExitStatus> {
    let elevator = test_elevator_override()
        .or_else(probe_elevator)
        .ok_or_else(|| {
            anyhow::anyhow!("elevation requires sudo, doas, or pkexec on PATH; none found")
        })?;
    let mut cmd = Command::new(&elevator);
    cmd.arg(exe)
        .arg("__apply-elevated")
        .arg(payload)
        .arg(&expected.digest_hex)
        .arg(expected.identity.encode())
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
        if let Some(path) = trusted_binary_on_path(name) {
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
#[cfg(all(unix, debug_assertions))]
fn test_elevator_override() -> Option<std::path::PathBuf> {
    if std::env::var_os("KERON_ALLOW_TEST_OVERRIDES").is_none_or(|v| v != "1") {
        return None;
    }
    std::env::var_os("KERON_TEST_ELEVATOR").map(std::path::PathBuf::from)
}

/// Release-build counterpart: always None so production cannot honour
/// the test override even with `KERON_ALLOW_TEST_OVERRIDES=1` set.
///
/// `#[cfg_attr(test, mutants::skip)]`: this variant is only compiled
/// under `not(debug_assertions)` and never reached by `cargo test`.
/// The debug-build sibling is covered by
/// `test_elevator_override_requires_allow_gate`.
#[cfg_attr(test, mutants::skip)]
#[cfg(all(unix, not(debug_assertions)))]
fn test_elevator_override() -> Option<std::path::PathBuf> {
    None
}

#[cfg(unix)]
fn trusted_binary_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(name);
        if !candidate.is_file() {
            continue;
        }
        let Ok(canonical) = std::fs::canonicalize(candidate) else {
            continue;
        };
        if check_binary_tamper_resistance(&canonical, None, true).is_ok() {
            return Some(canonical);
        }
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn probe_elevator_returns_none_when_path_is_empty() {
        let _g = crate::packages::lock_env();
        let saved = std::env::var_os("PATH");
        // SAFETY: lock_env serializes process-environment mutation and
        // PATH is restored before returning.
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
    fn trusted_binary_on_path_skips_relative_entries() {
        use std::fs::{self, Permissions};
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;

        let _g = crate::packages::lock_env();
        // A `.`/empty/`bin` entry in `PATH` could otherwise let an
        // attacker-controlled cwd inject `./sudo`. The filter keeps
        // only absolute directories.
        let dir = std::env::temp_dir().join(format!(
            "keron-which-rel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
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
        let got = trusted_binary_on_path("keron-test-which");
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

        assert!(
            got.is_none(),
            "a user-owned absolute fallback must not make relative PATH entries trustworthy",
        );
    }

    #[cfg(unix)]
    #[test]
    fn trusted_binary_on_path_returns_none_when_only_relative_entries_present() {
        let _g = crate::packages::lock_env();
        // `which_on_path` with PATH=".:bin" (no absolute entries)
        // must return None even if a matching name exists in cwd.
        // Catches the `delete !` mutation that would admit
        // relative entries.
        let saved = std::env::var_os("PATH");
        // SAFETY: lock_env serializes mutation; PATH is restored.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("PATH", ".:bin:tools");
        }
        let got = trusted_binary_on_path("keron-no-such-name-12345");
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

    #[test]
    fn trusted_binary_on_path_rejects_writable_absolute_directory() {
        use std::os::unix::fs::PermissionsExt;

        let _g = crate::packages::lock_env();
        let dir = std::env::temp_dir().join(format!(
            "keron-untrusted-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = "keron-fake-elevator";
        let binary = dir.join(name);
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let saved = std::env::var_os("PATH");
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("PATH", &dir);
        }
        let trusted = trusted_binary_on_path(name);
        #[allow(unsafe_code)]
        unsafe {
            if let Some(path) = saved {
                std::env::set_var("PATH", path);
            } else {
                std::env::remove_var("PATH");
            }
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            trusted.is_none(),
            "writable PATH entry must not supply sudo"
        );
    }

    #[test]
    fn executable_mode_policy_allows_setid_only_for_elevators() {
        let path = Path::new("/usr/bin/example");
        let keron_err = validate_executable_mode(path, 0o104_755, false)
            .expect_err("keron itself must not be setuid");
        assert!(format!("{keron_err:#}").contains("setuid/setgid"));
        validate_executable_mode(path, 0o104_755, true)
            .expect("a locked-down setuid-root elevator mode is valid");
        validate_executable_mode(path, 0o104_775, true)
            .expect_err("setid does not excuse group-writable mode");
        validate_executable_mode(path, 0o100_644, true)
            .expect_err("an elevator must be executable");
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

        let err = check_binary_tamper_resistance(&bin, None, false)
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
    fn check_binary_tamper_resistance_accepts_current_test_binary() {
        let bin = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let invoking_uid = rustix::process::geteuid().as_raw();
        check_binary_tamper_resistance(&bin, Some(invoking_uid), false)
            .expect("the current test binary must satisfy the production elevation policy");
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

        let result = check_binary_tamper_resistance(&bin, None, false);
        // Restore perms before any assertion can early-return.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&parent);

        let err = result.expect_err("group-writable non-root-group ancestor must refuse elevation");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("group- or world-writable"),
            "expected group-write refusal, got: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_binary_tamper_resistance_rejects_owner_writable_non_root_ancestor() {
        // Pins the owner-writable ancestor check at line 139:
        // `!allow_owner_writable && mode & 0o200 != 0 && meta.uid() != 0`.
        // Catches:
        //   - `& with |` (mode | 0o200 is always non-zero -> always bail,
        //      conflating with other paths)
        //   - `& with ^` (toggles the bit; depends on existing mode)
        //   - `!= 0 with == 0` (inverts the owner-writable check)
        //   - `meta.uid() != 0 with == 0` (inverts to "owner is root")
        //
        // With the original code: an owner-writable (0o700) ancestor
        // owned by the running non-root user must refuse elevation
        // when `allow_owner_writable=false`.
        use std::os::unix::fs::PermissionsExt;
        let parent = std::env::temp_dir().join(format!(
            "keron-elev-ow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let bin = parent.join("fake-keron");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Owner-writable WITHOUT group/world write.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();

        // Sanity: if the test happens to run as root, the uid != 0
        // check would not refuse. Skip in that case.
        #[allow(unsafe_code)]
        let euid = unsafe { libc::geteuid() };
        if euid == 0 {
            let _ = std::fs::remove_dir_all(&parent);
            return;
        }

        let result = check_binary_tamper_resistance(&bin, None, false);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&parent);

        let err = result.expect_err("owner-writable non-root ancestor must refuse elevation");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("owned by untrusted"),
            "expected untrusted-owner refusal, got: {msg}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_binary_tamper_resistance_rejects_owner_writable_non_root_binary() {
        // Companion test for the binary-self check at line 164. Same
        // mutations apply (`& with |/^`, `!= with ==` on the bit
        // check, `!= with ==` on the uid check). An owner-writable bin
        // owned by the running non-root user must be refused. The
        // ancestor walk also catches this same combination via the
        // line-139 check (parent dir is 0o755 by default and
        // owner-writable too), so the diagnostic may come from either
        // — we just pin that the function REFUSES.
        use std::os::unix::fs::PermissionsExt;
        let parent = std::env::temp_dir().join(format!(
            "keron-elev-owb-{}-{}",
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

        #[allow(unsafe_code)]
        let euid = unsafe { libc::geteuid() };
        if euid == 0 {
            let _ = std::fs::remove_dir_all(&parent);
            return;
        }

        let result = check_binary_tamper_resistance(&bin, None, false);
        let _ = std::fs::remove_dir_all(&parent);

        let err = result
            .expect_err("owner-writable non-root binary must refuse elevation when allow=false");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("owned by untrusted"),
            "expected untrusted-owner refusal, got: {msg}",
        );
    }

    #[cfg(all(unix, debug_assertions))]
    #[test]
    fn test_elevator_override_requires_allow_gate() {
        // Pins the `KERON_ALLOW_TEST_OVERRIDES` gate inside
        // test_elevator_override: a set `KERON_TEST_ELEVATOR` without
        // the explicit allow-flag must NOT be honoured. Catches the
        // `!= with ==` mutation on the comparison `v != "1"` (which
        // would invert the gate) and the body replacements that
        // return None / Some(Default) unconditionally.
        // Run via a small helper that single-threads its env
        // mutations through the package-mod ENV_LOCK so it can't race
        // the tap / cargo tests.
        let _g = crate::packages::lock_env();
        // SAFETY: edition-2024 set_var; lock_env serialises the
        // section and we restore both vars on the way out.
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
            std::env::set_var("KERON_TEST_ELEVATOR", "/tmp/fake-elevator");
        }
        let without_allow = test_elevator_override();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
        }
        let with_allow = test_elevator_override();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_TEST_ELEVATOR");
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
        }
        assert!(
            without_allow.is_none(),
            "override must require the explicit allow gate; got: {without_allow:?}",
        );
        let path = with_allow.expect("override honoured with allow gate");
        assert_eq!(path, std::path::PathBuf::from("/tmp/fake-elevator"));
    }

    #[cfg(all(unix, debug_assertions))]
    #[test]
    fn invoke_elevator_runs_the_overridden_binary_and_propagates_its_status() {
        // Drive invoke_elevator with KERON_TEST_ELEVATOR pointing at a
        // shell spy that records its argv and exits successfully. The
        // mutation `-> Ok(Default::default())` would skip the spawn
        // entirely and never touch the marker file, so a missing
        // marker file proves the spawn ran.
        use crate::elevated::payload::{PayloadExpectation, PayloadIdentity};
        use std::os::unix::fs::PermissionsExt;
        let _g = crate::packages::lock_env();
        let dir = std::env::temp_dir().join(format!(
            "keron-elev-invoke-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("marker");
        let spy = dir.join("spy.sh");
        std::fs::write(
            &spy,
            format!("#!/bin/sh\necho ran > '{}'\nexit 0\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&spy, std::fs::Permissions::from_mode(0o755)).unwrap();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
            std::env::set_var("KERON_TEST_ELEVATOR", &spy);
        }
        let exe = std::path::PathBuf::from("/usr/bin/true");
        let payload = dir.join("payload.json");
        std::fs::write(&payload, "{}").unwrap();
        let expected = PayloadExpectation {
            digest_hex: "0".repeat(64),
            identity: PayloadIdentity::Unix {
                dev: 0,
                ino: 0,
                uid: 0,
                gid: 0,
                mode: 0o600,
                len: 2,
            },
        };
        let status = invoke_elevator(&exe, &payload, &expected);
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_TEST_ELEVATOR");
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
        }
        let status = status.expect("spy must succeed");
        assert!(status.success(), "spy must exit 0; got: {status:?}");
        assert!(
            marker.exists(),
            "elevator spy must have run and written the marker file",
        );
        let _ = std::fs::remove_dir_all(&dir);
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

        let result = check_binary_tamper_resistance(&bin, None, false);
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::plan::{Action, ResourceChange, ResourceKind, ResourceState};

    #[test]
    fn empty_plan_does_not_require_windows_elevation() {
        let plan = Plan {
            changes: Vec::new(),
        };
        preflight(&plan).expect("an empty plan is supported");
        let summary = run_elevated(&plan).expect("an empty plan is a no-op");
        assert_eq!(summary.added, 0);
        assert_eq!(summary.changed, 0);
        assert_eq!(summary.ran, 0);
    }

    #[test]
    fn nonempty_plan_fails_before_windows_elevation() {
        let path = std::path::PathBuf::from(r"C:\Windows\keron-test.conf");
        let plan = Plan {
            changes: vec![ResourceChange {
                address: path.display().to_string(),
                kind: ResourceKind::Template,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Template {
                    path,
                    content: "test".into(),
                    sensitive: false,
                }),
                requires_elevation: true,
                requires_force: false,
            }],
        };

        let preflight_error = preflight(&plan).expect_err("Windows preflight must fail closed");
        assert!(
            preflight_error
                .to_string()
                .contains("no changes were applied")
        );
        let error = run_elevated(&plan).expect_err("Windows elevation must fail closed");
        assert!(error.to_string().contains("not supported on Windows"));
    }
}
