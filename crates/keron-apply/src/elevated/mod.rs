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
    let exe = current_exe_canonicalized()?;
    let status = invoke_elevator(&exe, tempfile.path())?;
    if !status.success() {
        bail!("elevated apply exited with status {status}; see output above for details");
    }
    // The child writes its summary to stdout. The parent inherited
    // stdio so the user already saw it; we report a generic
    // "succeeded" tally rather than parse the child's summary. v1
    // doesn't try to merge counts across the boundary; both halves
    // are visible to the user.
    Ok(ExecuteSummary {
        added: 0,
        changed: 0,
        destroyed: 0,
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
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = canonical.parent()
            && let Ok(meta) = std::fs::metadata(parent)
            && meta.permissions().mode() & 0o002 != 0
        {
            bail!(
                "refusing to elevate: `{}` lives in a world-writable directory (`{}`)",
                canonical.display(),
                parent.display(),
            );
        }
    }
    Ok(canonical)
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
    // Tty-friendly first: sudo and doas always prompt on the
    // controlling terminal. pkexec last because it requires a polkit
    // auth agent that only runs in a graphical desktop session;
    // falling back to it on a headless box just produces a confusing
    // "no agent" failure.
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
#[cfg(unix)]
fn test_elevator_override() -> Option<std::path::PathBuf> {
    std::env::var_os("KERON_TEST_ELEVATOR").map(std::path::PathBuf::from)
}

#[cfg(unix)]
fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
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
        // SAFETY: test-only env mutation; the lint is fine because
        // single-threaded test_elevator_override doesn't read PATH.
        // Use a scoped set/restore so other tests aren't affected.
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
}
