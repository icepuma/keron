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
    /// Homebrew tap registration. Synthesized by the planner from any
    /// [`ResourceState::Package`] whose `tap` field is `Some`; never
    /// constructed directly by user code.
    Tap,
    SshKey,
    GpgKey,
}

impl ResourceKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Symlink => "symlink",
            Self::Package => "package",
            Self::Shell => "shell",
            Self::Tap => "tap",
            Self::SshKey => "ssh_key",
            Self::GpgKey => "gpg_key",
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
///
/// `BrewCask` shares the `brew` binary with `Brew` but routes through
/// `brew install --cask` / `brew list --cask -1` / etc. — distinct from
/// `Brew` so the cache namespaces installed casks separately and the
/// classifier compares against the right "outdated" set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PackageManager {
    Brew,
    BrewCask,
    Cargo,
    Winget,
}

impl PackageManager {
    /// The CLI binary name. `Brew` and `BrewCask` share `"brew"` —
    /// the cask/formula split is a flag at invocation time, not a
    /// different program.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Brew | Self::BrewCask => "brew",
            Self::Cargo => "cargo",
            Self::Winget => "winget",
        }
    }

    /// User-facing manager name used in addresses and diagnostics.
    /// Diverges from [`Self::label`] for `BrewCask` so the diff reads
    /// `cask:font-jetbrains-mono` rather than `brew:font-jetbrains-mono`.
    pub const fn kind_label(self) -> &'static str {
        match self {
            Self::Brew => "brew",
            Self::BrewCask => "cask",
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
    ///   - `Brew` / `BrewCask` — refuse to run under sudo by design.
    ///   - `Cargo` — installs to `~/.cargo/bin`, per-user.
    ///   - `Winget` — brokers its own UAC at install time for
    ///     machine-scope packages.
    ///
    /// The hook stays so future managers (apt, dnf, pacman) can flip
    /// their arm to `true` without rewiring callers.
    ///
    /// `#[cfg_attr(test, mutants::skip)]`: every existing variant
    /// returns `false`, so the function-body replacement
    /// `-> bool with false` is an equivalent mutation that no test
    /// could distinguish until a future manager flips its arm.
    #[cfg_attr(test, mutants::skip)]
    pub const fn requires_elevation(self) -> bool {
        match self {
            Self::Brew | Self::BrewCask | Self::Cargo | Self::Winget => false,
        }
    }

    pub const fn is_supported_on(self, os: OsFamily) -> bool {
        match self {
            Self::Brew => matches!(os, OsFamily::Linux | OsFamily::Macos),
            // Casks are macOS-only — Linux brew rejects `--cask`.
            Self::BrewCask => matches!(os, OsFamily::Macos),
            Self::Cargo => true,
            Self::Winget => matches!(os, OsFamily::Windows),
        }
    }

    pub const fn supported_os_label(self) -> &'static str {
        match self {
            Self::Brew => "Linux or Macos",
            Self::BrewCask => "Macos",
            Self::Cargo => "any OS",
            Self::Winget => "Windows",
        }
    }
}

/// A Homebrew tap declaration carried alongside a `Package`. Synthesized
/// by the evaluator from `user/tap/formula` name parsing plus an
/// optional `tap_url` second arg.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TapSpec {
    /// `user/tap` — the canonical homebrew tap identifier. Always
    /// lowercase ASCII per brew's conventions.
    pub user_tap: String,
    /// Custom remote URL. `None` means brew derives the URL from the
    /// `homebrew-<tap>` GitHub convention; `Some(url)` means we pass
    /// `--custom-remote` to ensure the tap points at this remote.
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceState {
    Template {
        path: PathBuf,
        content: String,
        /// Drives the file mode chosen by the executor: `true` →
        /// `0o600` (owner-only), `false` → standard `0o644`-after-umask.
        /// No longer affects diff rendering — verbose mode reveals
        /// content regardless, default mode hides content regardless
        /// (see `--verbose-will-reveal-sensitive-content`).
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
        /// Optional tap binding for `Brew` / `BrewCask` packages.
        /// `Some(_)` means the planner will synthesize a sibling
        /// [`ResourceState::Tap`] change so the tap registration
        /// appears in the plan diff before the package install. Always
        /// `None` for `Cargo` / `Winget` — they have no tap concept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tap: Option<TapSpec>,
    },
    /// Homebrew tap registration. Materialized by the planner from
    /// [`ResourceState::Package::tap`]; not constructible from user
    /// source. Carries enough state for the executor to call
    /// `brew tap [--custom-remote] user/tap [URL]`.
    Tap(TapSpec),
    Shell {
        kind: ShellKind,
        name: String,
        cwd: PathBuf,
        script: String,
        /// True when any input that flowed into `script` was marked
        /// sensitive at the manifest layer. Surfaces a `[sensitive]`
        /// hint in the default-mode diff summary so an operator can
        /// see that a body block is going to print secrets before
        /// they opt in (via `--verbose-will-reveal-sensitive-content`
        /// or the interactive prompt) to see the actual content.
        /// Does not affect the executor (shell scripts run via stdin
        /// regardless).
        #[serde(default)]
        sensitive: bool,
    },
    /// User-supplied SSH keypair to write to disk as a single atomic
    /// resource. Both `private_key` and `public_key` are the literal
    /// material that lands on disk (the encrypted PEM blob for the
    /// private half, the OpenSSH `ssh-…` one-liner for the public
    /// half). Diff rendering treats this variant as always-sensitive;
    /// there is no opt-out flag, since SSH keys have no non-sensitive
    /// use case.
    SshKey {
        private_path: PathBuf,
        public_path: PathBuf,
        private_key: String,
        public_key: String,
    },
    /// User-supplied GPG secret-key import. `key` is the ASCII-armored
    /// blob produced by `gpg --export-secret-keys --armor <fpr>`. The
    /// executor pipes it to `gpg --batch --import` over child stdin —
    /// never argv, never a tempfile. `fingerprint` is the idempotency
    /// probe: a hex fingerprint string the classifier matches against
    /// `gpg --batch --list-secret-keys` (exit status only).
    GpgKey {
        fingerprint: String,
        key: String,
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
            // Tap operations shell out to `brew`, which refuses to run
            // under sudo — no elevation involved. Same fall-through as
            // the `None` arm, but kept explicit so a future kind that
            // forgets to opt in trips the exhaustiveness check.
            Some(ResourceState::Tap(_)) | None => false,
            Some(other) => elevated::detect::path_requires_elevation(other, self.action),
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
    build_prechecked_plan_with_prereq_probe(graph, keron_root, &crate::capability::LiveEnvProbe)
}

/// Test seam: build a prechecked plan with a caller-supplied
/// [`PrereqProbe`]. Production wires [`crate::capability::LiveEnvProbe`]
/// through the public entry point above; tests pass a mock so the
/// ordering guarantee ("`validate_prerequisites` fires before any
/// classify-time shell-out") can be exercised deterministically without
/// touching `$PATH`.
pub fn build_prechecked_plan_with_prereq_probe(
    graph: &keron_modules::ModuleGraph,
    keron_root: &Path,
    prereq_probe: &dyn crate::capability::PrereqProbe,
) -> Result<PrecheckedPlan> {
    // The same probe flows into eval — so a mock that gates the
    // plan-time prereq pass *also* gates secret-resolution session
    // checks. Two-tier separation in types, one probe in practice.
    let resources = eval::eval_graph_with_prereq_probe(graph, keron_root, prereq_probe)?;
    let resources = dedup_resources(&resources)?;
    let resources = synthesize_taps(&resources)?;
    let os = crate::platform::detect_os_family();
    let precheck = precheck_resources(&resources, os);
    let mut cache = PackageCache::new(os);
    // Eagerly fan out every probe the upcoming classify pass will
    // need (brew list / outdated / tap, cargo install --list, …) on
    // worker threads so the wall time collapses to roughly the slowest
    // probe. The lazy `ensure_*_loaded` paths still cover any cache
    // miss that prewarm didn't anticipate (e.g. a `classify_tap` on a
    // tap that doesn't appear in the filtered resource set), so this
    // is a pure speedup with no behavior change.
    let probe_inputs: Vec<_> = resources
        .iter()
        .filter(|state| include_in_plan(state, os))
        .cloned()
        .collect();
    // Tier-1 pre-check: every package-manager prerequisite must be
    // satisfied before any classify-time shell-out fires. Without
    // this, missing brew would surface as `brew list … exited with
    // status 127` from inside `cache.prewarm`, with no chance for a
    // hint-bearing diagnostic. Session prereqs (1Password CLI, etc.)
    // fire from inside `eval::resolve_secret` the moment a `secret()`
    // URI is evaluated — earlier than this point — so they're not
    // re-checked here.
    crate::capability::validate_prerequisites(&probe_inputs, prereq_probe)
        .map_err(|report| anyhow::Error::msg(report.to_string()))?;
    cache.prewarm(&probe_inputs)?;
    let changes = probe_inputs
        .iter()
        .map(|state| classify(state, &mut cache))
        .collect::<Result<Vec<_>>>()?;
    let plan = Plan { changes };
    // DAG-level capability validation: every resource's declared
    // `needs` (e.g. GpgKey → `gpg` binary) must be satisfied by the
    // live environment or by an earlier provider in the plan. Failing
    // here surfaces a hint-bearing diagnostic at plan time instead of
    // a generic crash mid-apply.
    crate::capability::validate_capabilities(&plan, &crate::capability::LiveEnvProbe)?;
    Ok(PrecheckedPlan { plan, precheck })
}

/// Expand every `Package { tap: Some(_) }` into a `[Tap, Package]`
/// pair, deduping taps that share the same `user_tap`. The synthesized
/// `Tap` is inserted **before** its first dependent package so the
/// executor's source-order pass runs `brew tap` before `brew install`.
///
/// Multiple packages may share a tap. URL merge rules:
///   - bare + bare → bare (no URL implied)
///   - bare + custom URL → custom URL wins (it's strictly more
///     specific — the bare form auto-derives the same URL when the
///     repo follows `homebrew-<tap>` convention; if it doesn't, the
///     custom URL is what the user intended)
///   - custom URL A + custom URL B where A != B → hard error
///     (silently picking one would produce surprising runtime behavior)
fn synthesize_taps(resources: &[ResourceState]) -> Result<Vec<ResourceState>> {
    let mut out: Vec<ResourceState> = Vec::with_capacity(resources.len());
    let mut tap_pos: HashMap<String, usize> = HashMap::new();
    for state in resources {
        if let ResourceState::Package {
            tap: Some(spec), ..
        } = state
        {
            match tap_pos.get(&spec.user_tap) {
                None => {
                    tap_pos.insert(spec.user_tap.clone(), out.len());
                    out.push(ResourceState::Tap(spec.clone()));
                }
                Some(&idx) => {
                    let ResourceState::Tap(existing) = &mut out[idx] else {
                        unreachable!("tap_pos must point at a Tap entry");
                    };
                    match (&existing.url, &spec.url) {
                        // A second mention with a custom URL upgrades
                        // a previously bare tap declaration.
                        (None, Some(_)) => existing.url.clone_from(&spec.url),
                        // Same URL or both bare → no-op.
                        (Some(a), Some(b)) if a == b => {}
                        (None | Some(_), None) => {}
                        // Two different custom URLs for the same tap
                        // is almost certainly a manifest bug.
                        (Some(a), Some(b)) => bail!(
                            "tap `{}` is declared with conflicting URLs: `{a}` and `{b}`; \
                             pick one or drop the second `tap_url`",
                            spec.user_tap,
                        ),
                    }
                }
            }
        }
        out.push(state.clone());
    }
    Ok(out)
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
        // Taps are macOS/Linux only — they ride along whenever a brew
        // package they back is in scope, so check brew's supported OS.
        ResourceState::Tap(_) => PackageManager::Brew.is_supported_on(os),
        ResourceState::Symlink { .. }
        | ResourceState::Template { .. }
        | ResourceState::Shell { .. }
        | ResourceState::SshKey { .. }
        | ResourceState::GpgKey { .. } => true,
    }
}

fn precheck_resources(resources: &[ResourceState], os: OsFamily) -> Precheck {
    let unsupported_packages = resources
        .iter()
        .filter_map(|state| {
            let ResourceState::Package { manager, name, .. } = state else {
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
        ResourceState::Package { manager, name, .. } => {
            classify_package(*manager, name, state, cache)?
        }
        ResourceState::Tap(spec) => classify_tap(spec, state, cache)?,
        ResourceState::Shell { kind, .. } => classify_shell(*kind, state)?,
        ResourceState::SshKey {
            private_path,
            public_path,
            private_key,
            public_key,
        } => classify_ssh_key(private_path, public_path, private_key, public_key, state)?,
        ResourceState::GpgKey { fingerprint, .. } => {
            let status = probe_gpg_keyring(fingerprint)?;
            classify_gpg_key(state, status)
        }
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

/// Classify a package resource against the live state.
///
/// For brew formulae and casks, the cache holds two sets:
///   - `installed_*` — bare names from `brew list --formula -1` (resp.
///     `--cask -1`). Tap-installed formulae appear here under their
///     bare formula name, not the qualified `user/tap/formula` form.
///   - `outdated_*` — qualified names from `brew outdated`.
///
/// So we look the bare tail of the manifest name (`"ripgrep"` for both
/// `brew("ripgrep")` and `brew("icepuma/keron/keron")`) up in installed,
/// and the qualified name (`"icepuma/keron/keron"` or the bare form) up
/// in outdated. Update wins over `NoOp` wins over Create.
///
/// Cargo / winget keep today's behavior: bare name only, install-only,
/// no outdated probe. The cache's per-package "scheduled" dedup also
/// stays: two `brew("ripgrep")` resources in the same plan collapse to
/// Create + `NoOp` rather than Create + Create.
fn classify_package(
    manager: PackageManager,
    name: &str,
    state: &ResourceState,
    cache: &mut PackageCache,
) -> Result<ResourceChange> {
    let action = cache.classify_package(manager, name)?;
    Ok(ResourceChange {
        address: address_for(state),
        kind: ResourceKind::Package,
        action,
        before: if matches!(action, Action::Create) {
            None
        } else {
            Some(state.clone())
        },
        after: Some(state.clone()),
        requires_elevation: false,
        requires_force: false,
    })
}

/// Classify a `Tap` change against the live state of tapped repos.
/// See [`PackageCache::classify_tap`] for the three-state rule.
fn classify_tap(
    spec: &TapSpec,
    state: &ResourceState,
    cache: &mut PackageCache,
) -> Result<ResourceChange> {
    let action = cache.classify_tap(spec)?;
    Ok(ResourceChange {
        address: address_for(state),
        kind: ResourceKind::Tap,
        action,
        before: if matches!(action, Action::Create) {
            None
        } else {
            Some(state.clone())
        },
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

/// Diff a desired SSH keypair against the live filesystem.
///
/// SSH keys ensure *presence* only — `apply` never silently rotates an
/// existing key (a rotation could lock the user out of every machine
/// that trusts the old public key). The diff is therefore three-state,
/// not four: Create when both files are missing, `NoOp` when both
/// files match byte-for-byte, hard error in every other case. In
/// particular:
///
///   - one file matches, the other is missing → "out of sync" error
///   - either file exists with different content → "refusing to
///     rotate" error
///   - either path is a symlink / directory / other non-regular-file
///     occupant → "refusing to overwrite" error (mirrors
///     [`classify_symlink`] / [`classify_template`])
///
/// The user resolves drift by removing the existing file manually,
/// after which `apply` writes the declared key.
fn classify_ssh_key(
    private_path: &Path,
    public_path: &Path,
    private_key: &str,
    public_key: &str,
    after: &ResourceState,
) -> Result<ResourceChange> {
    let address = address_for(after);
    let kind = ResourceKind::SshKey;
    let private = probe_ssh_key_file(private_path, private_key.as_bytes())?;
    let public = probe_ssh_key_file(public_path, public_key.as_bytes())?;
    let action = match (private, public) {
        (KeyFileState::Missing, KeyFileState::Missing) => Action::Create,
        (KeyFileState::Match, KeyFileState::Match) => Action::NoOp,
        _ => bail!(
            "ssh key files at `{}` / `{}` are out of sync; \
             remove the existing key files manually if a new key is intended",
            private_path.display(),
            public_path.display(),
        ),
    };
    let before = if matches!(action, Action::NoOp) {
        Some(after.clone())
    } else {
        None
    };
    Ok(ResourceChange {
        address,
        kind,
        action,
        before,
        after: Some(after.clone()),
        requires_elevation: false,
        requires_force: false,
    })
}

enum KeyFileState {
    Missing,
    Match,
}

/// Probe one half of an SSH keypair. The path must either be absent or
/// be a regular file whose content is byte-identical to `expected`;
/// anything else (different content, non-regular file) is a hard error
/// rather than an `Update` opportunity, matching the "ensure presence,
/// never silently rotate" rule documented on [`classify_ssh_key`].
fn probe_ssh_key_file(path: &Path, expected: &[u8]) -> Result<KeyFileState> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(KeyFileState::Missing),
        Err(e) => Err(anyhow!("reading existing path `{}`: {e}", path.display())),
        Ok(meta) if meta.file_type().is_file() => {
            let existing = fs::read(path)
                .with_context(|| format!("reading existing ssh key `{}`", path.display()))?;
            if existing == expected {
                Ok(KeyFileState::Match)
            } else {
                bail!(
                    "refusing to rotate ssh key at `{}`; \
                     remove it manually if a new key is intended",
                    path.display(),
                );
            }
        }
        Ok(_) => bail!(
            "`{}` exists and is not a regular file; refusing to overwrite",
            path.display()
        ),
    }
}

/// Outcome of probing the user's keyring for a desired fingerprint.
///
/// Three states because we deliberately do *not* fail at plan time
/// when `gpg` itself is missing — the capability validator
/// (`crate::capability::validate_capabilities`) has already confirmed
/// that a Package in this plan will install `gpg` before the `GpgKey`
/// resource runs, so `GpgUnavailable` is the planning-time view of
/// "the keyring will be empty when we get there." See
/// [`classify_gpg_key`] for how each state maps to an `Action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GpgKeyringStatus {
    /// `gpg` is not on PATH at plan time. Capability validation owns
    /// the "is this fatal?" decision; the classifier just treats it as
    /// Create.
    GpgUnavailable,
    /// `gpg --batch --list-secret-keys <fpr>` exited 0 — the
    /// fingerprint is in the user's secret keyring.
    Present,
    /// `gpg` is available but the fingerprint is absent.
    Absent,
}

/// Probe the user's GPG secret keyring for `fingerprint`. Pure I/O —
/// no classification logic. Only `gpg`'s exit status is consulted
/// (`Stdio::null()` covers stdout and stderr) so the keyring's
/// contents never enter keron's memory (no shell-output exfiltration
/// channel).
///
/// Returns `GpgUnavailable` rather than an error when `gpg` is missing
/// from PATH. That lets the capability validator surface a
/// hint-bearing plan-time diagnostic instead of the bare
/// "`gpg` is not available on PATH" error this function used to
/// raise.
fn probe_gpg_keyring(fingerprint: &str) -> Result<GpgKeyringStatus> {
    if which::which("gpg").is_err() {
        return Ok(GpgKeyringStatus::GpgUnavailable);
    }
    let status = std::process::Command::new("gpg")
        .args([
            "--batch",
            "--list-secret-keys",
            "--with-colons",
            fingerprint,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("probing gpg keyring for fingerprint `{fingerprint}`"))?;
    Ok(if status.success() {
        GpgKeyringStatus::Present
    } else {
        GpgKeyringStatus::Absent
    })
}

/// Diff a desired GPG secret-key import against a probed keyring
/// status. Pure — no `which`, no subprocess — so it unit-tests
/// trivially.
///
/// State mapping:
///   - `Present` → `NoOp` (fingerprint already in the keyring).
///   - `Absent` → `Create`.
///   - `GpgUnavailable` → `Create`. The capability validator has
///     already confirmed a Package in this plan installs `gpg`, so the
///     keyring will be empty when the executor runs. The executor's
///     `gpg --import` is idempotent if the key turns out to be present
///     already (it prints "secret key unchanged" and exits 0).
fn classify_gpg_key(after: &ResourceState, status: GpgKeyringStatus) -> ResourceChange {
    let action = match status {
        GpgKeyringStatus::Present => Action::NoOp,
        GpgKeyringStatus::Absent | GpgKeyringStatus::GpgUnavailable => Action::Create,
    };
    let before = if matches!(action, Action::NoOp) {
        Some(after.clone())
    } else {
        None
    };
    ResourceChange {
        address: address_for(after),
        kind: ResourceKind::GpgKey,
        action,
        before,
        after: Some(after.clone()),
        requires_elevation: false,
        requires_force: false,
    }
}

fn address_for(state: &ResourceState) -> String {
    match state {
        ResourceState::Template { path, .. } => path.display().to_string(),
        ResourceState::Symlink { from, .. } => from.display().to_string(),
        ResourceState::Package { manager, name, .. } => {
            format!("{}:{}", manager.kind_label(), name)
        }
        ResourceState::Tap(spec) => format!("tap:{}", spec.user_tap),
        ResourceState::Shell { name, .. } => name.clone(),
        ResourceState::SshKey { private_path, .. } => private_path.display().to_string(),
        ResourceState::GpgKey { fingerprint, .. } => format!("gpg:{fingerprint}"),
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
            // NoOp arm compares against `fs::canonicalize(link)`, so
            // the test path must already be canonical (macOS rewrites
            // `/var/folders/...` -> `/private/var/folders/...`).
            let path = fs::canonicalize(&p).unwrap();
            // Strip the Windows verbatim-UNC prefix (`\\?\C:\...`).
            // Without this, `symlink_file(\\?\C:\…, link)` ends up
            // storing a target Windows then reads back without the
            // prefix, breaking `assert_eq!(read_link, original)`.
            #[cfg(windows)]
            let path = {
                let s = path.to_string_lossy();
                s.strip_prefix(r"\\?\")
                    .map_or_else(|| path.clone(), PathBuf::from)
            };
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
        let change = classify(&state, &mut PackageCache::for_tests()).unwrap();
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
        let err = classify(&desired(&a, &new_target), &mut PackageCache::for_tests())
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
}
