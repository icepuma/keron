//! Entry point for the elevated keron subprocess.
//!
//! Invoked as `keron __apply-elevated <payload-path>` by the
//! unprivileged parent through a Unix elevator. Reads the JSON payload,
//! validates that every change is an action the planner is allowed to
//! elevate, then applies each change via the shared
//! [`crate::execute::apply_change_one_in`].

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::elevated::payload::{ElevatedPayload, PAYLOAD_VERSION, PayloadExpectation};
use crate::execute::{ApplyContext, apply_change_one_in};
use crate::plan::{Action, ResourceChange, ResourceKind, ResourceState};

/// Read `payload`, validate its privileged action set, and apply it.
///
/// Called by `keron-cli`'s hidden `__apply-elevated` subcommand.
/// Writes a one-line summary to `stdout` for the unprivileged
/// parent's user to see (stdio is inherited through the elevator).
///
/// # Errors
/// Errors on payload parse failure, version mismatch, contract
/// validation failure, or apply failure. The error is intentionally
/// propagated so the Unix elevator sees a non-zero exit.
pub fn run(payload_path: &Path, expected: &PayloadExpectation) -> Result<()> {
    let bytes = crate::elevated::payload::read_verified(payload_path, expected)?;
    let payload: ElevatedPayload = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing elevated payload `{}`", payload_path.display()))?;
    if payload.version != PAYLOAD_VERSION {
        bail!(
            "elevated payload version mismatch: file has {}, this keron expects {PAYLOAD_VERSION}",
            payload.version
        );
    }
    validate_payload(&payload)?;

    let mut stdout = std::io::stdout().lock();
    let total = payload.changes.len();
    let mut applied = 0usize;

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
    }

    let _ = writeln!(
        stdout,
        "elevated apply complete: {applied}/{total} resources processed",
    );
    Ok(())
}

/// Treat the serialized payload as hostile input. The digest and inode
/// expectation protect a parent-created payload from replacement while
/// sudo is starting, but they do not prove that the bytes came from the
/// planner: a caller can invoke the hidden subcommand directly and supply
/// a matching digest. Re-establish the planner's elevation contract before
/// the first side effect so the privileged entry point cannot become a
/// generic root shell/package/GPG runner.
fn validate_payload(payload: &ElevatedPayload) -> Result<()> {
    #[cfg(windows)]
    if !payload.changes.is_empty() {
        bail!(
            "elevated filesystem writes are disabled on Windows until the apply path has a reparse-point-safe handle walker"
        );
    }
    for (index, change) in payload.changes.iter().enumerate() {
        validate_change(change).with_context(|| {
            format!(
                "invalid elevated payload change {} of {}",
                index + 1,
                payload.changes.len(),
            )
        })?;
    }
    Ok(())
}

fn validate_change(change: &ResourceChange) -> Result<()> {
    if !change.requires_elevation {
        bail!(
            "change `{}` is not marked as requiring elevation",
            change.address
        );
    }
    if change.requires_force != change.compute_requires_force() {
        bail!(
            "change `{}` has an incoherent force-approval flag",
            change.address
        );
    }

    match change.action {
        Action::Create => {
            if change.before.is_some() {
                bail!(
                    "create change `{}` unexpectedly has prior state",
                    change.address
                );
            }
            let after = change.after.as_ref().with_context(|| {
                format!("create change `{}` has no desired state", change.address)
            })?;
            validate_filesystem_state(change, after)?;
        }
        Action::Update => {
            let before = change.before.as_ref().with_context(|| {
                format!("update change `{}` has no prior state", change.address)
            })?;
            let after = change.after.as_ref().with_context(|| {
                format!("update change `{}` has no desired state", change.address)
            })?;
            validate_filesystem_state(change, before)?;
            validate_filesystem_state(change, after)?;
            match (before, after) {
                (
                    ResourceState::Template { path: before, .. },
                    ResourceState::Template { path: after, .. },
                )
                | (
                    ResourceState::Symlink { from: before, .. },
                    ResourceState::Symlink { from: after, .. },
                ) if before == after => {}
                _ => bail!(
                    "update change `{}` changes resource kind or destination",
                    change.address
                ),
            }
        }
        Action::Run | Action::NoOp => bail!(
            "change `{}` has action {:?}, which is never elevated",
            change.address,
            change.action,
        ),
    }
    Ok(())
}

fn validate_filesystem_state(change: &ResourceChange, state: &ResourceState) -> Result<()> {
    let (kind, destination) = match state {
        ResourceState::Template { path, .. } => (ResourceKind::Template, path.as_path()),
        ResourceState::Symlink { from, .. } => (ResourceKind::Symlink, from.as_path()),
        ResourceState::Package { .. }
        | ResourceState::Tap(_)
        | ResourceState::Shell { .. }
        | ResourceState::SshKey { .. }
        | ResourceState::GpgKey { .. } => {
            bail!(
                "resource kind {:?} is not allowed in an elevated payload",
                change.kind
            )
        }
    };
    if change.kind != kind {
        bail!(
            "change `{}` declares kind {:?} but carries {:?} state",
            change.address,
            change.kind,
            kind,
        );
    }
    validate_destination(destination)?;
    if change.address != destination.display().to_string() {
        bail!(
            "change address `{}` does not match destination `{}`",
            change.address,
            destination.display(),
        );
    }
    Ok(())
}

fn validate_destination(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.file_name().is_none() {
        bail!(
            "elevated filesystem destination must be an absolute leaf path, got `{}`",
            path.display()
        );
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        bail!(
            "elevated filesystem destination must be normalized, got `{}`",
            path.display()
        );
    }
    Ok(())
}

/// Fast pre-check: refuse early when any ancestor is a symlink
/// owned by a non-root user.
///
/// Cheaper than the full `openat`-based walk inside
/// [`crate::elevated::safe_write::ParentDir::open`] (which is the
/// authoritative TOCTOU-safe walker invoked by the Create path).
/// We keep the pre-check here so:
/// - Update actions get an upfront bail before their fd-anchored
///   replacement path runs.
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
        ResourceState::SshKey { private_path, .. } => Some(private_path.clone()),
        ResourceState::Package { .. }
        | ResourceState::Tap(_)
        | ResourceState::Shell { .. }
        | ResourceState::GpgKey { .. } => None,
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

    #[cfg(unix)]
    fn invalid_elevated_changes(root: &Path) -> Vec<ResourceChange> {
        use crate::plan::{PackageManager, ShellKind, TapSpec};

        vec![
            ResourceChange {
                address: "shell".into(),
                kind: ResourceKind::Shell,
                action: Action::Run,
                before: None,
                after: Some(ResourceState::Shell {
                    kind: ShellKind::Sh,
                    name: "shell".into(),
                    cwd: root.to_path_buf(),
                    script: "exit 0".into(),
                    sensitive: false,
                }),
                requires_elevation: true,
                requires_force: false,
            },
            ResourceChange {
                address: "gpg".into(),
                kind: ResourceKind::GpgKey,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::GpgKey {
                    fingerprint: "DEADBEEF".into(),
                    key: "secret".into(),
                }),
                requires_elevation: true,
                requires_force: false,
            },
            ResourceChange {
                address: "package".into(),
                kind: ResourceKind::Package,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Package {
                    manager: PackageManager::Cargo,
                    name: "ripgrep".into(),
                    tap: None,
                }),
                requires_elevation: true,
                requires_force: false,
            },
            ResourceChange {
                address: "tap".into(),
                kind: ResourceKind::Tap,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Tap(TapSpec {
                    user_tap: "example/tools".into(),
                    url: None,
                })),
                requires_elevation: true,
                requires_force: false,
            },
            ResourceChange {
                address: root.join("noop").display().to_string(),
                kind: ResourceKind::Template,
                action: Action::NoOp,
                before: None,
                after: None,
                requires_elevation: true,
                requires_force: false,
            },
            ResourceChange {
                address: root.join("wrong-kind").display().to_string(),
                kind: ResourceKind::Package,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Template {
                    path: root.join("wrong-kind"),
                    content: "x".into(),
                    sensitive: false,
                }),
                requires_elevation: true,
                requires_force: false,
            },
            ResourceChange {
                address: root.join("unmarked").display().to_string(),
                kind: ResourceKind::Template,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Template {
                    path: root.join("unmarked"),
                    content: "x".into(),
                    sensitive: false,
                }),
                requires_elevation: false,
                requires_force: false,
            },
        ]
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
    fn invalid_payload_changes_are_rejected_before_any_mutation() {
        use crate::elevated::payload::write_payload;
        use crate::plan::Plan;

        let root = std::env::temp_dir().join(format!(
            "keron-elevated-contract-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        std::fs::create_dir_all(&root).unwrap();
        let invalid = invalid_elevated_changes(&root);

        for (index, invalid_change) in invalid.into_iter().enumerate() {
            let marker = root.join(format!("marker-{index}"));
            let allowed_first = ResourceChange {
                address: marker.display().to_string(),
                kind: ResourceKind::Template,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Template {
                    path: marker.clone(),
                    content: "must not be written".into(),
                    sensitive: false,
                }),
                requires_elevation: true,
                requires_force: false,
            };
            let plan = Plan {
                changes: vec![allowed_first, invalid_change],
            };
            let payload = write_payload(&plan).unwrap();
            let expected = payload.expected().clone();
            run(payload.path(), &expected).expect_err("invalid payload must be rejected");
            assert!(
                !marker.exists(),
                "validation must finish before applying change 1 (case {index})",
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn elevated_success_keeps_the_elevated_process_owner() {
        use crate::elevated::payload::write_payload;
        use crate::plan::Plan;
        use std::os::unix::fs::MetadataExt;

        let root = std::env::temp_dir().join(format!(
            "keron-elevated-owner-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("managed.conf");
        #[allow(unsafe_code)]
        let uid = unsafe { libc::geteuid() };
        let plan = Plan {
            changes: vec![ResourceChange {
                address: path.display().to_string(),
                kind: ResourceKind::Template,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Template {
                    path: path.clone(),
                    content: "system-owned".into(),
                    sensitive: false,
                }),
                requires_elevation: true,
                requires_force: false,
            }],
        };
        let payload = write_payload(&plan).unwrap();
        let expected = payload.expected().clone();
        run(payload.path(), &expected).expect("valid elevated template should apply");
        let actual_uid = std::fs::metadata(&path).unwrap().uid();
        assert_eq!(actual_uid, uid);
        let _ = std::fs::remove_dir_all(&root);
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
