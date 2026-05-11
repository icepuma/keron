//! Pre-check whether a [`ResourceState`] needs elevated rights to
//! apply. The result is purely informational for the rendered plan;
//! the actual apply still fails fast if permissions drift between
//! plan time and apply time.
//!
//! Detection is a try-write probe at the nearest existing ancestor
//! of the destination path. We don't use `libc::access(W_OK)` —
//! POSIX `access` consults the *real* UID and mis-reports ACL /
//! capability / MAC scenarios. The probe asks the kernel the same
//! question the actual write will, with one extra syscall.
//!
//! Cost: a `create_new` + `remove_file` per filesystem resource at
//! plan time. Acceptable: planning a manifest of N resources is
//! O(N), and N is small for any realistic dotfile flow.

use std::fs;
use std::io;
use std::path::Path;

use crate::plan::{Action, ResourceState};

/// Whether the executor will need elevated rights to apply `state`
/// with `action`. Packages flow through the per-manager policy on
/// [`crate::plan::PackageManager::requires_elevation`] — this
/// function speaks only for filesystem resources.
pub fn path_requires_elevation(state: &ResourceState, action: Action) -> bool {
    if matches!(action, Action::NoOp) {
        return false;
    }
    match state {
        // Symlink touches `from.parent()`: create makes a new entry,
        // update removes+re-creates, destroy unlinks. All three need
        // write on the parent. The `to` field is the link target;
        // we never write to it, so it's irrelevant for elevation.
        ResourceState::Symlink { from, .. } => from.parent().is_none_or(|p| !dir_is_writable(p)),
        // Template + Directory both touch their `path.parent()`.
        // For Update on a Template we also need to overwrite `path`
        // itself; if the parent is writable that's almost always
        // possible, and if it's not the probe catches it.
        ResourceState::Template { path, .. } | ResourceState::Directory { path } => {
            path.parent().is_none_or(|p| !dir_is_writable(p))
        }
        // Packages are routed through `PackageManager::requires_elevation`.
        // Reaching this arm means a caller mistakenly asked the
        // filesystem detector about a package; treat as "no" so the
        // package-side policy is authoritative.
        ResourceState::Package { .. } => false,
    }
}

/// Walk up from `start` until we find an existing directory, then
/// try to create-and-delete a small probe file inside it. Returns
/// `false` if the probe fails for any reason — conservative: when in
/// doubt, classify as elevated so the user sees a sudo prompt rather
/// than a mid-apply permission error.
fn dir_is_writable(start: &Path) -> bool {
    let Some(anchor) = nearest_existing_ancestor(start) else {
        return false;
    };
    let probe = anchor.join(format!(
        ".keron-elevation-probe-{}-{}",
        std::process::id(),
        // Monotonic-ish suffix so two probes on the same dir at the
        // same pid don't collide. Time-based is fine; the probe is
        // ephemeral.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => false,
        // ReadOnlyFilesystem, NotFound (race), etc. — conservative.
        Err(_) => false,
    }
}

fn nearest_existing_ancestor(start: &Path) -> Option<std::path::PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(p) = cur {
        if p.is_dir() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
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
                "keron-detect-test-{tag}-{}-{n}",
                std::process::id()
            ));
            if p.exists() {
                let _ = fs::remove_dir_all(&p);
            }
            fs::create_dir_all(&p).unwrap();
            Self {
                path: fs::canonicalize(p).unwrap(),
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn writable_dir_does_not_need_elevation() {
        let d = TempDir::new("writable");
        let state = ResourceState::Symlink {
            from: d.path.join("alias"),
            to: PathBuf::from("/tmp/target"),
        };
        assert!(!path_requires_elevation(&state, Action::Create));
    }

    #[test]
    fn noop_never_needs_elevation_even_for_protected_paths() {
        // NoOp short-circuits before the probe: nothing happens at
        // apply time, so elevation is moot.
        let state = ResourceState::Symlink {
            from: PathBuf::from("/etc/keron-noop"),
            to: PathBuf::from("/etc/some-target"),
        };
        assert!(!path_requires_elevation(&state, Action::NoOp));
    }

    #[test]
    fn package_resource_defers_to_manager_policy() {
        // Filesystem detector returns false for packages; the
        // per-manager policy on `PackageManager::requires_elevation`
        // is the authoritative source.
        let state = ResourceState::Package {
            manager: crate::plan::PackageManager::Brew,
            name: "ripgrep".into(),
        };
        assert!(!path_requires_elevation(&state, Action::Create));
    }

    #[test]
    fn template_uses_path_parent_for_probe() {
        let d = TempDir::new("template");
        let state = ResourceState::Template {
            path: d.path.join("a.conf"),
            content: "x".into(),
        };
        assert!(!path_requires_elevation(&state, Action::Create));
    }

    #[test]
    fn directory_walks_up_to_find_anchor() {
        // Resource path is several levels deep below an existing
        // ancestor; the probe must find the ancestor and decide
        // based on its writability.
        let d = TempDir::new("nested");
        let state = ResourceState::Directory {
            path: d.path.join("a").join("b").join("c"),
        };
        assert!(!path_requires_elevation(&state, Action::Create));
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_dir_needs_elevation() {
        // chmod 0500 removes write for owner and everyone — the
        // probe should report PermissionDenied. Skip on Windows
        // because the chmod-equivalent dance is ACL-heavy and
        // out-of-scope for v1 (we cover Windows protected paths via
        // manual e2e tests with `/etc`-equivalent locations).
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new("readonly");
        let mut perms = fs::metadata(&d.path).unwrap().permissions();
        perms.set_mode(0o500);
        fs::set_permissions(&d.path, perms.clone()).unwrap();
        let state = ResourceState::Symlink {
            from: d.path.join("alias"),
            to: PathBuf::from("/tmp/x"),
        };
        let result = path_requires_elevation(&state, Action::Create);
        // Restore so Drop can clean up.
        let mut perms = fs::metadata(&d.path).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&d.path, perms).unwrap();
        assert!(result, "0500 dir should need elevation");
    }

    #[test]
    fn nearest_existing_ancestor_walks_up_for_missing_path() {
        let d = TempDir::new("ancestor");
        let deep = d.path.join("a").join("b").join("c");
        let anchor = nearest_existing_ancestor(&deep).expect("must find some ancestor");
        assert!(anchor.is_dir(), "anchor must be an existing directory");
        // The anchor should be a prefix of `deep`.
        assert!(deep.starts_with(&anchor));
    }
}
