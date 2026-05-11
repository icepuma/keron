//! Entry point for the elevated keron subprocess.
//!
//! Invoked as `keron __apply-elevated <payload-path>` by the
//! unprivileged parent through sudo / `ShellExecuteExW`. Reads the
//! JSON payload, applies each [`crate::plan::ResourceChange`] via the
//! shared [`crate::execute::apply_change_one`], then `chown`s every
//! affected filesystem path back to the calling user.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::elevated::chown;
use crate::elevated::payload::{ElevatedPayload, OwnerId, PAYLOAD_VERSION};
use crate::execute::apply_change_one;
use crate::plan::{ResourceChange, ResourceState};

/// Read `payload`, apply its changes, and chown-back.
///
/// Called by `keron-cli`'s hidden `__apply-elevated` subcommand.
/// Writes a one-line summary to `stdout` for the unprivileged
/// parent's user to see (stdio is inherited through the elevator).
///
/// # Errors
/// Errors on payload parse failure, version mismatch, apply failure,
/// or chown-back failure. The error is intentionally propagated so
/// the elevator (sudo / `ShellExecuteExW`) sees a non-zero exit.
pub fn run(payload_path: &Path) -> Result<()> {
    let bytes = std::fs::read(payload_path)
        .with_context(|| format!("reading elevated payload `{}`", payload_path.display()))?;
    let payload: ElevatedPayload = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing elevated payload `{}`", payload_path.display()))?;
    if payload.version != PAYLOAD_VERSION {
        bail!(
            "elevated payload version mismatch: file has {}, this keron expects {PAYLOAD_VERSION}",
            payload.version
        );
    }

    let mut stdout = std::io::stdout().lock();
    let total = payload.changes.len();
    let mut applied = 0usize;
    let mut ownership_failures: Vec<PathBuf> = Vec::new();

    for change in &payload.changes {
        apply_change_one(change).with_context(|| {
            format!(
                "elevated apply failed after {applied} of {total} resources at `{}`",
                change.address,
            )
        })?;
        applied += 1;
        if let Some(path) = leaf_path(change)
            && let Err(e) = chown::set_owner(&path, &payload.owner)
        {
            // Write succeeded, chown failed: don't abort the loop —
            // finish the rest and surface a complete list of broken
            // ownerships at the end.
            let _ = writeln!(
                stdout,
                "warning: failed to set owner on `{}`: {e:#}",
                path.display(),
            );
            ownership_failures.push(path);
        }
    }

    let _ = writeln!(
        stdout,
        "elevated apply complete: {applied}/{total} resources processed",
    );
    if !ownership_failures.is_empty() {
        let example = describe_owner(&payload.owner);
        let _ = writeln!(
            stdout,
            "warning: {} resource(s) had ownership-fixup failures. Re-run \
             `chown {example} <path>` (or `chown -h …` for symlinks) to repair:",
            ownership_failures.len(),
        );
        for p in &ownership_failures {
            let _ = writeln!(stdout, "  - {}", p.display());
        }
        bail!(
            "elevated apply finished but {} resource(s) ended up owned by root; see \
             warnings above for the repair command",
            ownership_failures.len(),
        );
    }
    Ok(())
}

/// Filesystem path that should receive the chown-back. We only
/// chown the *leaf* — intermediate directories created by
/// `mkdir -p` keep their pre-existing ownership (chowning `/etc`
/// back to the user would be a disaster).
fn leaf_path(change: &ResourceChange) -> Option<PathBuf> {
    let state = change.after.as_ref().or(change.before.as_ref())?;
    match state {
        ResourceState::Symlink { from, .. } => Some(from.clone()),
        ResourceState::Template { path, .. } => Some(path.clone()),
        ResourceState::Package { .. } => None,
    }
}

fn describe_owner(owner: &OwnerId) -> String {
    match owner {
        OwnerId::Posix { uid, gid } => format!("-h {uid}:{gid}"),
        OwnerId::Windows { sid } => format!("(SID {sid})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Action, ResourceKind};
    use std::path::PathBuf;

    fn change(state: ResourceState) -> ResourceChange {
        ResourceChange {
            address: "x".into(),
            kind: ResourceKind::Symlink,
            action: Action::Create,
            before: None,
            after: Some(state),
            requires_elevation: true,
            requires_force: false,
        }
    }

    #[test]
    fn leaf_path_for_symlink_returns_from() {
        let c = change(ResourceState::Symlink {
            from: PathBuf::from("/a"),
            to: PathBuf::from("/b"),
        });
        assert_eq!(leaf_path(&c), Some(PathBuf::from("/a")));
    }

    #[test]
    fn leaf_path_for_template_returns_path() {
        let c = change(ResourceState::Template {
            path: PathBuf::from("/c"),
            content: "x".into(),
            sensitive: false,
        });
        assert_eq!(leaf_path(&c), Some(PathBuf::from("/c")));
    }

    #[test]
    fn leaf_path_for_package_returns_none() {
        let c = change(ResourceState::Package {
            manager: crate::plan::PackageManager::Brew,
            name: "ripgrep".into(),
        });
        assert_eq!(leaf_path(&c), None);
    }

    #[test]
    fn describe_owner_renders_posix_and_windows() {
        let p = describe_owner(&OwnerId::Posix {
            uid: 1000,
            gid: 1000,
        });
        assert!(p.contains("1000:1000"), "got: {p}");
        let w = describe_owner(&OwnerId::Windows {
            sid: "S-1-5-21-x".into(),
        });
        assert!(w.contains("S-1-5-21-x"), "got: {w}");
    }
}
