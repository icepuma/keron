//! `Plan` — the diffable, renderable description of what `apply` will
//! do. [`build_plan`] runs the evaluator over a checked module graph
//! and classifies each produced resource into a [`ResourceChange`].
//!
//! Resources are diffed against the live filesystem so the rendered
//! plan reflects what `keron apply --execute` will actually perform;
//! removals are intentionally out-of-scope until keron has managed
//! state proving ownership.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::elevated;
use crate::eval;
use crate::packages::PackageCache;
use crate::platform::OsFamily;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Create,
    Update,
    Run,
    NoOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKind {
    Template,
    Symlink,
    Package,
    Shell,
}

impl ResourceKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Symlink => "symlink",
            Self::Package => "package",
            Self::Shell => "shell",
        }
    }
}

/// Shell interpreter selected by the `shell(kind = ...)` constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShellKind {
    Sh,
    Bash,
    Zsh,
    Pwsh,
    Powershell,
}

impl ShellKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sh => "sh",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Pwsh => "pwsh",
            Self::Powershell => "powershell",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "sh" => Ok(Self::Sh),
            "bash" => Ok(Self::Bash),
            "zsh" => Ok(Self::Zsh),
            "pwsh" => Ok(Self::Pwsh),
            "powershell" => Ok(Self::Powershell),
            _ => bail!(
                "`{raw}` is not a valid ShellKind; expected one of sh, bash, zsh, pwsh, powershell"
            ),
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

    pub const fn is_supported_on(self, os: OsFamily) -> bool {
        match self {
            Self::Brew => matches!(os, OsFamily::Linux | OsFamily::Macos),
            Self::Cargo => true,
            Self::Winget => matches!(os, OsFamily::Windows),
        }
    }

    pub const fn supported_os_label(self) -> &'static str {
        match self {
            Self::Brew => "Linux or Macos",
            Self::Cargo => "any OS",
            Self::Winget => "Windows",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceState {
    Template {
        path: PathBuf,
        content: String,
        #[serde(default)]
        sensitive: bool,
    },
    Symlink {
        from: PathBuf,
        to: PathBuf,
    },
    Package {
        manager: PackageManager,
        name: String,
    },
    Shell {
        kind: ShellKind,
        name: String,
        cwd: PathBuf,
        script: String,
        #[serde(default)]
        sensitive: bool,
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
    /// True when applying this change would overwrite or remove a
    /// pre-existing filesystem object without managed-state proof.
    /// The executor will only proceed after an explicit force prompt.
    #[serde(default)]
    pub requires_force: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    pub changes: Vec<ResourceChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedPackage {
    pub address: String,
    pub manager: PackageManager,
    pub name: String,
    pub os: OsFamily,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Precheck {
    pub unsupported_packages: Vec<UnsupportedPackage>,
}

impl Precheck {
    pub const fn is_empty(&self) -> bool {
        self.unsupported_packages.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct PrecheckedPlan {
    pub plan: Plan,
    pub precheck: Precheck,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlanSummary {
    pub add: usize,
    pub change: usize,
    pub run: usize,
    /// Declared resources that already match the live state. Surfaced
    /// in the diff footer as `N unchanged` so the user can see what the
    /// manifest manages even when nothing needs to be applied.
    pub unchanged: usize,
    /// How many of the above are flagged as requiring elevated rights.
    /// Sub-total of `add + change`, surfaced in the diff
    /// summary as `(N elevated)`.
    pub elevated: usize,
    pub force: usize,
}

impl Plan {
    pub fn summary(&self) -> PlanSummary {
        let mut s = PlanSummary::default();
        for c in &self.changes {
            match c.action {
                Action::Create => s.add += 1,
                Action::Update => s.change += 1,
                Action::Run => s.run += 1,
                Action::NoOp => {
                    s.unchanged += 1;
                    continue;
                }
            }
            if c.requires_elevation {
                s.elevated += 1;
            }
            if c.requires_force {
                s.force += 1;
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

    /// Whether this change must be explicitly force-approved because
    /// keron cannot prove ownership of the existing destination.
    pub fn compute_requires_force(&self) -> bool {
        if !matches!(self.action, Action::Update) {
            return false;
        }
        matches!(
            self.after.as_ref().or(self.before.as_ref()),
            Some(ResourceState::Symlink { .. } | ResourceState::Template { .. })
        )
    }
}

/// Build a `Plan` from a checked module graph.
///
/// `keron_root` is the canonical absolute path the user passed to
/// `keron apply` (or its parent for the single-file case); it is
/// surfaced to user code through the `keron_root()` builtin so paths
/// can be expressed relative to the install location.
///
/// Desired resources are diffed against the live filesystem (Create /
/// Update / `NoOp`, or a hard error when an unrelated file sits at the
/// target). Resources absent from the desired graph are ignored:
/// without persisted managed state, keron cannot prove that such paths
/// are safe to remove.
pub fn build_plan(graph: &keron_modules::ModuleGraph, keron_root: &Path) -> Result<Plan> {
    Ok(build_prechecked_plan(graph, keron_root)?.plan)
}

pub fn build_prechecked_plan(
    graph: &keron_modules::ModuleGraph,
    keron_root: &Path,
) -> Result<PrecheckedPlan> {
    let resources = eval::eval_graph(graph, keron_root)?;
    let resources = dedup_resources(&resources)?;
    let os = crate::platform::detect_os_family();
    let precheck = precheck_resources(&resources, os);
    let mut cache = PackageCache::new();
    let changes = resources
        .iter()
        .filter(|state| include_in_plan(state, os))
        .map(|state| classify(state, &mut cache))
        .collect::<Result<Vec<_>>>()?;
    Ok(PrecheckedPlan {
        plan: Plan { changes },
        precheck,
    })
}

/// Collapse repeat declarations of the same resource. Two `reconcile`
/// statements that name the same path (or the same `manager:name` /
/// shell name) are common in real configs — composing a base list with
/// per-host overrides, importing a library that already reconciles a
/// shared dotfile, etc. Without dedup the plan would carry two `Create`
/// changes for one address and the executor would fail at the second
/// with `EEXIST` *after* the first had already landed, leaving the
/// system in a half-applied state.
///
/// Equality is by full [`ResourceState`] value, not just by address:
/// two same-address declarations with diverging payload (e.g. one
/// template and another with different `vars`, or two symlinks pointing
/// at different sources) is almost certainly a mistake — we surface it
/// as a hard error at plan time so the user fixes the conflict before
/// any change touches disk.
///
/// Order of the first occurrence is preserved so apply order matches
/// declaration order.
fn dedup_resources(resources: &[ResourceState]) -> Result<Vec<ResourceState>> {
    let mut seen: HashMap<String, &ResourceState> = HashMap::new();
    let mut out: Vec<ResourceState> = Vec::with_capacity(resources.len());
    for state in resources {
        let address = address_for(state);
        if let Some(prev) = seen.get(&address) {
            if *prev != state {
                bail!(
                    "duplicate resource `{address}` declared with conflicting state: \
                     two reconciliations target the same address but with different values; \
                     drop one of the declarations or align the values"
                );
            }
            continue;
        }
        seen.insert(address, state);
        out.push(state.clone());
    }
    Ok(out)
}

const fn include_in_plan(state: &ResourceState, os: OsFamily) -> bool {
    match state {
        ResourceState::Package { manager, .. } => manager.is_supported_on(os),
        ResourceState::Symlink { .. }
        | ResourceState::Template { .. }
        | ResourceState::Shell { .. } => true,
    }
}

fn precheck_resources(resources: &[ResourceState], os: OsFamily) -> Precheck {
    let unsupported_packages = resources
        .iter()
        .filter_map(|state| {
            let ResourceState::Package { manager, name } = state else {
                return None;
            };
            if manager.is_supported_on(os) {
                return None;
            }
            Some(UnsupportedPackage {
                address: address_for(state),
                manager: *manager,
                name: name.clone(),
                os,
            })
        })
        .collect();
    Precheck {
        unsupported_packages,
    }
}

fn classify(state: &ResourceState, cache: &mut PackageCache) -> Result<ResourceChange> {
    let mut change = match state {
        ResourceState::Symlink { from, to } => classify_symlink(from, to, state)?,
        ResourceState::Template { path, content, .. } => classify_template(path, content, state)?,
        ResourceState::Package { manager, name } => classify_package(*manager, name, state, cache)?,
        ResourceState::Shell { kind, .. } => classify_shell(*kind, state)?,
    };
    change.requires_elevation = change.compute_requires_elevation();
    change.requires_force = change.compute_requires_force();
    Ok(change)
}

fn classify_shell(kind: ShellKind, state: &ResourceState) -> Result<ResourceChange> {
    which::which(kind.label())
        .with_context(|| format!("shell `{}` is not available on PATH", kind.label()))?;
    Ok(ResourceChange {
        address: address_for(state),
        kind: ResourceKind::Shell,
        action: Action::Run,
        before: None,
        after: Some(state.clone()),
        requires_elevation: false,
        requires_force: false,
    })
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
        requires_force: false,
    })
}

/// Diff a desired symlink against the live filesystem.
///
/// - missing path → `Create`
/// - symlink already pointing at `source` → `NoOp`
/// - symlink pointing elsewhere → `Update` (the existing target is the
///   `before` state)
/// - non-symlink occupant → hard error: the apply pipeline refuses to
///   overwrite real files, mirroring how `stow` and friends bail rather
///   than clobber user data
fn classify_symlink(target: &Path, source: &Path, after: &ResourceState) -> Result<ResourceChange> {
    let address = target.display().to_string();
    let kind = ResourceKind::Symlink;
    match fs::symlink_metadata(target) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ResourceChange {
            address,
            kind,
            action: Action::Create,
            before: None,
            after: Some(after.clone()),
            requires_elevation: false,
            requires_force: false,
        }),
        Err(e) => Err(anyhow!("reading existing path `{}`: {e}", target.display())),
        Ok(meta) if meta.file_type().is_symlink() => {
            // `read_link` returns the literal target for diff display;
            // `canonicalize` follows the link so NoOp/Update compares
            // apples to apples against `source` (already canonicalized by
            // the evaluator). A broken link is always Update.
            let current_literal = fs::read_link(target)
                .with_context(|| format!("reading existing symlink `{}`", target.display()))?;
            let resolves_to_same = match fs::canonicalize(target) {
                Ok(resolved) => resolved == source,
                // A dangling symlink (target missing) legitimately
                // canonicalizes-to-error; classify as Update. Any
                // other error (EACCES on an intermediate dir, EIO,
                // …) would silently look like "different target"
                // and let apply fail later with worse context, so
                // bail with the underlying error attached.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => {
                    return Err(anyhow!(
                        "canonicalizing existing symlink `{}`: {e}",
                        target.display()
                    ));
                }
            };
            let before = ResourceState::Symlink {
                from: target.to_path_buf(),
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
                requires_force: false,
            })
        }
        Ok(_) => bail!(
            "`{}` exists and is not a symlink; refusing to overwrite",
            target.display()
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
            requires_force: false,
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
            // The diff renderer only consumes `before.content` for
            // display; NoOp/Update was decided on the lossless byte
            // slice above so `from_utf8_lossy` is safe here.
            let existing_text = String::from_utf8_lossy(&existing_bytes).into_owned();
            Ok(ResourceChange {
                address,
                kind,
                action,
                before: Some(ResourceState::Template {
                    path: path.to_path_buf(),
                    content: existing_text,
                    sensitive: false,
                }),
                after: Some(after.clone()),
                requires_elevation: false,
                requires_force: false,
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
        ResourceState::Template { path, .. } => path.display().to_string(),
        ResourceState::Symlink { from, .. } => from.display().to_string(),
        ResourceState::Package { manager, name } => format!("{}:{}", manager.label(), name),
        ResourceState::Shell { name, .. } => name.clone(),
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
                        sensitive: false,
                    }),
                    requires_elevation: false,
                    requires_force: false,
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
                    requires_force: false,
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
            },
            ResourceState::Package {
                manager: PackageManager::Brew,
                name: "ripgrep".into(),
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
        let src = format!(
            "val t: Template = template(source = \"tmpl.tpl\", target = \"{}\", vars = {{\"body\": \"x\"}})\n\
             reconcile t\n\
             reconcile t\n",
            target.display(),
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
        let src = format!(
            "reconcile template(source = \"tmpl.tpl\", target = \"{path}\", vars = {{\"body\": \"first\"}})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"{path}\", vars = {{\"body\": \"second\"}})\n",
            path = target.display(),
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
            // NoOp arm compares against `fs::canonicalize(link)`, so
            // the test path must already be canonical (macOS rewrites
            // `/var/folders/...` -> `/private/var/folders/...`).
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
        let change = classify(&state, &mut PackageCache::new()).unwrap();
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
        let err = classify(&state, &mut PackageCache::new()).expect_err("missing bash should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("shell `bash` is not available on PATH"));
    }

    #[test]
    fn classify_symlink_marks_missing_path_as_create() {
        let d = TempDir::new("missing");
        let from = d.path.join("alias");
        let to = d.path.join("real");
        let state = desired(&from, &to);
        let change = classify(&state, &mut PackageCache::new()).unwrap();
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
        let change = classify(&state, &mut PackageCache::new()).unwrap();
        assert_eq!(change.action, Action::NoOp);
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
        let change = classify(&state, &mut PackageCache::new()).unwrap();
        assert_eq!(change.action, Action::Update);
        assert!(change.requires_force);
        let before = change.before.expect("before populated for update");
        let ResourceState::Symlink { to: bt, .. } = before else {
            panic!("expected Symlink before");
        };
        assert_eq!(bt, old_target, "before should record the *current* target");
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
        let change = classify(&desired(&from, &new_target), &mut PackageCache::new())
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
    fn classify_symlink_bails_on_non_enoent_canonicalize_error() {
        // ELOOP from a symlink cycle is not NotFound; the second
        // `Err(e)` arm must bail rather than silently treat it as
        // "diverging target". Pins the `replace match guard … with
        // true` mutation, which would funnel all errors through the
        // dangling-link path and quietly return Update.
        let d = TempDir::new("loop");
        let a = d.path.join("a");
        let b = d.path.join("b");
        // a -> b, b -> a forms a 2-cycle. canonicalize(a) returns
        // ELOOP, which has io::ErrorKind::FilesystemLoop on modern
        // Rust (mapped to Uncategorized on older toolchains) — in
        // either case, NOT NotFound.
        make_symlink(&b, &a).unwrap();
        make_symlink(&a, &b).unwrap();

        let new_target = d.path.join("target");
        fs::write(&new_target, "x").unwrap();
        let err = classify(&desired(&a, &new_target), &mut PackageCache::new())
            .expect_err("symlink loop must surface a canonicalize error, not be papered over");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("canonicalizing existing symlink"),
            "got: {msg}"
        );
    }

    #[test]
    fn classify_symlink_rejects_real_file_occupant() {
        let d = TempDir::new("clobber");
        let from = d.path.join("alias");
        fs::write(&from, "user data").unwrap();
        let to = d.path.join("target");

        let err = classify(&desired(&from, &to), &mut PackageCache::new())
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
        let change = classify(&state, &mut PackageCache::new()).unwrap();
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
        let change = classify(&state, &mut PackageCache::new()).unwrap();
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
        let change = classify(&state, &mut PackageCache::new()).unwrap();
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
        let change = classify(&state, &mut PackageCache::new()).unwrap();
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
        let err = classify(&template(&path, "y"), &mut PackageCache::new())
            .expect_err("symlink should not be treated as a template target");
        let msg = format!("{err:#}");
        assert!(msg.contains("not a regular file"), "got: {msg}");
        assert!(msg.contains("refusing to overwrite"), "got: {msg}");
    }
}
