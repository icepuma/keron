//! `Plan` — the diffable, renderable description of what `apply` will
//! do. [`build_prechecked_plan`] runs the evaluator over a checked
//! module graph and classifies each produced resource into a
//! [`ResourceChange`].
//!
//! Resources are diffed against the live filesystem so the rendered
//! plan reflects what `keron apply --execute` will actually perform;
//! removals are intentionally out-of-scope until keron has managed
//! state proving ownership.

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
/// Test-only convenience that discards the precheck and returns just
/// the [`Plan`]. Production always goes through [`build_prechecked_plan`]
/// so the package-prerequisite gate runs.
#[cfg(test)]
fn build_plan(graph: &keron_modules::ModuleGraph, keron_root: &Path) -> Result<Plan> {
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

/// Whether a symlink's literal target equals the declared source.
///
/// The comparison is on the *literal* paths, but first normalizes the
/// platform path aliases that denote the same file through a *stable
/// OS-level* link — the macOS `/private` firmlink (`/var` ↔
/// `/private/var`) and the Windows `\\?\` verbatim prefix. `source` is
/// `canonicalize`-d at eval time (so it carries `/private` / `\\?\`)
/// while `fs::read_link` returns the raw bytes the link was created
/// with, which a hand-created link or the source's own non-canonical
/// form may lack — without this normalization an otherwise-identical
/// link would be re-pointed on every apply. Crucially this only folds
/// the fixed OS prefixes, *not* arbitrary intermediate user symlinks,
/// so a link pointing at a deletable deprecated path that merely
/// resolves to the same file is still (correctly) classified Update.
fn symlink_targets_equal(current: &Path, source: &Path) -> bool {
    current == source || normalize_link_path(current) == normalize_link_path(source)
}

fn normalize_link_path(p: &Path) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix("/private")
            && (rest.starts_with("/var") || rest.starts_with("/tmp") || rest.starts_with("/etc"))
        {
            return std::path::PathBuf::from(rest.to_string());
        }
    }
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return std::path::PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return std::path::PathBuf::from(rest);
        }
    }
    p.to_path_buf()
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
            // Compare the link's *literal* target to the declared source,
            // not filesystem identity. `is_same_file`/`canonicalize`
            // follow the link, so a link pointing at a *different* path
            // that merely resolves to the same inode (e.g. via a
            // deprecated intermediate symlink) would be reported up to
            // date while the declared layout silently drifts — and the
            // user only finds out when they delete the deprecated path
            // and the link breaks. A literal comparison also makes a
            // dangling link idempotent: it is exactly as declared even
            // though its target is currently absent. `read_link` returns
            // the immediate target without dereferencing, so a cyclic
            // link is classified, not an error.
            let current_literal = fs::read_link(target)
                .with_context(|| format!("reading existing symlink `{}`", target.display()))?;
            let action = if symlink_targets_equal(&current_literal, source) {
                Action::NoOp
            } else {
                Action::Update
            };
            let before = ResourceState::Symlink {
                from: target.to_path_buf(),
                to: current_literal,
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
/// True when `after` is a *sensitive* template but the live file at
/// `meta` carries any group/other permission bit — i.e. a secret is
/// currently world/group-readable and an Update is needed to clamp it
/// back to `0o600`. Always false on non-Unix (Windows has no mode bits
/// the executor manages) and for non-sensitive templates (whose mode is
/// intentionally preserved, not enforced).
fn sensitive_template_mode_violation(after: &ResourceState, meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let ResourceState::Template {
            sensitive: true, ..
        } = after
        {
            return (meta.mode() & 0o077) != 0;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (after, meta);
    }
    false
}

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
            // A *sensitive* template must be owner-only (`0o600`). If the
            // content already matches but the live file is group/world-
            // readable, the declared secret is silently exposed — classify
            // Update so the executor repairs the mode, even though no
            // content rewrite is strictly needed.
            let mode_violation = sensitive_template_mode_violation(after, &meta);
            let action = if same_content && !mode_violation {
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
#[path = "plan_tests.rs"]
mod tests;
