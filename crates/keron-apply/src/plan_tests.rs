use super::*;

#[test]
fn summary_counts_each_action() {
    let mut plan = Plan::sample();
    plan.changes.push(ResourceChange {
        address: "refresh".into(),
        kind: ResourceKind::Shell,
        action: Action::Run,
        before: None,
        after: Some(ResourceState::Shell {
            kind: ShellKind::Sh,
            name: "refresh".into(),
            cwd: PathBuf::from("/tmp"),
            script: "echo ok".into(),
            sensitive: false,
        }),
        requires_elevation: false,
        requires_force: false,
    });
    let s = plan.summary();
    assert_eq!(s.add, 1);
    assert_eq!(s.change, 1);
    assert_eq!(s.run, 1);
}

#[test]
fn summary_counts_force_changes() {
    let mut plan = Plan::sample();
    plan.changes[1].requires_force = true;
    let s = plan.summary();
    assert_eq!(s.force, 1);
}

#[test]
fn is_empty_only_when_all_noop() {
    assert!(Plan::default().is_empty());
    let only_noop = Plan {
        changes: vec![ResourceChange {
            address: "x".into(),
            kind: ResourceKind::Template,
            action: Action::NoOp,
            before: None,
            after: None,
            requires_elevation: false,
            requires_force: false,
        }],
    };
    assert!(only_noop.is_empty());
    assert!(!Plan::sample().is_empty());
}

#[test]
fn address_for_template_uses_target() {
    let s = ResourceState::Template {
        path: PathBuf::from("/etc/x"),
        content: "y".into(),
        sensitive: false,
    };
    assert_eq!(address_for(&s), "/etc/x");
}

#[test]
fn address_for_symlink_uses_target() {
    let s = ResourceState::Symlink {
        from: PathBuf::from("/a"),
        to: PathBuf::from("/b"),
    };
    assert_eq!(address_for(&s), "/a");
}

#[test]
fn address_for_shell_uses_name() {
    let s = ResourceState::Shell {
        kind: ShellKind::Sh,
        name: "refresh-font-cache".into(),
        cwd: PathBuf::from("/tmp"),
        script: "echo ok".into(),
        sensitive: false,
    };
    assert_eq!(address_for(&s), "refresh-font-cache");
}

#[test]
fn shell_kind_parse_accepts_all_declared_variants() {
    assert_eq!(ShellKind::parse("sh").unwrap(), ShellKind::Sh);
    assert_eq!(ShellKind::parse("bash").unwrap(), ShellKind::Bash);
    assert_eq!(ShellKind::parse("zsh").unwrap(), ShellKind::Zsh);
    assert_eq!(ShellKind::parse("pwsh").unwrap(), ShellKind::Pwsh);
    assert_eq!(
        ShellKind::parse("powershell").unwrap(),
        ShellKind::Powershell
    );
}

#[test]
fn package_manager_support_matrix_matches_host_os_policy() {
    assert!(PackageManager::Brew.is_supported_on(OsFamily::Linux));
    assert!(PackageManager::Brew.is_supported_on(OsFamily::Macos));
    assert!(!PackageManager::Brew.is_supported_on(OsFamily::Windows));
    assert!(!PackageManager::Brew.is_supported_on(OsFamily::Unknown));

    assert!(PackageManager::Cargo.is_supported_on(OsFamily::Linux));
    assert!(PackageManager::Cargo.is_supported_on(OsFamily::Macos));
    assert!(PackageManager::Cargo.is_supported_on(OsFamily::Windows));
    assert!(PackageManager::Cargo.is_supported_on(OsFamily::Unknown));

    assert!(!PackageManager::Winget.is_supported_on(OsFamily::Linux));
    assert!(!PackageManager::Winget.is_supported_on(OsFamily::Macos));
    assert!(PackageManager::Winget.is_supported_on(OsFamily::Windows));
    assert!(!PackageManager::Winget.is_supported_on(OsFamily::Unknown));
}

#[test]
fn precheck_reports_unsupported_packages_and_keeps_supported_resources() {
    let resources = vec![
        ResourceState::Package {
            manager: PackageManager::Winget,
            name: "Microsoft.PowerShell".into(),
            tap: None,
        },
        ResourceState::Package {
            manager: PackageManager::Brew,
            name: "ripgrep".into(),
            tap: None,
        },
        ResourceState::Template {
            path: PathBuf::from("/tmp/out"),
            content: "x".into(),
            sensitive: false,
        },
    ];
    let precheck = precheck_resources(&resources, OsFamily::Linux);
    assert_eq!(precheck.unsupported_packages.len(), 1);
    let unsupported = &precheck.unsupported_packages[0];
    assert_eq!(unsupported.address, "winget:Microsoft.PowerShell");
    assert_eq!(unsupported.manager, PackageManager::Winget);
    assert_eq!(unsupported.name, "Microsoft.PowerShell");
    assert_eq!(unsupported.os, OsFamily::Linux);
    assert!(!include_in_plan(&resources[0], OsFamily::Linux));
    assert!(include_in_plan(&resources[1], OsFamily::Linux));
    assert!(include_in_plan(&resources[2], OsFamily::Linux));
}

// Manifest literals `/a` / `/b` are Unix-style absolute paths.
// On Windows they're rooted-but-not-absolute (no drive letter) and
// keron's path normalisation refuses them. Gate to unix.
#[cfg(unix)]
#[test]
fn build_plan_emits_one_change_per_resource() {
    use keron_modules::{EntrySource, ModuleId, resolve};
    use std::env;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("keron-build-plan-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("entry.keron");
    fs::write(dir.join("tmpl.tpl"), "{{ body }}").unwrap();
    let src = "reconcile template(source = \"tmpl.tpl\", target = \"/a\", vars = {\"body\": \"\"})\n\
                   reconcile template(source = \"tmpl.tpl\", target = \"/b\", vars = {\"body\": \"\"})\n";
    fs::write(&entry, src).unwrap();
    let canonical = fs::canonicalize(&entry).unwrap();
    let keron_root = canonical.parent().unwrap().to_path_buf();
    let graph = resolve(vec![EntrySource {
        text: src.into(),
        base_dir: canonical.parent().unwrap().to_path_buf(),
        id: ModuleId(canonical),
    }])
    .unwrap();
    let plan = build_plan(&graph, &keron_root).unwrap();
    assert_eq!(plan.changes.len(), 2);
    assert!(
        plan.changes
            .iter()
            .all(|c| matches!(c.action, Action::Create))
    );
    let addrs: Vec<&str> = plan.changes.iter().map(|c| c.address.as_str()).collect();
    assert_eq!(addrs, vec!["/a", "/b"]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dedup_resources_collapses_byte_identical_repeats() {
    // Two reconciles of the same template — common when a base
    // module declares it and a per-host overlay also references
    // it. Pre-fix this would crash apply mid-stream with EEXIST;
    // now both fold to a single Create.
    let t = ResourceState::Template {
        path: PathBuf::from("/a"),
        content: "x".into(),
        sensitive: false,
    };
    let deduped = dedup_resources(&[t.clone(), t.clone(), t]).unwrap();
    assert_eq!(deduped.len(), 1);
}

#[test]
fn dedup_resources_preserves_first_occurrence_order() {
    // Apply order matches declaration order: a duplicate later in
    // the stream must not bump its counterpart up the queue.
    let a = ResourceState::Template {
        path: PathBuf::from("/a"),
        content: "x".into(),
        sensitive: false,
    };
    let b = ResourceState::Symlink {
        from: PathBuf::from("/b"),
        to: PathBuf::from("/source"),
    };
    let deduped = dedup_resources(&[a.clone(), b.clone(), a, b]).unwrap();
    let addrs: Vec<String> = deduped.iter().map(address_for).collect();
    assert_eq!(addrs, vec!["/a", "/b"]);
}

#[test]
fn dedup_resources_errors_on_conflicting_template_at_same_path() {
    // Same target path, different rendered content — almost
    // certainly a mistake (forgot to update a partial in one of
    // the two callers, or pasted the wrong vars). Surface it as
    // a hard error at plan time rather than letting one declaration
    // silently win.
    let a = ResourceState::Template {
        path: PathBuf::from("/a"),
        content: "first".into(),
        sensitive: false,
    };
    let b = ResourceState::Template {
        path: PathBuf::from("/a"),
        content: "second".into(),
        sensitive: false,
    };
    let err = dedup_resources(&[a, b]).expect_err("conflicting state must error");
    let msg = format!("{err:#}");
    assert!(msg.contains("/a"), "error must name the address: {msg}");
    assert!(
        msg.contains("conflicting state"),
        "error must call the conflict out: {msg}",
    );
}

#[test]
fn dedup_resources_errors_on_conflicting_symlink_target() {
    // Same link path, different sources — a real-world bug where
    // two modules disagree on what `~/.zshrc` should point at.
    let a = ResourceState::Symlink {
        from: PathBuf::from("/link"),
        to: PathBuf::from("/source-a"),
    };
    let b = ResourceState::Symlink {
        from: PathBuf::from("/link"),
        to: PathBuf::from("/source-b"),
    };
    let err = dedup_resources(&[a, b]).expect_err("conflicting symlinks must error");
    assert!(format!("{err:#}").contains("/link"));
}

#[test]
fn build_prechecked_plan_dedups_repeated_template_into_one_change() {
    // Pin the dedup at the public entry point: writing
    // `reconcile t; reconcile t` for the same template now lands
    // in the plan as a single Create. Pre-fix this produced two
    // Create changes and apply crashed at the second with EEXIST.
    use keron_modules::{EntrySource, ModuleId, resolve};
    use std::env;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("keron-dedup-build-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("entry.keron");
    fs::write(dir.join("tmpl.tpl"), "{{ body }}").unwrap();
    let target = dir.join("dedup-target");
    // Forward slashes only in the manifest: on Windows `target.display()`
    // emits backslashes, and keron's string parser would treat them
    // as escape introducers (`\U`, `\d`...).
    let target_str = target.display().to_string().replace('\\', "/");
    let src = format!(
        "val t: Template = template(source = \"tmpl.tpl\", target = \"{target_str}\", vars = {{\"body\": \"x\"}})\n\
             reconcile t\n\
             reconcile t\n",
    );
    fs::write(&entry, &src).unwrap();
    let canonical = fs::canonicalize(&entry).unwrap();
    let keron_root = canonical.parent().unwrap().to_path_buf();
    let graph = resolve(vec![EntrySource {
        text: src,
        base_dir: canonical.parent().unwrap().to_path_buf(),
        id: ModuleId(canonical),
    }])
    .unwrap();
    let plan = build_plan(&graph, &keron_root).unwrap();
    assert_eq!(plan.changes.len(), 1, "duplicate reconciles must dedup");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn build_prechecked_plan_errors_on_conflicting_template_at_same_target() {
    // Two templates with the same target but different vars (and
    // therefore different rendered content) is a conflict, not a
    // dedup. The user must fix the manifest before any apply runs.
    use keron_modules::{EntrySource, ModuleId, resolve};
    use std::env;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("keron-conflict-build-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("entry.keron");
    fs::write(dir.join("tmpl.tpl"), "{{ body }}").unwrap();
    let target = dir.join("conflict-target");
    // Forward slashes only — see the dedup test above for the rationale.
    let path = target.display().to_string().replace('\\', "/");
    let src = format!(
        "reconcile template(source = \"tmpl.tpl\", target = \"{path}\", vars = {{\"body\": \"first\"}})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"{path}\", vars = {{\"body\": \"second\"}})\n",
    );
    fs::write(&entry, &src).unwrap();
    let canonical = fs::canonicalize(&entry).unwrap();
    let keron_root = canonical.parent().unwrap().to_path_buf();
    let graph = resolve(vec![EntrySource {
        text: src,
        base_dir: canonical.parent().unwrap().to_path_buf(),
        id: ModuleId(canonical),
    }])
    .unwrap();
    let err = build_plan(&graph, &keron_root).expect_err("conflict must surface");
    assert!(format!("{err:#}").contains("conflicting state"));
    let _ = fs::remove_dir_all(&dir);
}

// Manifest literal `/a` is a Unix-style absolute path. Gate.
#[cfg(unix)]
#[test]
fn build_prechecked_plan_skips_unsupported_packages_before_classification() {
    use keron_modules::{EntrySource, ModuleId, resolve};
    use std::env;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let _os = crate::platform::OsOverride::set(OsFamily::Linux);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("keron-build-precheck-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("entry.keron");
    fs::write(dir.join("tmpl.tpl"), "{{ body }}").unwrap();
    let src = "reconcile {\n\
                   winget(\"Microsoft.PowerShell\");\n\
                   template(source = \"tmpl.tpl\", target = \"/a\", vars = {\"body\": \"\"});\n\
                   }\n";
    fs::write(&entry, src).unwrap();
    let canonical = fs::canonicalize(&entry).unwrap();
    let keron_root = canonical.parent().unwrap().to_path_buf();
    let graph = resolve(vec![EntrySource {
        text: src.into(),
        base_dir: canonical.parent().unwrap().to_path_buf(),
        id: ModuleId(canonical),
    }])
    .unwrap();
    let prechecked = build_prechecked_plan(&graph, &keron_root).unwrap();
    assert_eq!(prechecked.precheck.unsupported_packages.len(), 1);
    assert_eq!(prechecked.plan.changes.len(), 1);
    assert_eq!(prechecked.plan.changes[0].address, "/a");
    let _ = fs::remove_dir_all(&dir);
}

/// Tier-1 prereq probe that counts invocations and reports brew
/// as missing. Used to assert that `build_prechecked_plan` fails
/// at the prereq gate *before* reaching the brew classify probes
/// — `pm_calls` proves the gate was consulted, and the surfaced
/// diagnostic proves it short-circuited rather than falling
/// through to `cache.prewarm`.
struct OrderingProbe {
    pm_calls: std::cell::Cell<usize>,
}

impl crate::capability::PrereqProbe for OrderingProbe {
    fn package_manager_available(&self, _pm: PackageManager) -> bool {
        self.pm_calls.set(self.pm_calls.get() + 1);
        false
    }
    fn session_state(
        &self,
        _kind: crate::capability::SessionKind,
    ) -> crate::capability::SessionState {
        crate::capability::SessionState::Active
    }
}

#[test]
fn build_prechecked_plan_runs_prereq_check_before_classify_probes() {
    use keron_modules::{EntrySource, ModuleId, resolve};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    // Force a platform where brew is supported so the package
    // doesn't get filtered out by `include_in_plan` before the
    // prereq pass would see it.
    let _os = crate::platform::OsOverride::set(OsFamily::Macos);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("keron-prereq-ordering-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("entry.keron");
    let src = "reconcile { brew(\"ripgrep\"); }\n";
    fs::write(&entry, src).unwrap();
    let canonical = fs::canonicalize(&entry).unwrap();
    let keron_root = canonical.parent().unwrap().to_path_buf();
    let graph = resolve(vec![EntrySource {
        text: src.into(),
        base_dir: canonical.parent().unwrap().to_path_buf(),
        id: ModuleId(canonical),
    }])
    .unwrap();
    let probe = OrderingProbe {
        pm_calls: std::cell::Cell::new(0),
    };
    let err = build_prechecked_plan_with_prereq_probe(&graph, &keron_root, &probe)
        .expect_err("missing brew should fail at the prereq gate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("`brew` is not installed"),
        "diagnostic should name the missing prereq: {msg}"
    );
    assert!(
        msg.contains("https://brew.sh"),
        "diagnostic should include the brew install URL: {msg}"
    );
    // Once-per-kind guarantee at the plan-builder boundary: even
    // though one package was declared, the probe fires exactly
    // once. Failing here would mean the gate ran per-resource
    // (wasteful) or — worse — that classify probes ran before
    // the gate fired (then `pm_calls` would still be 1 but a
    // brew shell-out would have happened).
    assert_eq!(probe.pm_calls.get(), 1);
    let _ = fs::remove_dir_all(&dir);
}

use std::env;
use std::sync::atomic::{AtomicUsize, Ordering};

static CLASSIFY_SEQ: AtomicUsize = AtomicUsize::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = CLASSIFY_SEQ.fetch_add(1, Ordering::Relaxed);
        let p = env::temp_dir().join(format!(
            "keron-classify-test-{tag}-{}-{n}",
            std::process::id()
        ));
        if p.exists() {
            fs::remove_dir_all(&p).ok();
        }
        fs::create_dir_all(&p).unwrap();
        // Canonicalise so the test path equals what macOS rewrites
        // (`/var/folders/...` → `/private/var/folders/...`) and
        // what `fs::canonicalize` would return for downstream
        // comparisons. The Windows `\\?\` prefix is fine here —
        // `classify_symlink` compares by filesystem identity, not
        // by path string.
        let path = fs::canonicalize(&p).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

fn desired(from: &Path, to: &Path) -> ResourceState {
    ResourceState::Symlink {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
    }
}

#[cfg(unix)]
fn write_executable(dir: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
}

#[cfg(unix)]
struct PathGuard {
    original: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
impl PathGuard {
    fn set(path: &Path) -> Self {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let lock = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let original = env::var_os("PATH");
        // SAFETY: this test guard serializes process-env mutation and restores on drop.
        #[allow(unsafe_code)]
        unsafe {
            env::set_var("PATH", path);
        }
        Self {
            original,
            _lock: lock,
        }
    }
}

#[cfg(unix)]
impl Drop for PathGuard {
    fn drop(&mut self) {
        // SAFETY: this test guard serializes process-env mutation and restores on drop.
        #[allow(unsafe_code)]
        unsafe {
            if let Some(original) = &self.original {
                env::set_var("PATH", original);
            } else {
                env::remove_var("PATH");
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn classify_shell_always_runs_when_shell_exists() {
    let d = TempDir::new("shell-present");
    write_executable(&d.path, "sh");
    let _path = PathGuard::set(&d.path);
    let state = ResourceState::Shell {
        kind: ShellKind::Sh,
        name: "refresh".into(),
        cwd: d.path.clone(),
        script: "echo ok".into(),
        sensitive: false,
    };
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.kind, ResourceKind::Shell);
    assert_eq!(change.action, Action::Run);
    assert!(!change.requires_elevation);
    assert!(!change.requires_force);
    assert!(change.before.is_none());
    assert_eq!(change.after, Some(state));
}

#[cfg(unix)]
#[test]
fn classify_shell_errors_when_shell_is_missing() {
    let d = TempDir::new("shell-missing");
    let _path = PathGuard::set(&d.path);
    let state = ResourceState::Shell {
        kind: ShellKind::Bash,
        name: "refresh".into(),
        cwd: d.path.clone(),
        script: "echo ok".into(),
        sensitive: false,
    };
    let err =
        classify(&state, &mut PackageCache::for_tests()).expect_err("missing bash should fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("shell `bash` is not available on PATH"));
}

#[test]
fn classify_symlink_marks_missing_path_as_create() {
    let d = TempDir::new("missing");
    let from = d.path.join("alias");
    let to = d.path.join("real");
    let state = desired(&from, &to);
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::Create);
    assert!(change.before.is_none());
    assert_eq!(change.after, Some(state));
}

#[test]
fn classify_symlink_marks_matching_target_as_noop() {
    let d = TempDir::new("noop");
    let to = d.path.join("real");
    fs::write(&to, "hi").unwrap();
    let from = d.path.join("alias");
    make_symlink(&to, &from).unwrap();

    let state = desired(&from, &to);
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::NoOp);
    let before = change.before.expect("before populated for noop");
    let ResourceState::Symlink { from: bf, to: bt } = before else {
        panic!("expected Symlink in before");
    };
    assert_eq!(bf, from);
    // Compare by filesystem identity rather than path string:
    // Windows' `fs::read_link` may normalise away the `\\?\`
    // prefix the test's `to` carries, but both PathBufs still
    // refer to the same file.
    assert!(
        same_file::is_same_file(&bt, &to).unwrap_or(false),
        "before.to ({}) must point to the same file as the test's to ({})",
        bt.display(),
        to.display(),
    );
}

#[test]
fn classify_symlink_marks_diverging_target_as_update() {
    let d = TempDir::new("update");
    let old_target = d.path.join("old");
    let new_target = d.path.join("new");
    fs::write(&old_target, "old").unwrap();
    fs::write(&new_target, "new").unwrap();
    let from = d.path.join("alias");
    make_symlink(&old_target, &from).unwrap();

    let state = desired(&from, &new_target);
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::Update);
    assert!(change.requires_force);
    let before = change.before.expect("before populated for update");
    let ResourceState::Symlink { to: bt, .. } = before else {
        panic!("expected Symlink before");
    };
    // Identity check rather than string equality — see the noop
    // test above for the Windows `\\?\` rationale.
    assert!(
        same_file::is_same_file(&bt, &old_target).unwrap_or(false),
        "before.to ({}) must point to the *current* (old) target ({})",
        bt.display(),
        old_target.display(),
    );
}

#[cfg(unix)]
#[test]
fn classify_symlink_dangling_link_is_update() {
    // Dangling link: canonicalize(from) returns ENOENT. The
    // ENOENT branch must classify as Update (not bail), so the
    // user can re-point the alias. Pins the
    // `NotFound`-guard match in classify_symlink against
    // mutations that flip it to false / != / always-true.
    let d = TempDir::new("dangling");
    let missing = d.path.join("not-here");
    let from = d.path.join("alias");
    make_symlink(&missing, &from).unwrap();
    // Sanity: target genuinely missing.
    assert!(
        !missing.exists(),
        "fixture invariant: target must be absent"
    );

    let new_target = d.path.join("new");
    fs::write(&new_target, "new").unwrap();
    let change = classify(&desired(&from, &new_target), &mut PackageCache::for_tests())
        .expect("dangling symlink must classify, not bail");
    assert_eq!(change.action, Action::Update);
    let before = change.before.expect("before populated for dangling update");
    let ResourceState::Symlink { to: bt, .. } = before else {
        panic!("expected Symlink before");
    };
    assert_eq!(
        bt, missing,
        "before should record the dangling target literally"
    );
}

#[cfg(unix)]
#[test]
fn classify_symlink_cyclic_link_is_update_not_error() {
    // a -> b, b -> a forms a 2-cycle. The literal comparison reads
    // `a`'s immediate target (`b`) without dereferencing, so a
    // cyclic link is simply re-pointed (Update) rather than blowing
    // up with ELOOP the way the old identity comparison did. The
    // declared target differs from `b`, so Update is correct.
    let d = TempDir::new("loop");
    let a = d.path.join("a");
    let b = d.path.join("b");
    make_symlink(&b, &a).unwrap();
    make_symlink(&a, &b).unwrap();

    let new_target = d.path.join("target");
    fs::write(&new_target, "x").unwrap();
    let change = classify(&desired(&a, &new_target), &mut PackageCache::for_tests())
        .expect("cyclic symlink must classify, not bail");
    assert_eq!(change.action, Action::Update);
    let before = change.before.expect("before populated");
    let ResourceState::Symlink { to: bt, .. } = before else {
        panic!("expected Symlink before");
    };
    assert_eq!(bt, b, "before records the immediate literal target");
}

#[cfg(unix)]
#[test]
fn classify_symlink_different_literal_same_inode_is_update() {
    // The regression from finding [19]: a link pointing at a
    // *different* path that resolves (via an intermediate link) to
    // the same file as the declared source must be Update — the
    // declared layout is not actually in place.
    let d = TempDir::new("indirect");
    let real = d.path.join("real");
    fs::write(&real, "x").unwrap();
    let deprecated = d.path.join("deprecated");
    make_symlink(&real, &deprecated).unwrap(); // deprecated -> real
    let alias = d.path.join("alias");
    make_symlink(&deprecated, &alias).unwrap(); // alias -> deprecated (-> real)

    // Declare alias should point literally at `real`. It currently
    // points at `deprecated` (same inode as real), but the literal
    // differs, so this must be Update, not NoOp.
    let change = classify(&desired(&alias, &real), &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::Update);
}

#[test]
fn classify_symlink_rejects_real_file_occupant() {
    let d = TempDir::new("clobber");
    let from = d.path.join("alias");
    fs::write(&from, "user data").unwrap();
    let to = d.path.join("target");

    let err = classify(&desired(&from, &to), &mut PackageCache::for_tests())
        .expect_err("real file must be refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("not a symlink"), "got: {msg}");
    assert!(msg.contains("refusing to overwrite"), "got: {msg}");
}

fn template(path: &Path, content: &str) -> ResourceState {
    ResourceState::Template {
        path: path.to_path_buf(),
        content: content.into(),
        sensitive: false,
    }
}

#[test]
fn classify_template_marks_missing_path_as_create() {
    let d = TempDir::new("template-missing");
    let path = d.path.join("config.toml");
    let state = template(&path, "x = 1\n");
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::Create);
    assert!(change.before.is_none());
    assert_eq!(change.after, Some(state));
}

#[test]
fn classify_template_marks_byte_identical_content_as_noop() {
    let d = TempDir::new("template-noop");
    let path = d.path.join("config.toml");
    fs::write(&path, "hello\n").unwrap();
    let state = template(&path, "hello\n");
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::NoOp);
    let before = change.before.expect("before populated for noop");
    let ResourceState::Template { content: bc, .. } = before else {
        panic!("expected Template before");
    };
    assert_eq!(bc, "hello\n");
}

#[cfg(unix)]
#[test]
fn classify_sensitive_template_with_world_readable_mode_is_update() {
    use std::os::unix::fs::PermissionsExt;
    // Content matches but the live sensitive file is 0644: the
    // secret is group/world-readable, so this must be Update (repair
    // the mode), not NoOp.
    let d = TempDir::new("template-sensitive-mode");
    let path = d.path.join("netrc");
    fs::write(&path, "secret\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let state = ResourceState::Template {
        path: path.clone(),
        content: "secret\n".into(),
        sensitive: true,
    };
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::Update);

    // After the file is already 0600, the same plan is NoOp.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::NoOp);
}

#[test]
fn classify_template_marks_diverging_content_as_update() {
    let d = TempDir::new("template-update");
    let path = d.path.join("config.toml");
    fs::write(&path, "old\n").unwrap();
    let state = template(&path, "new\n");
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::Update);
    assert!(change.requires_force);
    let before = change.before.expect("before populated for update");
    let ResourceState::Template { content: bc, .. } = before else {
        panic!("expected Template before");
    };
    assert_eq!(bc, "old\n", "before should record the *current* content");
}

#[test]
fn classify_template_tolerates_non_utf8_existing_file() {
    let d = TempDir::new("template-non-utf8");
    let path = d.path.join("binary");
    fs::write(&path, [0xFFu8, 0xFE, 0xFD]).unwrap();
    let state = template(&path, "ascii only\n");
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::Update);
    assert!(change.before.is_some());
}

#[test]
fn classify_template_rejects_symlink_occupant() {
    let d = TempDir::new("template-vs-symlink");
    let real = d.path.join("real");
    fs::write(&real, "x").unwrap();
    let path = d.path.join("alias");
    make_symlink(&real, &path).unwrap();
    let err = classify(&template(&path, "y"), &mut PackageCache::for_tests())
        .expect_err("symlink should not be treated as a template target");
    let msg = format!("{err:#}");
    assert!(msg.contains("not a regular file"), "got: {msg}");
    assert!(msg.contains("refusing to overwrite"), "got: {msg}");
}

fn ssh_key(
    private_path: &Path,
    public_path: &Path,
    private_key: &str,
    public_key: &str,
) -> ResourceState {
    ResourceState::SshKey {
        private_path: private_path.to_path_buf(),
        public_path: public_path.to_path_buf(),
        private_key: private_key.into(),
        public_key: public_key.into(),
    }
}

#[test]
fn classify_ssh_key_marks_both_missing_as_create() {
    let d = TempDir::new("ssh-missing");
    let priv_path = d.path.join("id_ed25519");
    let pub_path = d.path.join("id_ed25519.pub");
    let state = ssh_key(&priv_path, &pub_path, "PRIV", "ssh-ed25519 AAAA host");
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.kind, ResourceKind::SshKey);
    assert_eq!(change.action, Action::Create);
    assert!(change.before.is_none());
    assert!(!change.requires_elevation);
    assert!(!change.requires_force);
}

#[test]
fn classify_ssh_key_marks_matching_pair_as_noop() {
    let d = TempDir::new("ssh-noop");
    let priv_path = d.path.join("id_ed25519");
    let pub_path = d.path.join("id_ed25519.pub");
    fs::write(&priv_path, "PRIV").unwrap();
    fs::write(&pub_path, "ssh-ed25519 AAAA host").unwrap();
    let state = ssh_key(&priv_path, &pub_path, "PRIV", "ssh-ed25519 AAAA host");
    let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
    assert_eq!(change.action, Action::NoOp);
    assert!(change.before.is_some());
}

#[test]
fn classify_ssh_key_refuses_to_rotate_drifted_private() {
    // Private already exists with different bytes — we never
    // silently overwrite an existing key; user must remove it.
    let d = TempDir::new("ssh-drift");
    let priv_path = d.path.join("id_ed25519");
    let pub_path = d.path.join("id_ed25519.pub");
    fs::write(&priv_path, "OTHER").unwrap();
    fs::write(&pub_path, "ssh-ed25519 AAAA host").unwrap();
    let err = classify(
        &ssh_key(&priv_path, &pub_path, "PRIV", "ssh-ed25519 AAAA host"),
        &mut PackageCache::for_tests(),
    )
    .expect_err("drifted private must refuse rotation");
    let msg = format!("{err:#}");
    assert!(msg.contains("refusing to rotate ssh key"), "got: {msg}");
}

#[test]
fn classify_ssh_key_refuses_asymmetric_state() {
    // Private exists and matches, but public is missing — most
    // likely an interrupted prior apply. We bail rather than
    // silently writing the missing half.
    let d = TempDir::new("ssh-asymmetric");
    let priv_path = d.path.join("id_ed25519");
    let pub_path = d.path.join("id_ed25519.pub");
    fs::write(&priv_path, "PRIV").unwrap();
    let err = classify(
        &ssh_key(&priv_path, &pub_path, "PRIV", "ssh-ed25519 AAAA host"),
        &mut PackageCache::for_tests(),
    )
    .expect_err("missing pub half must refuse partial Create");
    let msg = format!("{err:#}");
    assert!(msg.contains("out of sync"), "got: {msg}");
}

#[cfg(unix)]
#[test]
fn classify_ssh_key_rejects_symlink_occupant() {
    // Same data-safety rule as classify_template: any non-regular
    // occupant (here: a symlink) is a hard error.
    let d = TempDir::new("ssh-vs-symlink");
    let real = d.path.join("real");
    fs::write(&real, "real").unwrap();
    let priv_path = d.path.join("id_ed25519");
    make_symlink(&real, &priv_path).unwrap();
    let pub_path = d.path.join("id_ed25519.pub");
    let err = classify(
        &ssh_key(&priv_path, &pub_path, "PRIV", "ssh-ed25519 AAAA host"),
        &mut PackageCache::for_tests(),
    )
    .expect_err("symlink at private path must be refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("not a regular file"), "got: {msg}");
    assert!(msg.contains("refusing to overwrite"), "got: {msg}");
}

#[test]
fn classify_ssh_key_address_uses_private_path() {
    let d = TempDir::new("ssh-address");
    let priv_path = d.path.join("id_ed25519");
    let pub_path = d.path.join("id_ed25519.pub");
    let state = ssh_key(&priv_path, &pub_path, "PRIV", "ssh-ed25519 AAAA host");
    assert_eq!(address_for(&state), priv_path.display().to_string());
}

#[test]
fn classify_gpg_key_address_uses_fingerprint_prefix() {
    let state = ResourceState::GpgKey {
        fingerprint: "ABCD1234".into(),
        key: "-----BEGIN PGP PRIVATE KEY BLOCK-----...".into(),
    };
    assert_eq!(address_for(&state), "gpg:ABCD1234");
}

fn gpg_state(fingerprint: &str) -> ResourceState {
    ResourceState::GpgKey {
        fingerprint: fingerprint.into(),
        key: "-----BEGIN PGP PRIVATE KEY BLOCK-----...".into(),
    }
}

#[test]
fn classify_gpg_key_marks_present_as_noop() {
    let state = gpg_state("ABCD1234");
    let change = classify_gpg_key(&state, GpgKeyringStatus::Present);
    assert_eq!(change.kind, ResourceKind::GpgKey);
    assert_eq!(change.action, Action::NoOp);
    assert!(
        change.before.is_some(),
        "NoOp must carry a before snapshot so the diff renders as unchanged"
    );
}

#[test]
fn classify_gpg_key_marks_absent_as_create() {
    let state = gpg_state("ABCD1234");
    let change = classify_gpg_key(&state, GpgKeyringStatus::Absent);
    assert_eq!(change.action, Action::Create);
    assert!(change.before.is_none());
}

#[test]
fn classify_gpg_key_marks_unavailable_as_create() {
    // gpg missing from PATH at plan time is no longer fatal. The
    // capability validator catches the truly-missing case earlier
    // (or confirms a Package will install gpg); here we just
    // assume the keyring will be empty when the executor runs.
    let state = gpg_state("ABCD1234");
    let change = classify_gpg_key(&state, GpgKeyringStatus::GpgUnavailable);
    assert_eq!(change.action, Action::Create);
    assert!(change.before.is_none());
}

fn pkg_with_tap(name: &str, user_tap: &str, url: Option<&str>) -> ResourceState {
    ResourceState::Package {
        manager: PackageManager::Brew,
        name: name.into(),
        tap: Some(TapSpec {
            user_tap: user_tap.into(),
            url: url.map(str::to_string),
        }),
    }
}

#[test]
fn synthesize_taps_collapses_same_url_into_one_entry() {
    // Two packages reference the same tap with the same URL. The
    // synthesizer must emit ONE Tap entry, not two. Pins the
    // `(Some(a), Some(b)) if a == b => {}` no-op arm: a mutation
    // that swaps the `==` for `!=` (or flips the guard to false)
    // would either duplicate the tap or bail on conflict.
    let url = "https://github.com/icepuma/keron";
    let resources = vec![
        pkg_with_tap("keron", "icepuma/keron", Some(url)),
        pkg_with_tap("kernel", "icepuma/keron", Some(url)),
    ];
    let out = synthesize_taps(&resources).expect("identical URLs must coalesce");
    let tap_count = out
        .iter()
        .filter(|r| matches!(r, ResourceState::Tap(_)))
        .count();
    assert_eq!(tap_count, 1, "duplicate same-url taps collapse to one");
}

#[test]
fn synthesize_taps_rejects_conflicting_urls_for_same_tap() {
    // Different URLs for the same tap is a manifest bug. Pins the
    // bail. Catches the mutation that swaps the `a == b` match
    // guard for `true`, which would silently accept whichever URL
    // landed first instead of erroring.
    let resources = vec![
        pkg_with_tap(
            "keron",
            "icepuma/keron",
            Some("https://github.com/icepuma/keron"),
        ),
        pkg_with_tap(
            "kernel",
            "icepuma/keron",
            Some("https://github.com/forked/keron"),
        ),
    ];
    let err = synthesize_taps(&resources).expect_err("conflicting URLs must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("conflicting URLs") && msg.contains("icepuma/keron"),
        "expected conflicting-URL bail, got: {msg}",
    );
}

#[test]
fn synthesize_taps_upgrades_bare_then_qualified_to_qualified() {
    // The bare declaration arrives first; the follow-up carries a
    // custom URL. The qualified declaration must win — pins the
    // `(None, Some(_))` arm so the same-url match guard can't be
    // smuggled into covering it.
    let url = "https://github.com/icepuma/keron";
    let resources = vec![
        pkg_with_tap("keron", "icepuma/keron", None),
        pkg_with_tap("kernel", "icepuma/keron", Some(url)),
    ];
    let out = synthesize_taps(&resources).expect("bare-then-qualified must merge");
    let Some(ResourceState::Tap(spec)) = out
        .iter()
        .find(|r| matches!(r, ResourceState::Tap(_)))
        .cloned()
    else {
        panic!("expected one Tap entry");
    };
    assert_eq!(spec.url.as_deref(), Some(url));
}
