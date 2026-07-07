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
            ResourceState::Package { manager, name, .. } => {
                format!("{}:{}", manager.kind_label(), name)
            }
            ResourceState::Tap(spec) => format!("tap:{}", spec.user_tap),
            ResourceState::Shell { name, .. } => name.clone(),
            ResourceState::SshKey { private_path, .. } => private_path.display().to_string(),
            ResourceState::GpgKey { fingerprint, .. } => format!("gpg:{fingerprint}"),
        },
        kind: match probe {
            ResourceState::Symlink { .. } => ResourceKind::Symlink,
            ResourceState::Template { .. } => ResourceKind::Template,
            ResourceState::Package { .. } => ResourceKind::Package,
            ResourceState::Tap(_) => ResourceKind::Tap,
            ResourceState::Shell { .. } => ResourceKind::Shell,
            ResourceState::SshKey { .. } => ResourceKind::SshKey,
            ResourceState::GpgKey { .. } => ResourceKind::GpgKey,
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
fn update_symlink_bails_when_live_state_changed_since_plan() {
    let d = TempDir::new("reverify");
    let actual_target = d.path.join("actual");
    let plan_thought = d.path.join("plan-thought");
    let new_target = d.path.join("new");
    fs::write(&actual_target, "a").unwrap();
    fs::write(&new_target, "n").unwrap();
    let link = d.path.join("alias");
    symlink_impl(&actual_target, &link).unwrap();

    // The plan's `before` says the link pointed at `plan_thought`,
    // but it actually points at `actual_target` — it changed since
    // the diff the user approved. The update must refuse and leave
    // the link untouched.
    let before = ResourceState::Symlink {
        from: link.clone(),
        to: plan_thought,
    };
    let after = ResourceState::Symlink {
        from: link.clone(),
        to: new_target,
    };
    let err = apply_update(&before, &after, ApplyContext::Unprivileged, &mut Vec::new())
        .expect_err("changed live state must bail");
    assert!(
        format!("{err:#}").contains("changed since the plan"),
        "got: {err:#}"
    );
    assert_eq!(fs::read_link(&link).unwrap(), actual_target);
}

#[test]
fn update_template_bails_when_live_content_changed_since_plan() {
    // The user approved replacing content "old"; another process
    // rewrote the file to "surprise" before apply. The update must
    // refuse and leave the live content untouched, mirroring the
    // symlink re-verify.
    let d = TempDir::new("reverify-template");
    let path = d.path.join("app.conf");
    fs::write(&path, "surprise").unwrap();
    let before = ResourceState::Template {
        path: path.clone(),
        content: "old".into(),
        sensitive: false,
    };
    let after = ResourceState::Template {
        path: path.clone(),
        content: "new".into(),
        sensitive: false,
    };
    let err = apply_update(&before, &after, ApplyContext::Unprivileged, &mut Vec::new())
        .expect_err("changed live content must bail");
    assert!(
        format!("{err:#}").contains("changed since the plan"),
        "got: {err:#}"
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "surprise",
        "live content must be left untouched"
    );
}

#[test]
fn update_template_writes_when_live_content_matches_plan() {
    let d = TempDir::new("reverify-template-ok");
    let path = d.path.join("app.conf");
    fs::write(&path, "old").unwrap();
    let before = ResourceState::Template {
        path: path.clone(),
        content: "old".into(),
        sensitive: false,
    };
    let after = ResourceState::Template {
        path: path.clone(),
        content: "new".into(),
        sensitive: false,
    };
    apply_update(&before, &after, ApplyContext::Unprivileged, &mut Vec::new())
        .expect("matching before-content must apply");
    assert_eq!(fs::read_to_string(&path).unwrap(), "new");
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
    replace_template(&file.path, "new", false, ApplyContext::Unprivileged).unwrap();
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
    replace_template(&path, "new", false, ApplyContext::Unprivileged).unwrap();
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
    replace_template(&path, "new", true, ApplyContext::Unprivileged).unwrap();
    let mode = fs::metadata(&path).unwrap().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "sensitive replace must drop group/other bits: {mode:o}"
    );
}

#[cfg(unix)]
#[test]
fn replace_template_mode_survives_restrictive_umask() {
    use rustix::fs::Mode;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let d = TempDir::new("umask-mode");
    let path = d.path.join("shared");
    fs::write(&path, "old").unwrap();
    // A deliberately group-readable file (e.g. shared with a daemon).
    fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
    // A restrictive umask would clamp `open(2)`'s mode; the fchmod
    // must restore the exact preserved bits. nextest runs each test
    // in its own process, so this umask change is isolated.
    let prev = rustix::process::umask(Mode::from_bits_truncate(0o077));
    let result = replace_template(&path, "new", false, ApplyContext::Unprivileged);
    rustix::process::umask(prev);
    result.unwrap();
    let mode = fs::metadata(&path).unwrap().mode() & 0o777;
    assert_eq!(
        mode, 0o664,
        "umask must not clamp the preserved mode: {mode:o}"
    );
}

#[cfg(unix)]
#[test]
fn create_ssh_key_writes_private_at_0600_and_public_at_0644() {
    use std::os::unix::fs::MetadataExt;
    let d = TempDir::new("ssh-create");
    let priv_path = d.path.join("id_ed25519");
    let pub_path = d.path.join("id_ed25519.pub");
    create_ssh_key(
        &priv_path,
        &pub_path,
        "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n",
        "ssh-ed25519 AAAA host\n",
    )
    .unwrap();
    let priv_mode = fs::metadata(&priv_path).unwrap().mode() & 0o777;
    let pub_mode = fs::metadata(&pub_path).unwrap().mode() & 0o777;
    assert_eq!(
        priv_mode, 0o600,
        "private key must be owner-only: {priv_mode:o}"
    );
    assert_eq!(pub_mode, 0o644, "public key mode: {pub_mode:o}");
    // Content survives byte-for-byte.
    assert!(fs::read_to_string(&priv_path).unwrap().contains("OPENSSH"));
    assert_eq!(
        fs::read_to_string(&pub_path).unwrap(),
        "ssh-ed25519 AAAA host\n"
    );
}

#[cfg(unix)]
#[test]
fn create_ssh_key_creates_parent_dir_at_0700() {
    use std::os::unix::fs::MetadataExt;
    let d = TempDir::new("ssh-parent");
    let ssh_dir = d.path.join(".ssh");
    let priv_path = ssh_dir.join("id_ed25519");
    let pub_path = ssh_dir.join("id_ed25519.pub");
    assert!(!ssh_dir.exists(), "fixture invariant: parent absent");
    create_ssh_key(&priv_path, &pub_path, "PRIV", "ssh-ed25519 AAAA").unwrap();
    let mode = fs::metadata(&ssh_dir).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o700, "parent dir must be 0700: {mode:o}");
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

fn pkg_change(manager: PackageManager, name: &str, action: Action) -> ResourceChange {
    let state = ResourceState::Package {
        manager,
        name: name.into(),
        tap: None,
    };
    change(action, Some(state.clone()), Some(state))
}

#[test]
fn group_package_changes_buckets_by_manager_in_stable_order() {
    // Plan declaration order is intentionally mixed to pin that
    // group_package_changes sorts by manager_order — the same key
    // the parallel flush relies on.
    let phase = vec![
        pkg_change(PackageManager::Cargo, "sccache", Action::Create),
        pkg_change(PackageManager::Brew, "fd", Action::Create),
        pkg_change(PackageManager::BrewCask, "alacritty", Action::Create),
        pkg_change(PackageManager::Brew, "ripgrep", Action::Create),
    ];
    let groups = group_package_changes(&phase).unwrap();
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].manager, PackageManager::Brew);
    assert_eq!(groups[0].action, Action::Create);
    assert_eq!(groups[0].names, vec!["fd", "ripgrep"]);
    assert_eq!(groups[1].manager, PackageManager::BrewCask);
    assert_eq!(groups[1].names, vec!["alacritty"]);
    assert_eq!(groups[2].manager, PackageManager::Cargo);
    assert_eq!(groups[2].names, vec!["sccache"]);
}

#[test]
fn group_package_changes_rejects_update_on_package() {
    // The classifier never returns Update for packages, so if one
    // appears here it indicates a planner bug — surface it loudly
    // instead of silently routing into a no-longer-existing
    // upgrade path.
    let phase = vec![pkg_change(PackageManager::Brew, "ripgrep", Action::Update)];
    let err = group_package_changes(&phase).expect_err("Update on Package must error");
    assert!(
        format!("{err:#}").contains("only ensures presence"),
        "got: {err:#}",
    );
}

#[test]
fn group_package_changes_drops_noops() {
    let phase = vec![
        pkg_change(PackageManager::Brew, "ripgrep", Action::Create),
        pkg_change(PackageManager::Brew, "fd", Action::NoOp),
        pkg_change(PackageManager::Cargo, "sccache", Action::NoOp),
    ];
    let groups = group_package_changes(&phase).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].manager, PackageManager::Brew);
    assert_eq!(groups[0].names, vec!["ripgrep"]);
}

#[test]
fn flush_phase_outputs_writes_in_stable_manager_order() {
    // Outcomes intentionally provided out of stable order to pin
    // that flush prints brew before cargo regardless of the input
    // sequence. (The real flow gets stable order from
    // group_package_changes' BTreeMap, but the flush function
    // itself should still write in the order it received — pinning
    // that contract here keeps the two pieces honest.)
    let cargo_group = BatchGroup {
        manager: PackageManager::Cargo,
        action: Action::Create,
        names: vec!["sccache".into()],
    };
    let brew_group = BatchGroup {
        manager: PackageManager::Brew,
        action: Action::Create,
        names: vec!["ripgrep".into(), "fd".into()],
    };
    let outcomes = vec![
        GroupOutcome {
            group: brew_group,
            result: Ok(BatchOutput {
                stdout: b"brew-out\n".to_vec(),
                stderr: Vec::new(),
            }),
        },
        GroupOutcome {
            group: cargo_group,
            result: Ok(BatchOutput {
                stdout: b"cargo-out\n".to_vec(),
                stderr: Vec::new(),
            }),
        },
    ];
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    flush_phase_outputs_to(&outcomes, &mut out, &mut err);
    let rendered = String::from_utf8(out).unwrap();
    let brew_idx = rendered.find("brew install").expect("brew header present");
    let cargo_idx = rendered
        .find("cargo install")
        .expect("cargo header present");
    assert!(
        brew_idx < cargo_idx,
        "brew block must precede cargo block; got: {rendered:?}",
    );
    let brew_payload_idx = rendered.find("brew-out").expect("brew payload present");
    let cargo_payload_idx = rendered.find("cargo-out").expect("cargo payload present");
    assert!(brew_payload_idx < cargo_payload_idx);
    assert!(brew_idx < brew_payload_idx);
    assert!(cargo_idx < cargo_payload_idx);
}

#[test]
fn flush_phase_outputs_marks_failed_batches_in_the_banner() {
    let group = BatchGroup {
        manager: PackageManager::Brew,
        action: Action::Create,
        names: vec!["does-not-exist".into()],
    };
    let outcomes = vec![GroupOutcome {
        group,
        result: Err(anyhow::anyhow!("spy refused")),
    }];
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    flush_phase_outputs_to(&outcomes, &mut out, &mut err);
    let rendered = String::from_utf8(out).unwrap();
    assert!(
        rendered.contains("[FAILED]"),
        "failed batch banner must be marked: {rendered:?}",
    );
}

#[cfg(unix)]
fn write_argv_recording_spy(dir: &std::path::Path, log: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("argv-spy.sh");
    // Each invocation appends one line to the log: "<binary>\t<arg1>,<arg2>,…"
    // — arg ',' is impossible in our argv (we validate package names), so the
    // simple comma split in the assertion is unambiguous. Newline-separated
    // entries are atomic appends ≤ PIPE_BUF on every relevant platform, so
    // parallel writes from multiple workers don't interleave.
    let script = "#!/bin/sh\n\
                      log=\"$KERON_TEST_ARGV_LOG\"\n\
                      printf '%s\\t' \"$0\" >> \"$log\"\n\
                      first=1\n\
                      for a in \"$@\"; do\n\
                      \tif [ \"$first\" -eq 1 ]; then\n\
                      \t\tprintf '%s' \"$a\" >> \"$log\"\n\
                      \t\tfirst=0\n\
                      \telse\n\
                      \t\tprintf ',%s' \"$a\" >> \"$log\"\n\
                      \tfi\n\
                      done\n\
                      printf '\\n' >> \"$log\"\n\
                      exit 0\n";
    fs::write(&path, script).unwrap();
    let mut perm = fs::metadata(&path).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&path, perm).unwrap();
    let _ = log;
    path
}

#[test]
fn bump_summary_create_increments_added_by_count() {
    // bump_summary is the package-phase counter; a function that
    // sees N installed names should bump `added` by N. Mutations on
    // line 411 swap `+=` for `*=` (collapses to 0 when starting
    // from 0) or `-=` (panics on usize underflow). Pin the
    // arithmetic shape directly so neither mutation passes silently.
    let mut s = ExecuteSummary {
        added: 2,
        changed: 0,
        ran: 0,
        warnings: Vec::new(),
    };
    bump_summary(&mut s, Action::Create, 3);
    assert_eq!(s.added, 5, "Create must add count to existing total");
    assert_eq!(s.changed, 0);
    assert_eq!(s.ran, 0);
}

#[test]
fn bump_summary_update_increments_changed_by_count() {
    // Companion of the Create case for line 412. Even though
    // `keron apply` never emits an Update for a Package today, the
    // arm is part of the public bump_summary contract for future
    // managers and the Tap-URL drift path.
    let mut s = ExecuteSummary {
        added: 0,
        changed: 4,
        ran: 0,
        warnings: Vec::new(),
    };
    bump_summary(&mut s, Action::Update, 2);
    assert_eq!(s.added, 0);
    assert_eq!(s.changed, 6, "Update must add count to existing total");
    assert_eq!(s.ran, 0);
}

#[test]
fn apply_update_package_bails_with_planner_bug_diagnostic() {
    // The (Package, Package) Update match arm exists to surface a
    // planner bug — Package classify never emits Update. Pin the
    // diagnostic so a `delete match arm` mutation that falls
    // through to the generic `unsupported_kind` bail can't sneak
    // past the test suite.
    let before = ResourceState::Package {
        manager: PackageManager::Brew,
        name: "ripgrep".into(),
        tap: None,
    };
    let after = ResourceState::Package {
        manager: PackageManager::Brew,
        name: "ripgrep".into(),
        tap: None,
    };
    let err = apply_update(&before, &after, ApplyContext::Unprivileged, &mut Vec::new())
        .expect_err("Package Update must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("reached the Update path") && msg.contains("ripgrep"),
        "expected planner-bug diagnostic naming the package: {msg}",
    );
}

#[cfg(unix)]
#[test]
fn apply_update_tap_invokes_brew_tap_with_custom_remote() {
    // The (Tap, Tap) Update match arm re-taps via brew so a remote
    // URL drift is repaired without a re-clone. Verify the arm by
    // pointing the brew binary override at a recording spy and
    // asserting the spy captured the `--custom-remote` flag. A
    // `delete match arm` mutation would fall through to the
    // unsupported_kind bail and the spy would never see the call.
    use crate::plan::TapSpec;
    use std::os::unix::fs::PermissionsExt;
    let _g = crate::packages::lock_env();
    let d = TempDir::new("tap-update");
    let log = d.path.join("argv.log");
    let spy = d.path.join("brew-spy.sh");
    let script = "#!/bin/sh\necho \"$@\" >> \"$KERON_TEST_ARGV_LOG\"\nexit 0\n";
    fs::write(&spy, script).unwrap();
    let mut perm = fs::metadata(&spy).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&spy, perm).unwrap();
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
        std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", &spy);
        std::env::set_var("KERON_TEST_ARGV_LOG", &log);
    }
    let before = ResourceState::Tap(TapSpec {
        user_tap: "icepuma/keron".into(),
        url: Some("https://github.com/old/url".into()),
    });
    let after = ResourceState::Tap(TapSpec {
        user_tap: "icepuma/keron".into(),
        url: Some("https://github.com/icepuma/keron".into()),
    });
    let result = apply_update(&before, &after, ApplyContext::Unprivileged, &mut Vec::new());
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
        std::env::remove_var("KERON_TEST_ARGV_LOG");
        std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
    }
    result.expect("tap update must succeed against the spy");
    let recorded = fs::read_to_string(&log).expect("spy ran");
    assert!(
        recorded.contains("tap")
            && recorded.contains("icepuma/keron")
            && recorded.contains("--custom-remote"),
        "spy must capture a re-tap with --custom-remote: {recorded:?}",
    );
}

#[test]
fn validate_gpg_fingerprint_accepts_hex_rejects_injection() {
    // Canonical forms pass.
    validate_gpg_fingerprint("ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234").unwrap();
    validate_gpg_fingerprint("0xDEADBEEF").unwrap();
    validate_gpg_fingerprint("ABCD 1234 ABCD 1234").unwrap();
    // A leading dash (flag injection into `gpg --list-secret-keys`)
    // and any other non-hex are rejected.
    for bad in ["--version", "-rf", "abc; rm", "zzzz", ""] {
        let err = validate_gpg_fingerprint(bad).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a valid hex fingerprint"),
            "fingerprint {bad:?} should be rejected, got: {err:#}"
        );
    }
}

#[test]
fn check_gpg_import_status_passes_only_on_success() {
    // Pins the `!status.success()` gate on the `gpg --import` call.
    // A mutation that deletes the `!` would bail on success and
    // accept failure — the user would see a spurious import error
    // even though gpg ran cleanly.
    check_gpg_import_status(true, "exit code: 0", "ABCD1234").expect("success must pass");
    let err =
        check_gpg_import_status(false, "exit code: 2", "ABCD1234").expect_err("failure must bail");
    let msg = format!("{err:#}");
    assert!(msg.contains("exit code: 2"), "got: {msg}");
    assert!(msg.contains("ABCD1234"), "got: {msg}");
}

#[test]
fn check_gpg_probe_status_passes_only_on_success() {
    // Pins the `!probe.success()` gate on the post-import
    // fingerprint re-probe. A mutation that deletes the `!` would
    // accept a missing fingerprint as confirmation, letting the
    // apply loop import-on-every-run silently.
    check_gpg_probe_status(true, "ABCD1234").expect("success must pass");
    let err = check_gpg_probe_status(false, "ABCD1234")
        .expect_err("failure must bail with wrong-fingerprint diagnostic");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("different fingerprint") && msg.contains("ABCD1234"),
        "expected fingerprint-mismatch diagnostic, got: {msg}",
    );
}

#[cfg(unix)]
#[test]
fn execute_collapses_multiple_brew_packages_into_one_subprocess() {
    let _g = crate::packages::lock_env();
    let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Macos);
    let d = TempDir::new("batch-brew");
    let log = d.path.join("argv.log");
    let spy = write_argv_recording_spy(&d.path, &log);
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
        std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", &spy);
        std::env::set_var("KERON_TEST_ARGV_LOG", &log);
    }
    let plan = Plan {
        changes: vec![
            pkg_change(PackageManager::Brew, "ripgrep", Action::Create),
            pkg_change(PackageManager::Brew, "bat", Action::Create),
            pkg_change(PackageManager::Brew, "fd", Action::Create),
        ],
    };
    let result = execute(&plan);
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
        std::env::remove_var("KERON_TEST_ARGV_LOG");
        std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
    }
    let summary = result.expect("spy should accept all batched packages");
    assert_eq!(summary.added, 3, "all three packages count toward added");
    let recorded = fs::read_to_string(&log).expect("log written");
    let lines: Vec<&str> = recorded.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "three brew packages must collapse into one subprocess; got: {recorded:?}",
    );
    let line = lines[0];
    let (_binary, args_str) = line.split_once('\t').expect("tab-separated row");
    let args: Vec<&str> = args_str.split(',').collect();
    assert_eq!(args, vec!["install", "ripgrep", "bat", "fd"]);
}

#[cfg(unix)]
#[test]
fn execute_runs_distinct_manager_batches_concurrently() {
    // Plan: 2 brew Create + 1 cargo Create + 1 cask Create, all in
    // one Package phase → 3 batches (brew/cask share the brew bin
    // but spawn separate subprocesses).
    //
    // We prove parallelism *structurally*, not by stopwatch. Each
    // spy writes a `start <pid>` line to the shared log, sleeps,
    // then writes an `end <pid> <argv>` line. POSIX `O_APPEND` on
    // a regular file makes each short write atomic and ordered, so
    // the file's line order is the wall-clock interleaving of the
    // children. If all three batches truly ran concurrently, every
    // `start` line lands before any `end` line — there is no
    // numerical threshold to tune and the test is impervious to
    // host load or busy CI.
    use std::os::unix::fs::PermissionsExt;
    let _g = crate::packages::lock_env();
    let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Macos);
    let d = TempDir::new("batch-parallel");
    let log = d.path.join("argv.log");
    let spy_path = d.path.join("overlap-spy.sh");
    // POSIX `O_APPEND` only guarantees atomicity for a single
    // `write()` syscall, so the spy assembles each log line in a
    // shell variable first and emits it with one `printf`. Each
    // line is well under PIPE_BUF (512 B on macOS), so concurrent
    // appends from the three children stay non-interleaved.
    let script = "#!/bin/sh\n\
                      log=\"$KERON_TEST_ARGV_LOG\"\n\
                      printf 'start %s\\n' \"$$\" >> \"$log\"\n\
                      sleep 0.2\n\
                      args=\"\"\n\
                      first=1\n\
                      for a in \"$@\"; do\n\
                      \tif [ \"$first\" -eq 1 ]; then\n\
                      \t\targs=\"$a\"; first=0\n\
                      \telse\n\
                      \t\targs=\"$args,$a\"\n\
                      \tfi\n\
                      done\n\
                      printf 'end %s\\t%s\\t%s\\n' \"$$\" \"$0\" \"$args\" >> \"$log\"\n\
                      exit 0\n";
    fs::write(&spy_path, script).unwrap();
    let mut perm = fs::metadata(&spy_path).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&spy_path, perm).unwrap();
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
        std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", &spy_path);
        std::env::set_var("KERON_TEST_PACKAGE_BIN_CARGO", &spy_path);
        std::env::set_var("KERON_TEST_ARGV_LOG", &log);
    }
    let plan = Plan {
        changes: vec![
            pkg_change(PackageManager::Brew, "ripgrep", Action::Create),
            pkg_change(PackageManager::Brew, "fd", Action::Create),
            pkg_change(PackageManager::Cargo, "sccache", Action::Create),
            pkg_change(PackageManager::BrewCask, "alacritty", Action::Create),
        ],
    };
    let result = execute(&plan);
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
        std::env::remove_var("KERON_TEST_PACKAGE_BIN_CARGO");
        std::env::remove_var("KERON_TEST_ARGV_LOG");
        std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
    }
    let summary = result.expect("all spies should succeed");
    assert_eq!(summary.added, 4);

    let recorded = fs::read_to_string(&log).expect("log written");
    let lines: Vec<&str> = recorded.lines().collect();
    let starts = lines.iter().filter(|l| l.starts_with("start ")).count();
    let ends = lines.iter().filter(|l| l.starts_with("end ")).count();
    assert_eq!(
        (starts, ends),
        (3, 3),
        "expected three start/end pairs (one per manager batch); got: {recorded:?}",
    );
    // All `start` lines must precede every `end` line — that is
    // the structural proof of overlap. If any batch had run
    // sequentially after another, an `end` would appear before a
    // later `start`.
    let first_end = lines
        .iter()
        .position(|l| l.starts_with("end "))
        .expect("at least one end line");
    let starts_before_any_end = lines[..first_end]
        .iter()
        .filter(|l| l.starts_with("start "))
        .count();
    assert_eq!(
        starts_before_any_end, 3,
        "all three batches must start before any of them finishes; got:\n{recorded}",
    );
}

#[cfg(unix)]
#[test]
fn execute_failed_batch_in_parallel_phase_reports_succeeded_and_failed() {
    use std::os::unix::fs::PermissionsExt;
    let _g = crate::packages::lock_env();
    let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Macos);
    let d = TempDir::new("batch-mixed");
    let ok_spy = d.path.join("ok.sh");
    fs::write(&ok_spy, "#!/bin/sh\necho cargo-ok\nexit 0\n").unwrap();
    let mut perm = fs::metadata(&ok_spy).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&ok_spy, perm).unwrap();
    let fail_spy = d.path.join("fail.sh");
    fs::write(&fail_spy, "#!/bin/sh\necho >&2 spy-refused\nexit 7\n").unwrap();
    let mut perm = fs::metadata(&fail_spy).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&fail_spy, perm).unwrap();
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
        std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", &fail_spy);
        std::env::set_var("KERON_TEST_PACKAGE_BIN_CARGO", &ok_spy);
    }
    let plan = Plan {
        changes: vec![
            pkg_change(PackageManager::Brew, "broken", Action::Create),
            pkg_change(PackageManager::Cargo, "sccache", Action::Create),
        ],
    };
    let result = execute(&plan);
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
        std::env::remove_var("KERON_TEST_PACKAGE_BIN_CARGO");
        std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
    }
    let err = result.expect_err("failed batch must surface as an error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("brew install (1 package: broken)"),
        "error must name the failing batch; got: {msg}",
    );
    assert!(
        msg.contains("cargo install (1 package: sccache)"),
        "error must list the sibling success; got: {msg}",
    );
    assert!(
        msg.contains("batch(es) succeeded"),
        "error must call out the succeeded batch; got: {msg}",
    );
}

#[test]
fn create_package_dispatches_to_packages_install() {
    // Share the process-wide env lock with packages::tests so a
    // concurrent test there doesn't clobber `KERON_ALLOW_TEST_OVERRIDES`
    // mid-classify.
    let _g = crate::packages::lock_env();
    let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Macos);
    let d = TempDir::new("package-noop");
    let noop = write_noop_binary(&d.path);
    // SAFETY: edition 2024 env mutation; the ENV_LOCK guard above
    // serialises against the packages::tests env mutators.
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
                tap: None,
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
