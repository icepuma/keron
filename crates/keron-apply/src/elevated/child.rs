//! Entry point for the elevated keron subprocess.
//!
//! Invoked as `keron __apply-elevated <payload-path>` by the
//! unprivileged parent through sudo / `ShellExecuteExW`. Reads the
//! JSON payload, applies each [`crate::plan::ResourceChange`] via the
//! shared [`crate::execute::apply_change_one_in`], then `chown`s every
//! affected filesystem path back to the calling user.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::elevated::chown;
use crate::elevated::payload::{ElevatedPayload, OwnerId, PAYLOAD_VERSION};
use crate::execute::{ApplyContext, apply_change_one_in};
use crate::plan::{ResourceChange, ResourceState};
use crate::terminal_safe::show_path;

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
    let mut stderr = std::io::stderr().lock();
    let total = payload.changes.len();
    let mut applied = 0usize;
    let mut ownership_failures: Vec<PathBuf> = Vec::new();

    for change in &payload.changes {
        if let Some(leaf) = leaf_path(change) {
            refuse_symlinked_ancestors(&leaf).with_context(|| {
                format!(
                    "elevated apply refused on `{}` before action",
                    change.address
                )
            })?;
        }
        apply_change_one_in(change, ApplyContext::Elevated).with_context(|| {
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
            // ownerships at the end. Per-resource warnings go to
            // stderr so a closed stdout pipe can't swallow them.
            let _ = writeln!(
                stderr,
                "warning: failed to set owner on `{}`: {e:#}",
                show_path(&path),
            );
            ownership_failures.push(path);
        }
    }

    let _ = writeln!(
        stdout,
        "elevated apply complete: {applied}/{total} resources processed",
    );
    if !ownership_failures.is_empty() {
        // The repair list is load-bearing: without it the user has
        // no way to know which paths still need chown-back. Write to
        // stderr (less likely to be redirected) and propagate the
        // write error — losing this list would leave the cluster of
        // root-owned files invisible.
        let example = describe_owner(&payload.owner);
        writeln!(
            stderr,
            "warning: {} resource(s) had ownership-fixup failures. Re-run \
             `chown {example} <path>` (or `chown -h …` for symlinks) to repair:",
            ownership_failures.len(),
        )
        .context("emitting ownership-fixup repair list to stderr")?;
        for p in &ownership_failures {
            writeln!(stderr, "  - {}", show_path(p))
                .context("emitting ownership-fixup repair list to stderr")?;
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
/// Fast pre-check: refuse early when any ancestor is a symlink
/// owned by a non-root user.
///
/// Cheaper than the full `openat`-based walk inside
/// [`crate::elevated::safe_write::ParentDir::open`] (which is the
/// authoritative TOCTOU-safe walker invoked by the Create path).
/// We keep the pre-check here so:
/// - Update actions, which go through the legacy `fs::*` paths,
///   still get an upfront bail when the parent chain has been
///   tampered with.
/// - The error message names the offending ancestor before any
///   filesystem mutation runs.
///
/// Policy matches `safe_write`: root-owned symlinks are allowed
/// (system paths like `/var -> /private/var` on macOS), non-root
/// symlinks are refused.
fn refuse_symlinked_ancestors(leaf: &Path) -> Result<()> {
    let mut cursor = leaf.parent();
    while let Some(dir) = cursor {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                #[cfg(unix)]
                let owner_uid = std::os::unix::fs::MetadataExt::uid(&meta);
                #[cfg(not(unix))]
                let owner_uid = 0u32;
                if owner_uid != 0 {
                    bail!(
                        "elevated apply refuses to write under non-root symlinked ancestor `{}` (uid {owner_uid}); a co-resident user could redirect a root-owned write",
                        dir.display(),
                    );
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "inspecting ancestor `{}` for symlink chain: {e}",
                    dir.display()
                ));
            }
        }
        cursor = dir.parent();
    }
    Ok(())
}

fn leaf_path(change: &ResourceChange) -> Option<PathBuf> {
    let state = change.after.as_ref().or(change.before.as_ref())?;
    match state {
        ResourceState::Symlink { from, .. } => Some(from.clone()),
        ResourceState::Template { path, .. } => Some(path.clone()),
        ResourceState::Package { .. } | ResourceState::Tap(_) | ResourceState::Shell { .. } => None,
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
    fn leaf_path_for_symlink_returns_target() {
        let c = change(ResourceState::Symlink {
            from: PathBuf::from("/a"),
            to: PathBuf::from("/b"),
        });
        assert_eq!(leaf_path(&c), Some(PathBuf::from("/a")));
    }

    #[test]
    fn leaf_path_for_template_returns_target() {
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
            tap: None,
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

    #[cfg(unix)]
    #[test]
    fn refuse_symlinked_ancestors_passes_when_chain_is_clean() {
        let root = std::env::temp_dir().join(format!(
            "keron-rsa-clean-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let dir = root.join("a").join("b");
        std::fs::create_dir_all(&dir).unwrap();
        let leaf = dir.join("c");
        // No symlinks in chain — must accept. Catches the
        // `Ok(())`-everywhere mutation (function-body replacement).
        refuse_symlinked_ancestors(&leaf).expect("clean ancestor chain must pass");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn refuse_symlinked_ancestors_rejects_non_root_symlinked_ancestor() {
        let root = std::env::temp_dir().join(format!(
            "keron-rsa-ns-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        std::fs::create_dir_all(&root).unwrap();
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let leaf = link.join("inside");

        // Test-host user owns the symlink (not root). Must refuse.
        // Catches the `is_symlink() with false` mutation that would
        // skip the symlink branch entirely, and the inverted-mode
        // mutations that flip the owner check.
        let err = refuse_symlinked_ancestors(&leaf)
            .expect_err("non-root-owned symlinked ancestor must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("non-root symlinked ancestor") || msg.contains("symlinked ancestor"),
            "expected symlink refusal message, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn refuse_symlinked_ancestors_bails_on_non_enoent_stat_error() {
        // EACCES on an intermediate dir (e.g., a closed-off parent)
        // is NOT NotFound. The `Err(e)` arm must bail rather than
        // silently treat it as "OK to skip". Pins the
        // `NotFound with true`-mutated guard, which would funnel all
        // errors through the silent-skip path and admit walks
        // beneath sealed ancestors.
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "keron-rsa-eacces-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        std::fs::create_dir_all(&root).unwrap();
        let sealed = root.join("sealed");
        std::fs::create_dir(&sealed).unwrap();
        let inner = sealed.join("inner");
        std::fs::create_dir(&inner).unwrap();
        let leaf = inner.join("leaf");
        // Strip all bits from the sealed directory: stat of
        // `sealed/inner` from outside now returns EACCES (cannot
        // search sealed).
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = refuse_symlinked_ancestors(&leaf);

        // Restore perms before assertions so cleanup works even if
        // the assertion fails.
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&root);

        let err = result.expect_err("EACCES on ancestor must surface, not be papered over");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("inspecting ancestor") || msg.contains("symlink chain"),
            "expected ancestor-inspection error, got: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuse_symlinked_ancestors_treats_missing_ancestors_as_ok() {
        // `leaf`'s parent doesn't exist yet — that's the
        // mkdir-on-apply case. The ENOENT match guard must accept.
        // Catches the `NotFound with true/false`-mutated guards.
        let leaf = std::env::temp_dir()
            .join(format!(
                "keron-rsa-nx-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_nanos()),
            ))
            .join("missing-subdir")
            .join("leaf");
        refuse_symlinked_ancestors(&leaf)
            .expect("ancestors that don't exist yet are fine (will be mkdir'd)");
    }
}
