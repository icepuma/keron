//! Apply phase. Walks the [`Plan`] in order and performs the side
//! effects each [`ResourceChange`] demands.
//!
//! v1 supports symlinks, templates, and packages end-to-end (create,
//! update, no-op). Other resource kinds bail with a clear
//! "not yet implemented" diagnostic — they land alongside the
//! planner work that diffs them against live state.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::packages;
use crate::plan::{Action, Plan, ResourceChange, ResourceKind, ResourceState, ShellKind};

#[derive(Debug, Clone, Copy, Default)]
pub struct ExecuteSummary {
    pub added: usize,
    pub changed: usize,
    pub ran: usize,
}

pub fn execute(plan: &Plan) -> Result<ExecuteSummary> {
    let mut summary = ExecuteSummary::default();
    let mut applied_addresses: Vec<&str> = Vec::new();
    for change in &plan.changes {
        if let Err(e) = apply_change(change, &mut summary) {
            // Surface what already landed before the failure so the
            // user knows the post-mortem state of their tree rather
            // than seeing a single error with no context.
            return Err(annotate_partial_apply(
                e,
                change.address.as_str(),
                &applied_addresses,
                plan.changes.len(),
            ));
        }
        applied_addresses.push(change.address.as_str());
    }
    Ok(summary)
}

fn annotate_partial_apply(
    err: anyhow::Error,
    failed_address: &str,
    applied: &[&str],
    total: usize,
) -> anyhow::Error {
    if applied.is_empty() {
        return err.context(format!(
            "apply failed at resource 1 of {total} (`{failed_address}`); nothing was applied"
        ));
    }
    let mut summary = format!(
        "apply failed at resource {} of {total} (`{failed_address}`); {} resource(s) already applied:",
        applied.len() + 1,
        applied.len(),
    );
    for addr in applied {
        summary.push_str("\n  - ");
        summary.push_str(addr);
    }
    err.context(summary)
}

fn apply_change(change: &ResourceChange, summary: &mut ExecuteSummary) -> Result<()> {
    apply_change_one(change)?;
    match change.action {
        Action::Create => summary.added += 1,
        Action::Update => summary.changed += 1,
        Action::Run => summary.ran += 1,
        Action::NoOp => {}
    }
    Ok(())
}

/// Privilege context in which a change is being applied.
///
/// The elevated child routes Create actions through `openat`-based
/// component walks so a concurrent symlink swap in an intermediate
/// directory can't redirect a root-owned write. Unprivileged
/// execution uses the simpler `fs::create_dir_all` + leaf-write
/// dance because the user can't escalate via their own writes.
#[derive(Debug, Clone, Copy)]
pub enum ApplyContext {
    /// Run by the user themselves.
    Unprivileged,
    /// Run by the elevated child (sudo / `ShellExecuteExW`).
    Elevated,
}

/// Apply a single change. Shared between the in-process executor and
/// the elevated child entry point so the two stay in lockstep.
///
/// # Errors
/// Errors when the underlying filesystem call fails or when the
/// resource kind has no executor support yet for the action.
pub fn apply_change_one(change: &ResourceChange) -> Result<()> {
    apply_change_one_in(change, ApplyContext::Unprivileged)
}

/// Apply a single change with an explicit privilege context. The
/// elevated child should always pass [`ApplyContext::Elevated`] so
/// Create actions route through the TOCTOU-safe `openat` walk
/// (`elevated::safe_write`).
///
/// # Errors
/// See [`apply_change_one`].
pub fn apply_change_one_in(change: &ResourceChange, ctx: ApplyContext) -> Result<()> {
    match change.action {
        Action::NoOp => Ok(()),
        Action::Create => {
            let state = change
                .after
                .as_ref()
                .with_context(|| format!("create `{}` has no desired state", change.address))?;
            apply_create(state, ctx).with_context(|| format!("creating `{}`", change.address))
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
            // Update reuses the existing fs::* path on both Unix and
            // Windows. The `before` state proves the leaf already
            // existed at plan time; the residual TOCTOU window (stat
            // ↔ rename, or stat ↔ remove+create) is narrower than
            // Create's mkdir-then-symlink race and is documented in
            // the elevated/child.rs ancestor pre-check.
            apply_update(before, after).with_context(|| format!("updating `{}`", change.address))
        }
        Action::Run => {
            let state = change
                .after
                .as_ref()
                .with_context(|| format!("run `{}` has no desired state", change.address))?;
            apply_run(state).with_context(|| format!("running `{}`", change.address))
        }
    }
}

fn apply_create(state: &ResourceState, ctx: ApplyContext) -> Result<()> {
    match state {
        ResourceState::Symlink { from, to } => create_symlink(from, to, ctx),
        ResourceState::Template {
            path,
            content,
            sensitive,
        } => create_template(path, content, *sensitive, ctx),
        ResourceState::Package { manager, name } => packages::install(*manager, name),
        ResourceState::Shell { .. } => bail!(unsupported_kind(state)),
    }
}

fn apply_run(state: &ResourceState) -> Result<()> {
    let ResourceState::Shell {
        kind,
        name,
        cwd,
        script,
        ..
    } = state
    else {
        bail!(unsupported_kind(state));
    };
    run_shell(*kind, name, cwd, script)
}

fn apply_update(before: &ResourceState, after: &ResourceState) -> Result<()> {
    match (before, after) {
        (
            ResourceState::Symlink { from: bt, .. },
            ResourceState::Symlink {
                from: at,
                to: source,
            },
        ) => {
            // Planner guarantees matched target on both sides; bail
            // loudly if that invariant ever drifts.
            if bt != at {
                bail!(
                    "symlink update target mismatch: `{}` vs `{}`",
                    bt.display(),
                    at.display(),
                );
            }
            remove_symlink(bt)?;
            create_symlink(at, source, ApplyContext::Unprivileged)
        }
        (
            ResourceState::Template { path: bp, .. },
            ResourceState::Template {
                path: ap,
                content,
                sensitive,
            },
        ) => {
            if bp != ap {
                bail!(
                    "template update target mismatch: `{}` vs `{}`",
                    bp.display(),
                    ap.display(),
                );
            }
            replace_template(ap, content, *sensitive)
        }
        _ => bail!(unsupported_kind(after)),
    }
}

fn create_symlink(target: &Path, source: &Path, ctx: ApplyContext) -> Result<()> {
    #[cfg(unix)]
    if matches!(ctx, ApplyContext::Elevated)
        && let Some(parent_path) = target.parent()
        && let Some(leaf) = target.file_name()
    {
        // Walk each ancestor with O_NOFOLLOW; symlinkat onto the
        // resulting parent fd. Closes the TOCTOU window between
        // `mkdir_all(parent)` and `symlink(2)` that the elevated
        // child would otherwise be racing.
        let parent = crate::elevated::safe_write::ParentDir::open(parent_path)
            .with_context(|| format!("opening elevated parent of `{}`", target.display()))?;
        return crate::elevated::safe_write::symlink_at(&parent, leaf, source).with_context(|| {
            format!(
                "creating symlink target `{}` from source `{}` (elevated)",
                target.display(),
                source.display()
            )
        });
    }
    let _ = ctx;
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory `{}`", parent.display()))?;
    }
    symlink_impl(source, target).with_context(|| {
        format!(
            "creating symlink target `{}` from source `{}`",
            target.display(),
            source.display()
        )
    })
}

fn remove_symlink(path: &Path) -> Result<()> {
    // `symlink_metadata` does not traverse the link, so a broken
    // symlink (target missing) still reports `is_symlink()`.
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("inspecting `{}`", path.display()))?;
    if !meta.file_type().is_symlink() {
        bail!("`{}` is not a symlink; refusing to remove", path.display());
    }
    fs::remove_file(path).with_context(|| format!("removing symlink `{}`", path.display()))
}

fn create_template(path: &Path, content: &str, sensitive: bool, ctx: ApplyContext) -> Result<()> {
    let mode = create_mode(sensitive);
    #[cfg(unix)]
    if matches!(ctx, ApplyContext::Elevated)
        && let Some(parent_path) = path.parent()
        && let Some(leaf) = path.file_name()
    {
        let parent = crate::elevated::safe_write::ParentDir::open(parent_path)
            .with_context(|| format!("opening elevated parent of `{}`", path.display()))?;
        let mut file = crate::elevated::safe_write::create_file_at(&parent, leaf, mode)
            .with_context(|| format!("creating template `{}` (elevated)", path.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("writing template `{}`", path.display()))?;
        return file
            .sync_all()
            .with_context(|| format!("syncing template `{}`", path.display()));
    }
    let _ = ctx;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory `{}`", parent.display()))?;
    }
    let mut file = open_new_leaf_no_follow(path, mode)
        .with_context(|| format!("creating template `{}`", path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("writing template `{}`", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing template `{}`", path.display()))
}

fn replace_template(path: &Path, content: &str, sensitive: bool) -> Result<()> {
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("inspecting `{}`", path.display()))?;
    if !meta.file_type().is_file() {
        bail!(
            "`{}` is not a regular file; refusing to replace",
            path.display()
        );
    }
    let tmp = temp_sibling(path);
    let mode = replace_mode(sensitive, &meta);
    let guard = TmpFileGuard::new(tmp.clone());
    let mut file = open_new_leaf_no_follow(&tmp, mode)
        .with_context(|| format!("creating temporary template `{}`", tmp.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("writing temporary template `{}`", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing temporary template `{}`", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "atomically replacing `{}` with `{}`",
            path.display(),
            tmp.display()
        )
    })?;
    guard.disarm();
    Ok(())
}

/// Removes `path` on drop unless [`Self::disarm`] is called first.
/// Used by [`replace_template`] so every failure path between the
/// `open(.tmp)` and the final `rename` cleans up the sibling temp
/// — including panic unwinding, where the previous
/// `let _ = fs::remove_file(&tmp)` would never have run.
struct TmpFileGuard {
    path: std::path::PathBuf,
    disarmed: bool,
}

impl TmpFileGuard {
    const fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            disarmed: false,
        }
    }

    fn disarm(mut self) {
        self.disarmed = true;
    }
}

impl Drop for TmpFileGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Permission bits to use when creating a fresh template file.
/// Sensitive content (rendered with `unwrap_secret(...)` anywhere
/// in `vars`) lands at `0o600` so a secret-bearing render of e.g.
/// `~/.netrc` doesn't briefly exist world-readable. Non-sensitive
/// templates keep the standard `0o644`-after-umask behavior.
#[cfg_attr(not(unix), allow(clippy::needless_pass_by_value))]
const fn create_mode(sensitive: bool) -> u32 {
    if sensitive { 0o600 } else { 0o644 }
}

/// Mode for the replacement tempfile. Preserves the existing file's
/// permissions so `chmod 600 ~/.ssh/config` survives an idempotent
/// update; if sensitive, additionally clamps group/other bits off.
#[cfg(unix)]
fn replace_mode(sensitive: bool, existing: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    let existing_mode = existing.mode() & 0o777;
    if sensitive {
        existing_mode & 0o700
    } else {
        existing_mode
    }
}

// `#[mutants::skip]` because this branch only compiles on non-unix
// hosts (Windows ignores the mode argument when opening files), and
// the CI mutant runner is unix-only — no test on a unix host can
// execute it.
#[cfg(not(unix))]
#[cfg_attr(test, mutants::skip)]
const fn replace_mode(_sensitive: bool, _existing: &fs::Metadata) -> u32 {
    0
}

fn temp_sibling(path: &Path) -> std::path::PathBuf {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map_or_else(|| "template".into(), std::ffi::OsStr::to_os_string);
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(
        ".keron-tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    parent.join(tmp_name)
}

fn open_new_leaf_no_follow(path: &Path, mode: u32) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode).custom_flags(libc::O_NOFOLLOW);
    }

    #[cfg(not(unix))]
    {
        let _ = mode;
    }

    options.open(path)
}

fn unsupported_kind(state: &ResourceState) -> String {
    let kind = match state {
        ResourceState::Symlink { .. } => ResourceKind::Symlink,
        ResourceState::Template { .. } => ResourceKind::Template,
        ResourceState::Package { .. } => ResourceKind::Package,
        ResourceState::Shell { .. } => ResourceKind::Shell,
    };
    format!(
        "executor not yet implemented for {} resources",
        kind.label()
    )
}

fn run_shell(kind: ShellKind, name: &str, cwd: &Path, script: &str) -> Result<()> {
    let shell_path = which::which(kind.label())
        .with_context(|| format!("shell `{}` is not available on PATH", kind.label()))?;
    let mut command = Command::new(shell_path);
    command.current_dir(cwd);
    match kind {
        ShellKind::Sh | ShellKind::Bash | ShellKind::Zsh => {
            command.arg("-s");
        }
        ShellKind::Pwsh | ShellKind::Powershell => {
            command.args(["-NoProfile", "-NonInteractive", "-Command", "-"]);
        }
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning shell resource `{name}`"))?;
    let mut stdin = child
        .stdin
        .take()
        .with_context(|| format!("opening stdin for shell resource `{name}`"))?;
    stdin
        .write_all(script.as_bytes())
        .with_context(|| format!("writing script for shell resource `{name}`"))?;
    drop(stdin);
    let status = child
        .wait()
        .with_context(|| format!("waiting for shell resource `{name}`"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("shell resource `{name}` exited with {status}")
    }
}

#[cfg(unix)]
fn symlink_impl(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_impl(target: &Path, link: &Path) -> io::Result<()> {
    // Windows splits file vs directory symlinks at the API level; pick
    // the right call so dotfile flows that link whole config dirs
    // (`~/.config/nvim` -> `<repo>/nvim`) work without ceremony.
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PackageManager, ResourceKind, ShellKind};
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

    struct CwdFile {
        path: PathBuf,
    }

    impl CwdFile {
        fn new(tag: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = PathBuf::from(format!(".keron-execute-{tag}-{}-{n}", std::process::id()));
            if path.exists() {
                fs::remove_file(&path).ok();
            }
            Self { path }
        }
    }

    impl Drop for CwdFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
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
                ResourceState::Template { path, .. } => path.display().to_string(),
                ResourceState::Package { manager, name } => format!("{}:{}", manager.label(), name),
                ResourceState::Shell { name, .. } => name.clone(),
            },
            kind: match probe {
                ResourceState::Symlink { .. } => ResourceKind::Symlink,
                ResourceState::Template { .. } => ResourceKind::Template,
                ResourceState::Package { .. } => ResourceKind::Package,
                ResourceState::Shell { .. } => ResourceKind::Shell,
            },
            action,
            before,
            after,
            requires_elevation: false,
            requires_force: false,
        }
    }

    #[cfg(unix)]
    fn write_noop_binary(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("noop.sh");
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(windows)]
    fn write_noop_binary(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("noop.bat");
        fs::write(&path, "@echo off\r\nexit /b 0\r\n").unwrap();
        path
    }

    #[cfg(unix)]
    fn write_fake_shell(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("sh");
        fs::write(
            &path,
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > \"$KERON_TEST_SHELL_ARGS\"\n\
             pwd > \"$KERON_TEST_SHELL_CWD\"\n\
             /bin/cat > \"$KERON_TEST_SHELL_STDIN\"\n\
             echo shell-stdout\n\
             echo shell-stderr >&2\n\
             exit \"$KERON_TEST_SHELL_EXIT\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }

    #[cfg(unix)]
    struct ShellEnvGuard {
        original_path: Option<std::ffi::OsString>,
        original_args: Option<std::ffi::OsString>,
        original_cwd: Option<std::ffi::OsString>,
        original_stdin: Option<std::ffi::OsString>,
        original_exit: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(unix)]
    impl ShellEnvGuard {
        fn set(
            path: &std::path::Path,
            args: &std::path::Path,
            cwd: &std::path::Path,
            stdin: &std::path::Path,
            exit: Option<&str>,
        ) -> Self {
            static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
            let lock = LOCK
                .get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap();
            let guard = Self {
                original_path: env::var_os("PATH"),
                original_args: env::var_os("KERON_TEST_SHELL_ARGS"),
                original_cwd: env::var_os("KERON_TEST_SHELL_CWD"),
                original_stdin: env::var_os("KERON_TEST_SHELL_STDIN"),
                original_exit: env::var_os("KERON_TEST_SHELL_EXIT"),
                _lock: lock,
            };
            // SAFETY: this test guard serializes process-env mutation and restores on drop.
            #[allow(unsafe_code)]
            unsafe {
                env::set_var("PATH", path);
                env::set_var("KERON_TEST_SHELL_ARGS", args);
                env::set_var("KERON_TEST_SHELL_CWD", cwd);
                env::set_var("KERON_TEST_SHELL_STDIN", stdin);
                env::set_var("KERON_TEST_SHELL_EXIT", exit.unwrap_or("0"));
            }
            guard
        }
    }

    #[cfg(unix)]
    impl Drop for ShellEnvGuard {
        fn drop(&mut self) {
            restore_env("PATH", self.original_path.as_ref());
            restore_env("KERON_TEST_SHELL_ARGS", self.original_args.as_ref());
            restore_env("KERON_TEST_SHELL_CWD", self.original_cwd.as_ref());
            restore_env("KERON_TEST_SHELL_STDIN", self.original_stdin.as_ref());
            restore_env("KERON_TEST_SHELL_EXIT", self.original_exit.as_ref());
        }
    }

    #[cfg(unix)]
    fn restore_env(key: &str, value: Option<&std::ffi::OsString>) {
        // SAFETY: this test guard serializes process-env mutation and restores on drop.
        #[allow(unsafe_code)]
        unsafe {
            if let Some(value) = value {
                env::set_var(key, value);
            } else {
                env::remove_var(key);
            }
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
        let resolved = fs::read_link(&link).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn create_symlink_creates_missing_parent_directories() {
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
    }

    #[test]
    fn summary_tallies_each_action_independently() {
        let d = TempDir::new("mixed");
        let target = d.path.join("real");
        fs::write(&target, "hi").unwrap();
        let to_create = d.path.join("a");
        let to_update_link = d.path.join("b");
        let old_target = d.path.join("old");
        fs::write(&old_target, "old").unwrap();
        symlink_impl(&old_target, &to_update_link).unwrap();

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
                        to: target,
                    }),
                ),
            ],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.added, 1);
        assert_eq!(summary.changed, 1);
    }

    #[test]
    fn create_template_writes_file_with_content() {
        let d = TempDir::new("template-create");
        let path = d.path.join("nested").join("config.toml");
        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Template {
                    path: path.clone(),
                    content: "key = \"value\"\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.added, 1);
        let written = fs::read_to_string(&path).expect("file written");
        assert_eq!(written, "key = \"value\"\n");
    }

    #[test]
    fn update_template_overwrites_content() {
        let d = TempDir::new("template-update");
        let path = d.path.join("config.toml");
        fs::write(&path, "old contents\n").unwrap();
        let plan = Plan {
            changes: vec![change(
                Action::Update,
                Some(ResourceState::Template {
                    path: path.clone(),
                    content: "old contents\n".into(),
                    sensitive: false,
                }),
                Some(ResourceState::Template {
                    path: path.clone(),
                    content: "new contents\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.changed, 1);
        let written = fs::read_to_string(&path).expect("file written");
        assert_eq!(written, "new contents\n");
    }

    #[test]
    fn update_template_handles_relative_leaf_paths() {
        let file = CwdFile::new("relative-template-update");
        fs::write(&file.path, "old").unwrap();
        replace_template(&file.path, "new", false).unwrap();
        assert_eq!(fs::read_to_string(&file.path).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn create_template_sensitive_writes_mode_0600() {
        use std::os::unix::fs::MetadataExt;
        let d = TempDir::new("sensitive-create");
        let path = d.path.join("creds");
        create_template(&path, "TOKEN=hunter2\n", true, ApplyContext::Unprivileged).unwrap();
        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "sensitive template must be owner-only: {mode:o}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn replace_template_preserves_existing_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let d = TempDir::new("preserve-mode");
        let path = d.path.join("config");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        replace_template(&path, "new", false).unwrap();
        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600, "existing mode should be preserved: {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn replace_template_sensitive_clamps_group_other_bits() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let d = TempDir::new("clamp-mode");
        let path = d.path.join("creds");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        replace_template(&path, "new", true).unwrap();
        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "sensitive replace must drop group/other bits: {mode:o}"
        );
    }

    #[test]
    fn temp_sibling_for_relative_leaf_uses_current_dir_parent() {
        let tmp = temp_sibling(Path::new("config.toml"));
        assert_eq!(tmp.parent(), Some(Path::new(".")));
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(".config.toml.keron-tmp-"), "got: {tmp:?}");
    }

    #[test]
    fn open_new_leaf_creates_the_requested_path() {
        let d = TempDir::new("open-new-leaf");
        let path = d.path.join("leaf");
        let mut file = open_new_leaf_no_follow(&path, 0o644).unwrap();
        file.write_all(b"x").unwrap();
        drop(file);
        assert_eq!(fs::read_to_string(path).unwrap(), "x");
    }

    #[cfg(unix)]
    #[test]
    fn open_new_leaf_refuses_symlink_leaf() {
        let d = TempDir::new("open-new-leaf-symlink");
        let real = d.path.join("real");
        fs::write(&real, "original").unwrap();
        let link = d.path.join("link");
        symlink_impl(&real, &link).unwrap();
        let err = open_new_leaf_no_follow(&link, 0o644).expect_err("symlink leaf must not open");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(real).unwrap(), "original");
    }

    #[test]
    fn create_package_dispatches_to_packages_install() {
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Macos);
        let d = TempDir::new("package-noop");
        let noop = write_noop_binary(&d.path);
        // SAFETY: edition 2024 env mutation; test serialises via
        // SEQ-based temp dir naming and restores PATH-style env on
        // exit.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
            std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", noop);
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
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
        }
        let summary = result.expect("install spy should succeed");
        assert_eq!(summary.added, 1);
    }

    #[cfg(unix)]
    #[test]
    fn run_shell_writes_script_to_stdin_and_uses_cwd() {
        let d = TempDir::new("shell-run");
        write_fake_shell(&d.path);
        let args = d.path.join("args");
        let cwd_file = d.path.join("cwd");
        let stdin_file = d.path.join("stdin");
        let _env = ShellEnvGuard::set(&d.path, &args, &cwd_file, &stdin_file, None);
        let plan = Plan {
            changes: vec![change(
                Action::Run,
                None,
                Some(ResourceState::Shell {
                    kind: ShellKind::Sh,
                    name: "refresh".into(),
                    cwd: d.path.clone(),
                    script: "echo one\necho two\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.ran, 1);
        assert_eq!(fs::read_to_string(args).unwrap(), "-s\n");
        assert_eq!(
            fs::read_to_string(cwd_file).unwrap().trim(),
            fs::canonicalize(&d.path).unwrap().display().to_string()
        );
        assert_eq!(
            fs::read_to_string(stdin_file).unwrap(),
            "echo one\necho two\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_shell_nonzero_exit_fails_with_context() {
        let d = TempDir::new("shell-nonzero");
        write_fake_shell(&d.path);
        let _env = ShellEnvGuard::set(
            &d.path,
            &d.path.join("args"),
            &d.path.join("cwd"),
            &d.path.join("stdin"),
            Some("7"),
        );
        let plan = Plan {
            changes: vec![change(
                Action::Run,
                None,
                Some(ResourceState::Shell {
                    kind: ShellKind::Sh,
                    name: "fail".into(),
                    cwd: d.path.clone(),
                    script: "exit 7\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let err = execute(&plan).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("running `fail`"), "got: {msg}");
        assert!(msg.contains("exited"), "got: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn run_shell_rechecks_missing_shell_before_spawn() {
        let d = TempDir::new("shell-missing");
        let _env = ShellEnvGuard::set(
            &d.path,
            &d.path.join("args"),
            &d.path.join("cwd"),
            &d.path.join("stdin"),
            None,
        );
        let plan = Plan {
            changes: vec![change(
                Action::Run,
                None,
                Some(ResourceState::Shell {
                    kind: ShellKind::Bash,
                    name: "missing".into(),
                    cwd: d.path.clone(),
                    script: "echo ok\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let err = execute(&plan).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("shell `bash` is not available on PATH"));
    }

    #[test]
    fn create_shell_action_reports_shell_executor_mismatch() {
        let d = TempDir::new("shell-create-mismatch");
        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Shell {
                    kind: ShellKind::Sh,
                    name: "refresh".into(),
                    cwd: d.path.clone(),
                    script: "echo ok\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let err = execute(&plan).expect_err("shell create action must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("executor not yet implemented for shell resources"),
            "got: {msg}"
        );
    }

    #[test]
    fn tmp_file_guard_removes_file_on_drop_when_armed() {
        // Pins `Drop::drop with ()` mutation: if drop is a no-op,
        // an interrupted `replace_template` would leak its sibling
        // tempfile. Also pins `delete ! in drop`: with the inversion,
        // an armed guard would skip removal.
        let d = TempDir::new("guard-armed");
        let path = d.path.join("leaked-tmp");
        fs::write(&path, "scratch").unwrap();
        assert!(path.exists(), "fixture invariant");
        {
            let _g = TmpFileGuard::new(path.clone());
        } // drop here
        assert!(
            !path.exists(),
            "armed guard's drop must delete the tempfile: {path:?}"
        );
    }

    #[test]
    fn tmp_file_guard_disarm_prevents_removal_on_drop() {
        // Pins `TmpFileGuard::disarm with ()`: if disarm fails to set
        // the flag, the drop path still fires and silently removes
        // the file the caller just renamed into place — losing the
        // template content. Also pins `delete ! in drop`: with the
        // inversion, a disarmed guard would still remove the file.
        let d = TempDir::new("guard-disarmed");
        let path = d.path.join("survives");
        fs::write(&path, "kept").unwrap();
        {
            let g = TmpFileGuard::new(path.clone());
            g.disarm();
        }
        assert!(
            path.exists(),
            "disarmed guard must NOT delete the file: {path:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "kept");
    }

    #[test]
    fn empty_plan_executes_with_zero_summary() {
        let summary = execute(&Plan::default()).unwrap();
        assert_eq!(summary.added, 0);
        assert_eq!(summary.changed, 0);
    }

    #[test]
    fn update_aborts_when_path_changes_between_before_and_after() {
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
        assert!(
            format!("{err:#}").contains("target mismatch"),
            "got: {err:#}"
        );
    }
}
