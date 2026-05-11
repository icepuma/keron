//! `Plan` — the diffable, renderable description of what `apply` will
//! do. [`build_plan`] runs the evaluator over a checked module graph
//! and classifies each produced resource into a [`ResourceChange`].
//!
//! Symlinks are diffed against the live filesystem so the rendered
//! plan reflects what `keron apply --execute` will actually perform;
//! other resource kinds still flow through as `Action::Create` until
//! their executor support lands.

#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::elevated;
use crate::eval;
use crate::packages::PackageCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Create,
    Update,
    Destroy,
    NoOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKind {
    Template,
    Directory,
    Symlink,
    Package,
}

impl ResourceKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Package => "package",
        }
    }
}

/// Which package manager owns a given [`ResourceState::Package`].
/// Carried as a discriminator on the unified `Package` resource so
/// the executor picks the right CLI at apply time; the user-facing
/// type system sees one `Package` shape regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PackageManager {
    Brew,
    Cargo,
    Winget,
}

impl PackageManager {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Brew => "brew",
            Self::Cargo => "cargo",
            Self::Winget => "winget",
        }
    }

    /// Whether this manager's install/uninstall flow needs to run as
    /// root / admin. Filesystem-side elevation is decided separately
    /// in [`elevated::detect`]; this method only speaks for the
    /// package-manager subprocess.
    ///
    /// v1 returns `false` for every variant:
    ///   - `Brew` — refuses to run under sudo by design.
    ///   - `Cargo` — installs to `~/.cargo/bin`, per-user.
    ///   - `Winget` — brokers its own UAC at install time for
    ///     machine-scope packages.
    ///
    /// The hook stays so future managers (apt, dnf, pacman) can flip
    /// their arm to `true` without rewiring callers.
    pub const fn requires_elevation(self) -> bool {
        match self {
            Self::Brew | Self::Cargo | Self::Winget => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceState {
    Template {
        path: PathBuf,
        content: String,
    },
    Directory {
        path: PathBuf,
    },
    Symlink {
        from: PathBuf,
        to: PathBuf,
    },
    Package {
        manager: PackageManager,
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceChange {
    pub address: String,
    pub kind: ResourceKind,
    pub action: Action,
    pub before: Option<ResourceState>,
    pub after: Option<ResourceState>,
    /// Pre-computed at plan time so the diff renderer and the
    /// elevation partition step don't have to re-probe the filesystem.
    /// Populated from [`elevated::detect::path_requires_elevation`]
    /// for filesystem resources and from
    /// [`PackageManager::requires_elevation`] for packages.
    #[serde(default)]
    pub requires_elevation: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    pub changes: Vec<ResourceChange>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlanSummary {
    pub add: usize,
    pub change: usize,
    pub destroy: usize,
    /// How many of the above are flagged as requiring elevated rights.
    /// Sub-total of `add + change + destroy`, surfaced in the diff
    /// summary as `(N elevated)`.
    pub elevated: usize,
}

impl Plan {
    pub fn summary(&self) -> PlanSummary {
        let mut s = PlanSummary::default();
        for c in &self.changes {
            match c.action {
                Action::Create => s.add += 1,
                Action::Update => s.change += 1,
                Action::Destroy => s.destroy += 1,
                Action::NoOp => continue,
            }
            if c.requires_elevation {
                s.elevated += 1;
            }
        }
        s
    }

    pub fn is_empty(&self) -> bool {
        self.changes
            .iter()
            .all(|c| matches!(c.action, Action::NoOp))
    }

    /// Split into `(unprivileged, elevated)` plans. Source order is
    /// preserved within each subset — `Vec::partition` is stable.
    /// `NoOp` changes never need elevation, so they always land in the
    /// unprivileged half regardless of their `requires_elevation`
    /// flag.
    #[must_use]
    pub fn partition_by_elevation(self) -> (Self, Self) {
        let (elev, plain): (Vec<_>, Vec<_>) = self
            .changes
            .into_iter()
            .partition(|c| c.requires_elevation && !matches!(c.action, Action::NoOp));
        (Self { changes: plain }, Self { changes: elev })
    }
}

impl ResourceChange {
    /// Whether this change must run under elevated rights to succeed.
    /// Computed at plan time from the filesystem writability probe
    /// (filesystem resources) or the package-manager policy
    /// (packages). The cached field is what the diff renderer and the
    /// partition step actually consult; this method is the
    /// computation used at plan time.
    pub fn compute_requires_elevation(&self) -> bool {
        let state = self.after.as_ref().or(self.before.as_ref());
        match state {
            Some(ResourceState::Package { manager, .. }) => manager.requires_elevation(),
            Some(other) => elevated::detect::path_requires_elevation(other, self.action),
            None => false,
        }
    }
}

/// Build a `Plan` from a checked module graph.
///
/// `keron_root` is the canonical absolute path the user passed to
/// `keron apply` (or its parent for the single-file case); it is
/// surfaced to user code through the `keron_root()` builtin so paths
/// can be expressed relative to the install location.
///
/// Symlinks are diffed against the live filesystem (Create / Update /
/// `NoOp`, or a hard error when an unrelated file sits at the target).
/// Other resource kinds are still reported as `Action::Create`; they
/// land alongside their respective executor support.
pub fn build_plan(graph: &keron_modules::ModuleGraph, keron_root: &Path) -> Result<Plan> {
    let resources = eval::eval_graph(graph, keron_root)?;
    let mut cache = PackageCache::new();
    let changes = resources
        .into_iter()
        .map(|state| classify(state, &mut cache))
        .collect::<Result<Vec<_>>>()?;
    Ok(Plan { changes })
}

fn classify(state: ResourceState, cache: &mut PackageCache) -> Result<ResourceChange> {
    let mut change = match &state {
        ResourceState::Symlink { from, to } => classify_symlink(from, to, &state)?,
        ResourceState::Template { path, content } => classify_template(path, content, &state)?,
        ResourceState::Package { manager, name } => {
            classify_package(*manager, name, &state, cache)?
        }
        ResourceState::Directory { .. } => ResourceChange {
            address: address_for(&state),
            kind: kind_for(&state),
            action: Action::Create,
            before: None,
            after: Some(state),
            requires_elevation: false,
        },
    };
    change.requires_elevation = change.compute_requires_elevation();
    Ok(change)
}

/// Classify a package resource by checking the cache. The cache is
/// populated lazily — the first time a given manager is queried, we
/// shell out to `<mgr> list`; subsequent queries reuse the snapshot.
/// `mark_to_install` both checks and inserts, so two
/// `brew("ripgrep")` resources in the same plan classify as
/// Create / `NoOp` rather than Create / Create.
fn classify_package(
    manager: PackageManager,
    name: &str,
    state: &ResourceState,
    cache: &mut PackageCache,
) -> Result<ResourceChange> {
    let already = cache.mark_to_install(manager, name)?;
    let action = if already {
        Action::NoOp
    } else {
        Action::Create
    };
    Ok(ResourceChange {
        address: address_for(state),
        kind: ResourceKind::Package,
        action,
        before: if already { Some(state.clone()) } else { None },
        after: Some(state.clone()),
        requires_elevation: false,
    })
}

/// Diff a desired symlink against the live filesystem.
///
/// - missing path → `Create`
/// - symlink already pointing at `to` → `NoOp`
/// - symlink pointing elsewhere → `Update` (the existing target is the
///   `before` state)
/// - non-symlink occupant → hard error: the apply pipeline refuses to
///   overwrite real files, mirroring how `stow` and friends bail rather
///   than clobber user data
fn classify_symlink(from: &Path, to: &Path, after: &ResourceState) -> Result<ResourceChange> {
    let address = from.display().to_string();
    let kind = ResourceKind::Symlink;
    match fs::symlink_metadata(from) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ResourceChange {
            address,
            kind,
            action: Action::Create,
            before: None,
            after: Some(after.clone()),
            requires_elevation: false,
        }),
        Err(e) => Err(anyhow!("reading existing path `{}`: {e}", from.display())),
        Ok(meta) if meta.file_type().is_symlink() => {
            // `read_link` gives the literal target as stored — used
            // verbatim for the diff display. `canonicalize(from)`
            // follows the link to its final destination so the
            // NoOp/Update decision compares apples to apples with the
            // desired `to` (which the evaluator has already
            // canonicalized). A broken link (canonicalize fails) is
            // always Update; the user wants the planner to fix it.
            let current_literal = fs::read_link(from)
                .with_context(|| format!("reading existing symlink `{}`", from.display()))?;
            let resolves_to_same = fs::canonicalize(from)
                .ok()
                .is_some_and(|resolved| resolved == to);
            let before = ResourceState::Symlink {
                from: from.to_path_buf(),
                to: current_literal,
            };
            let action = if resolves_to_same {
                Action::NoOp
            } else {
                Action::Update
            };
            Ok(ResourceChange {
                address,
                kind,
                action,
                before: Some(before),
                after: Some(after.clone()),
                requires_elevation: false,
            })
        }
        Ok(_) => bail!(
            "`{}` exists and is not a symlink; refusing to overwrite",
            from.display()
        ),
    }
}

/// Diff a desired template render against the live filesystem.
///
/// - missing path → `Create`
/// - regular file with byte-identical content → `NoOp`
/// - regular file with different content → `Update` (the existing
///   contents form the `before` state)
/// - symlink / directory / other non-file occupant → hard error: the
///   apply pipeline refuses to clobber user data the same way
///   [`classify_symlink`] does for non-symlinks
///
/// Comparison is bytewise so a non-UTF-8 existing file falls cleanly
/// through to `Update` rather than failing the read.
fn classify_template(path: &Path, content: &str, after: &ResourceState) -> Result<ResourceChange> {
    let address = path.display().to_string();
    let kind = ResourceKind::Template;
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ResourceChange {
            address,
            kind,
            action: Action::Create,
            before: None,
            after: Some(after.clone()),
            requires_elevation: false,
        }),
        Err(e) => Err(anyhow!("reading existing path `{}`: {e}", path.display())),
        Ok(meta) if meta.file_type().is_file() => {
            let existing_bytes = fs::read(path)
                .with_context(|| format!("reading existing template `{}`", path.display()))?;
            let same_content = existing_bytes == content.as_bytes();
            let action = if same_content {
                Action::NoOp
            } else {
                Action::Update
            };
            // `from_utf8_lossy` is fine here — the diff renderer
            // only consumes `before.content` for display; the
            // NoOp/Update decision above already used the lossless
            // byte slice.
            let existing_text = String::from_utf8_lossy(&existing_bytes).into_owned();
            Ok(ResourceChange {
                address,
                kind,
                action,
                before: Some(ResourceState::Template {
                    path: path.to_path_buf(),
                    content: existing_text,
                }),
                after: Some(after.clone()),
                requires_elevation: false,
            })
        }
        Ok(_) => bail!(
            "`{}` exists and is not a regular file; refusing to overwrite",
            path.display()
        ),
    }
}

fn address_for(state: &ResourceState) -> String {
    match state {
        ResourceState::Template { path, .. } | ResourceState::Directory { path } => {
            path.display().to_string()
        }
        ResourceState::Symlink { from, .. } => from.display().to_string(),
        // `<manager>:<name>` is unique enough to dedupe "brew install
        // ripgrep" from "cargo install ripgrep" while still being
        // human-readable in the diff header.
        ResourceState::Package { manager, name } => format!("{}:{}", manager.label(), name),
    }
}

const fn kind_for(state: &ResourceState) -> ResourceKind {
    match state {
        ResourceState::Template { .. } => ResourceKind::Template,
        ResourceState::Directory { .. } => ResourceKind::Directory,
        ResourceState::Symlink { .. } => ResourceKind::Symlink,
        ResourceState::Package { .. } => ResourceKind::Package,
    }
}

#[cfg(test)]
impl Plan {
    pub fn sample() -> Self {
        Self {
            changes: vec![
                ResourceChange {
                    address: "~/.zshrc".into(),
                    kind: ResourceKind::Template,
                    action: Action::Create,
                    before: None,
                    after: Some(ResourceState::Template {
                        path: PathBuf::from("~/.zshrc"),
                        content: "export PATH=...".into(),
                    }),
                    requires_elevation: false,
                },
                ResourceChange {
                    address: "~/.config/nvim".into(),
                    kind: ResourceKind::Symlink,
                    action: Action::Update,
                    before: Some(ResourceState::Symlink {
                        from: PathBuf::from("~/.config/nvim"),
                        to: PathBuf::from("/old/target"),
                    }),
                    after: Some(ResourceState::Symlink {
                        from: PathBuf::from("~/.config/nvim"),
                        to: PathBuf::from("/new/target"),
                    }),
                    requires_elevation: false,
                },
                ResourceChange {
                    address: "/tmp/scratch".into(),
                    kind: ResourceKind::Directory,
                    action: Action::Destroy,
                    before: Some(ResourceState::Directory {
                        path: PathBuf::from("/tmp/scratch"),
                    }),
                    after: None,
                    requires_elevation: false,
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_each_action() {
        let plan = Plan::sample();
        let s = plan.summary();
        assert_eq!(s.add, 1);
        assert_eq!(s.change, 1);
        assert_eq!(s.destroy, 1);
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
            }],
        };
        assert!(only_noop.is_empty());
        assert!(!Plan::sample().is_empty());
    }

    #[test]
    fn address_for_file_uses_path() {
        let s = ResourceState::Template {
            path: PathBuf::from("/etc/x"),
            content: "y".into(),
        };
        assert_eq!(address_for(&s), "/etc/x");
    }

    #[test]
    fn address_for_directory_uses_path() {
        let s = ResourceState::Directory {
            path: PathBuf::from("/d"),
        };
        assert_eq!(address_for(&s), "/d");
    }

    #[test]
    fn address_for_symlink_uses_from() {
        let s = ResourceState::Symlink {
            from: PathBuf::from("/a"),
            to: PathBuf::from("/b"),
        };
        assert_eq!(address_for(&s), "/a");
    }

    #[test]
    fn build_plan_emits_one_change_per_resource() {
        // End-to-end: build a graph from source, run build_plan, and
        // verify the resulting Plan reflects the resources produced
        // by the evaluator. Mutating build_plan to Ok(Default) would
        // produce an empty Plan, breaking every assertion below.
        use keron_modules::{EntrySource, ModuleId, resolve};
        use std::env;
        use std::fs;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("keron-build-plan-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let entry = dir.join("entry.keron");
        // Seed a one-placeholder template alongside the entry so the
        // `template(path = X, source = "tmpl.tpl", vars = {"body": Y})`
        // form below resolves at eval time.
        fs::write(dir.join("tmpl.tpl"), "${body}").unwrap();
        let src = "reconcile template(path = \"/a\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
                   reconcile template(path = \"/b\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n";
        fs::write(&entry, src).unwrap();
        let canonical = fs::canonicalize(&entry).unwrap();
        let keron_root = canonical.parent().unwrap().to_path_buf();
        let graph = resolve(vec![EntrySource {
            text: src.into(),
            base_dir: canonical.parent().unwrap().to_path_buf(),
            id: ModuleId::File(canonical),
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

    // ---------- classify_symlink ----------

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
            // Tests compare against `fs::canonicalize(link)` for the
            // NoOp arm, so the test path must already be canonical
            // (macOS `env::temp_dir()` returns `/var/folders/...`,
            // which canonicalize rewrites to `/private/var/folders/...`).
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

    #[test]
    fn classify_symlink_marks_missing_path_as_create() {
        let d = TempDir::new("missing");
        let from = d.path.join("alias");
        let to = d.path.join("real");
        let state = desired(&from, &to);
        let change = classify(state.clone(), &mut PackageCache::new()).unwrap();
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
        let change = classify(state, &mut PackageCache::new()).unwrap();
        assert_eq!(change.action, Action::NoOp);
        // The captured `before` records what's already on disk so the
        // diff renderer can show it; mutating either side would break
        // a future "show the existing link target" UX.
        let before = change.before.expect("before populated for noop");
        let ResourceState::Symlink { from: bf, to: bt } = before else {
            panic!("expected Symlink in before");
        };
        assert_eq!(bf, from);
        assert_eq!(bt, to);
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
        let change = classify(state, &mut PackageCache::new()).unwrap();
        assert_eq!(change.action, Action::Update);
        let before = change.before.expect("before populated for update");
        let ResourceState::Symlink { to: bt, .. } = before else {
            panic!("expected Symlink before");
        };
        assert_eq!(bt, old_target, "before should record the *current* target");
    }

    #[test]
    fn classify_symlink_rejects_real_file_occupant() {
        // A real file at the symlink path is exactly the case where
        // silent overwriting would lose user data. The planner refuses
        // so the user sees the conflict at plan time, not after their
        // dotfile has already been destroyed.
        let d = TempDir::new("clobber");
        let from = d.path.join("alias");
        fs::write(&from, "user data").unwrap();
        let to = d.path.join("target");

        let err = classify(desired(&from, &to), &mut PackageCache::new())
            .expect_err("real file must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("not a symlink"), "got: {msg}");
        assert!(msg.contains("refusing to overwrite"), "got: {msg}");
    }

    #[test]
    fn classify_directory_still_defaults_to_create() {
        // Directories don't yet have a live-state classifier (no
        // executor support yet either); the Create-only fallback
        // remains until that lands.
        let state = ResourceState::Directory {
            path: PathBuf::from("/whatever-keron-dir"),
        };
        let change = classify(state.clone(), &mut PackageCache::new()).unwrap();
        assert_eq!(change.action, Action::Create);
        assert!(change.before.is_none());
        assert_eq!(change.after, Some(state));
    }

    // ---------- classify_template ----------

    fn template(path: &Path, content: &str) -> ResourceState {
        ResourceState::Template {
            path: path.to_path_buf(),
            content: content.into(),
        }
    }

    #[test]
    fn classify_template_marks_missing_path_as_create() {
        let d = TempDir::new("template-missing");
        let path = d.path.join("config.toml");
        let state = template(&path, "x = 1\n");
        let change = classify(state.clone(), &mut PackageCache::new()).unwrap();
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
        let change = classify(state, &mut PackageCache::new()).unwrap();
        assert_eq!(change.action, Action::NoOp);
        let before = change.before.expect("before populated for noop");
        let ResourceState::Template { content: bc, .. } = before else {
            panic!("expected Template before");
        };
        assert_eq!(bc, "hello\n");
    }

    #[test]
    fn classify_template_marks_diverging_content_as_update() {
        let d = TempDir::new("template-update");
        let path = d.path.join("config.toml");
        fs::write(&path, "old\n").unwrap();
        let state = template(&path, "new\n");
        let change = classify(state, &mut PackageCache::new()).unwrap();
        assert_eq!(change.action, Action::Update);
        let before = change.before.expect("before populated for update");
        let ResourceState::Template { content: bc, .. } = before else {
            panic!("expected Template before");
        };
        assert_eq!(bc, "old\n", "before should record the *current* content");
    }

    #[test]
    fn classify_template_tolerates_non_utf8_existing_file() {
        // A non-UTF-8 file on disk should classify as Update (the
        // bytes don't equal our UTF-8 content) without erroring at
        // the read. `from_utf8_lossy` produces the `before` state
        // for diff display.
        let d = TempDir::new("template-non-utf8");
        let path = d.path.join("binary");
        fs::write(&path, [0xFFu8, 0xFE, 0xFD]).unwrap();
        let state = template(&path, "ascii only\n");
        let change = classify(state, &mut PackageCache::new()).unwrap();
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
        let err = classify(template(&path, "y"), &mut PackageCache::new())
            .expect_err("symlink should not be treated as a template target");
        let msg = format!("{err:#}");
        assert!(msg.contains("not a regular file"), "got: {msg}");
        assert!(msg.contains("refusing to overwrite"), "got: {msg}");
    }
}
