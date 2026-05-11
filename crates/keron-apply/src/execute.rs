//! Apply phase. Walks the [`Plan`] in order and performs the side
//! effects each [`ResourceChange`] demands.
//!
//! v1 supports symlinks end-to-end (create, update, destroy, no-op).
//! Other resource kinds bail with a clear "not yet implemented"
//! diagnostic — they land alongside the planner work that diffs them
//! against live state.

use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::packages;
use crate::plan::{Action, Plan, ResourceChange, ResourceKind, ResourceState};

#[derive(Debug, Clone, Copy, Default)]
pub struct ExecuteSummary {
    pub added: usize,
    pub changed: usize,
    pub destroyed: usize,
}

pub fn execute(plan: &Plan) -> Result<ExecuteSummary> {
    let mut summary = ExecuteSummary::default();
    for change in &plan.changes {
        apply_change(change, &mut summary)?;
    }
    Ok(summary)
}

fn apply_change(change: &ResourceChange, summary: &mut ExecuteSummary) -> Result<()> {
    apply_change_one(change)?;
    match change.action {
        Action::Create => summary.added += 1,
        Action::Update => summary.changed += 1,
        Action::Destroy => summary.destroyed += 1,
        Action::NoOp => {}
    }
    Ok(())
}

/// Apply a single change. Shared between the in-process executor and
/// the elevated child entry point so the two stay in lockstep.
///
/// # Errors
/// Errors when the underlying filesystem call fails or when the
/// resource kind has no executor support yet (templates, directories,
/// packages — they bail with a clear "not yet implemented" message
/// that names the kind).
pub fn apply_change_one(change: &ResourceChange) -> Result<()> {
    match change.action {
        Action::NoOp => Ok(()),
        Action::Create => {
            let state = change
                .after
                .as_ref()
                .with_context(|| format!("create `{}` has no desired state", change.address))?;
            apply_create(state).with_context(|| format!("creating `{}`", change.address))
        }
        Action::Update => {
            let before = change
                .before
                .as_ref()
                .with_context(|| format!("update `{}` has no prior state", change.address))?;
            let after = change
                .after
                .as_ref()
                .with_context(|| format!("update `{}` has no desired state", change.address))?;
            apply_update(before, after).with_context(|| format!("updating `{}`", change.address))
        }
        Action::Destroy => {
            let state = change
                .before
                .as_ref()
                .with_context(|| format!("destroy `{}` has no prior state", change.address))?;
            apply_destroy(state).with_context(|| format!("destroying `{}`", change.address))
        }
    }
}

fn apply_create(state: &ResourceState) -> Result<()> {
    match state {
        ResourceState::Symlink { from, to } => create_symlink(from, to),
        ResourceState::Package { manager, name } => packages::install(*manager, name),
        other => bail!(unsupported_kind(other)),
    }
}

fn apply_update(before: &ResourceState, after: &ResourceState) -> Result<()> {
    match (before, after) {
        (ResourceState::Symlink { from: bf, .. }, ResourceState::Symlink { from: af, to: at }) => {
            // Update only fires when both sides point at the same path.
            // The planner produces matched pairs; bail loudly if that
            // invariant ever drifts.
            if bf != af {
                bail!(
                    "symlink update path mismatch: `{}` vs `{}`",
                    bf.display(),
                    af.display(),
                );
            }
            remove_symlink(bf)?;
            create_symlink(af, at)
        }
        _ => bail!(unsupported_kind(after)),
    }
}

fn apply_destroy(state: &ResourceState) -> Result<()> {
    match state {
        ResourceState::Symlink { from, .. } => remove_symlink(from),
        other => bail!(unsupported_kind(other)),
    }
}

fn create_symlink(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = from.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory `{}`", parent.display()))?;
    }
    symlink_impl(to, from)
        .with_context(|| format!("symlinking `{}` -> `{}`", from.display(), to.display()))
}

fn remove_symlink(path: &Path) -> Result<()> {
    // `symlink_metadata` does not traverse the link itself, so a broken
    // symlink (target missing) still reports `is_symlink()`. Refusing
    // to call `remove_file` on a real file is the safety net that
    // matches the planner's "won't overwrite" rule.
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("inspecting `{}`", path.display()))?;
    if !meta.file_type().is_symlink() {
        bail!("`{}` is not a symlink; refusing to remove", path.display());
    }
    fs::remove_file(path).with_context(|| format!("removing symlink `{}`", path.display()))
}

fn unsupported_kind(state: &ResourceState) -> String {
    let kind = match state {
        ResourceState::Symlink { .. } => ResourceKind::Symlink,
        ResourceState::Template { .. } => ResourceKind::Template,
        ResourceState::Directory { .. } => ResourceKind::Directory,
        ResourceState::Package { .. } => ResourceKind::Package,
    };
    format!(
        "executor not yet implemented for {} resources",
        kind.label()
    )
}

#[cfg(unix)]
fn symlink_impl(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_impl(target: &Path, link: &Path) -> io::Result<()> {
    // Dotfile flows mostly link files, but a project that vendors a
    // whole config directory (`~/.config/nvim` -> `<repo>/nvim`) also
    // expects this to "just work". Pick the right Windows variant up
    // front so the user never has to think about file-vs-dir links.
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PackageManager, ResourceKind};
    use std::env;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let p = env::temp_dir().join(format!(
                "keron-execute-test-{tag}-{}-{n}",
                std::process::id()
            ));
            if p.exists() {
                fs::remove_dir_all(&p).ok();
            }
            fs::create_dir_all(&p).unwrap();
            Self { path: p }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn change(
        action: Action,
        before: Option<ResourceState>,
        after: Option<ResourceState>,
    ) -> ResourceChange {
        let probe = before
            .as_ref()
            .or(after.as_ref())
            .expect("a change must have at least one state");
        ResourceChange {
            address: match probe {
                ResourceState::Symlink { from, .. } => from.display().to_string(),
                ResourceState::Template { path, .. } | ResourceState::Directory { path } => {
                    path.display().to_string()
                }
                ResourceState::Package { manager, name } => format!("{}:{}", manager.label(), name),
            },
            kind: match probe {
                ResourceState::Symlink { .. } => ResourceKind::Symlink,
                ResourceState::Template { .. } => ResourceKind::Template,
                ResourceState::Directory { .. } => ResourceKind::Directory,
                ResourceState::Package { .. } => ResourceKind::Package,
            },
            action,
            before,
            after,
            requires_elevation: false,
        }
    }

    #[test]
    fn create_symlink_writes_link_on_disk() {
        let d = TempDir::new("create");
        let target = d.path.join("real");
        fs::write(&target, "hi").unwrap();
        let link = d.path.join("alias");

        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: target.clone(),
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.added, 1);
        assert_eq!(summary.changed, 0);
        assert_eq!(summary.destroyed, 0);
        let resolved = fs::read_link(&link).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn create_symlink_creates_missing_parent_directories() {
        // Dotfile flows expect `~/.config/nvim` to "just work" even
        // when `~/.config` does not yet exist; the executor mkdirs the
        // chain so users don't have to declare every intermediate
        // directory by hand.
        let d = TempDir::new("create-parent");
        let target = d.path.join("real");
        fs::write(&target, "hi").unwrap();
        let link = d.path.join("a/b/c/alias");

        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: target.clone(),
                }),
            )],
        };
        execute(&plan).unwrap();
        assert!(link.is_symlink(), "missing symlink at {}", link.display());
        let resolved = fs::read_link(&link).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn update_symlink_replaces_existing_target() {
        let d = TempDir::new("update");
        let old_target = d.path.join("old");
        let new_target = d.path.join("new");
        fs::write(&old_target, "old").unwrap();
        fs::write(&new_target, "new").unwrap();
        let link = d.path.join("alias");
        symlink_impl(&old_target, &link).unwrap();

        let plan = Plan {
            changes: vec![change(
                Action::Update,
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: old_target,
                }),
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: new_target.clone(),
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.changed, 1);
        let resolved = fs::read_link(&link).unwrap();
        assert_eq!(resolved, new_target);
    }

    #[test]
    fn destroy_symlink_removes_link() {
        let d = TempDir::new("destroy");
        let target = d.path.join("real");
        fs::write(&target, "hi").unwrap();
        let link = d.path.join("alias");
        symlink_impl(&target, &link).unwrap();

        let plan = Plan {
            changes: vec![change(
                Action::Destroy,
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: target.clone(),
                }),
                None,
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.destroyed, 1);
        assert!(!link.exists() && !link.is_symlink());
        assert!(
            target.exists(),
            "destroy should leave the link target alone"
        );
    }

    #[test]
    fn destroy_refuses_to_remove_real_files() {
        // The "destroy only operates on symlinks" guard backs up the
        // planner-side "won't overwrite real files" rule. If the user
        // hand-edits a plan or some future code path constructs a
        // Destroy against a regular file, we still refuse.
        let d = TempDir::new("destroy-real");
        let path = d.path.join("real");
        fs::write(&path, "data").unwrap();
        let plan = Plan {
            changes: vec![change(
                Action::Destroy,
                Some(ResourceState::Symlink {
                    from: path.clone(),
                    to: PathBuf::from("/whatever"),
                }),
                None,
            )],
        };
        let err = execute(&plan).expect_err("destroying a real file must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("not a symlink"), "got: {msg}");
        assert!(path.exists(), "real file should still exist");
    }

    #[test]
    fn noop_change_does_nothing() {
        let d = TempDir::new("noop");
        let target = d.path.join("real");
        fs::write(&target, "hi").unwrap();
        let link = d.path.join("alias");
        symlink_impl(&target, &link).unwrap();

        let plan = Plan {
            changes: vec![change(
                Action::NoOp,
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: target.clone(),
                }),
                Some(ResourceState::Symlink {
                    from: link,
                    to: target,
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.added, 0);
        assert_eq!(summary.changed, 0);
        assert_eq!(summary.destroyed, 0);
    }

    #[test]
    fn summary_tallies_each_action_independently() {
        // Three actions in one plan — guards each counter against
        // mutation. Each is its own action so we can pin exactly which
        // tally arm fired without ordering ambiguity.
        let d = TempDir::new("mixed");
        let target = d.path.join("real");
        fs::write(&target, "hi").unwrap();
        let to_create = d.path.join("a");
        let to_update_link = d.path.join("b");
        let to_destroy = d.path.join("c");
        let old_target = d.path.join("old");
        fs::write(&old_target, "old").unwrap();
        symlink_impl(&old_target, &to_update_link).unwrap();
        symlink_impl(&target, &to_destroy).unwrap();

        let plan = Plan {
            changes: vec![
                change(
                    Action::Create,
                    None,
                    Some(ResourceState::Symlink {
                        from: to_create,
                        to: target.clone(),
                    }),
                ),
                change(
                    Action::Update,
                    Some(ResourceState::Symlink {
                        from: to_update_link.clone(),
                        to: old_target,
                    }),
                    Some(ResourceState::Symlink {
                        from: to_update_link,
                        to: target.clone(),
                    }),
                ),
                change(
                    Action::Destroy,
                    Some(ResourceState::Symlink {
                        from: to_destroy,
                        to: target,
                    }),
                    None,
                ),
            ],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.added, 1);
        assert_eq!(summary.changed, 1);
        assert_eq!(summary.destroyed, 1);
    }

    #[test]
    fn create_template_returns_not_implemented_error() {
        // Templates / directories / packages are still stubbed; the
        // executor must surface a clear "not yet implemented"
        // diagnostic mentioning the kind so users can see exactly
        // which resource type is blocking.
        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Template {
                    path: PathBuf::from("/x"),
                    content: "y".into(),
                }),
            )],
        };
        let err = execute(&plan).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not yet implemented"), "got: {msg}");
        assert!(msg.contains("template"), "should name the kind: {msg}");
    }

    #[test]
    fn create_directory_returns_not_implemented_error() {
        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Directory {
                    path: PathBuf::from("/d"),
                }),
            )],
        };
        let err = execute(&plan).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not yet implemented"));
        assert!(msg.contains("directory"));
    }

    #[test]
    fn create_package_dispatches_to_packages_install() {
        // Packages are now wired through `packages::install`. We
        // verify dispatch happens (i.e. we get past the executor's
        // "unsupported kind" fallthrough) by pointing the binary
        // override at `/bin/true` so the install "succeeds"
        // immediately without touching the real package manager.
        // SAFETY: edition 2024 env mutation; test serialises via
        // SEQ-based temp dir naming and restores PATH-style env on
        // exit.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", "/usr/bin/true");
        }
        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Package {
                    manager: PackageManager::Brew,
                    name: "ripgrep".into(),
                }),
            )],
        };
        let result = execute(&plan);
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
        }
        let summary = result.expect("install spy should succeed");
        assert_eq!(summary.added, 1);
    }

    #[test]
    fn empty_plan_executes_with_zero_summary() {
        let summary = execute(&Plan::default()).unwrap();
        assert_eq!(summary.added, 0);
        assert_eq!(summary.changed, 0);
        assert_eq!(summary.destroyed, 0);
    }

    #[test]
    fn update_aborts_when_path_changes_between_before_and_after() {
        // The planner pairs Update before/after on the same `from`.
        // A drifted plan with different `from` paths is a bug — bail
        // loudly so we never delete a path the user did not declare.
        let d = TempDir::new("update-drift");
        let plan = Plan {
            changes: vec![change(
                Action::Update,
                Some(ResourceState::Symlink {
                    from: d.path.join("a"),
                    to: d.path.join("t1"),
                }),
                Some(ResourceState::Symlink {
                    from: d.path.join("b"),
                    to: d.path.join("t2"),
                }),
            )],
        };
        let err = execute(&plan).unwrap_err();
        assert!(format!("{err:#}").contains("path mismatch"), "got: {err:#}");
    }
}
