//! Capability signalling: every resource kind declares the capabilities
//! it `provides` to the rest of the plan and the ones it `needs` from
//! either an earlier provider or the live environment.
//!
//! [`validate_capabilities`] walks the plan in source order at plan
//! time and fails fast — with a typed, hint-bearing diagnostic — when
//! a resource's needs cannot be met. That promotes "the executor
//! crashed because `gpg` isn't on PATH" from an apply-time surprise to
//! a plan-time error that names both nodes and tells the user how to
//! fix it.
//!
//! The table is engine-internal. There is no user-facing
//! `requires` / `provides` syntax in `.keron` source; new resource
//! kinds register their signals here, alongside the planner.
//!
//! Two design rules keep the table lean:
//!   1. **Needs first.** Resources only register a `provides` entry
//!      for a capability that some other resource currently `needs`.
//!      In v1 the only `need` is `gpg_key` → `Binary("gpg")`, so the
//!      `well_known_binaries` table starts with a single row.
//!   2. **No synthesis.** v1 *validates* the DAG; it does not insert
//!      missing providers. Tap synthesis at `plan.rs:457` remains the
//!      one mechanism that does that, and predates this module.

use std::collections::HashSet;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::plan::{PackageManager, Plan, ResourceChange, ResourceState};

/// Wall-clock budget for a single password-manager session probe.
/// Tight enough that a stuck CLI doesn't keep `keron apply` hanging
/// indefinitely; loose enough to absorb a cold-start of the upstream
/// helper daemon. See [`probe_op_session`].
const SESSION_PROBE_BUDGET: Duration = Duration::from_secs(5);

/// Granularity of the `try_wait` poll loop inside the session probe.
/// At 50 ms a 5 s budget visits the kernel ~100 times — invisible cost
/// relative to a CLI process spawn, and tight enough that an
/// already-finished `op` doesn't sit waiting another full tick.
const SESSION_PROBE_POLL: Duration = Duration::from_millis(50);

/// A typed contract a resource can either `need` from some earlier
/// provider (or the live environment) or `provide` to later requirers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Capability {
    /// An executable available on `$PATH`. `name` is the bare binary
    /// (e.g. `"gpg"`). Satisfied by the env probe or by a `Package`
    /// resource whose `(manager, name)` pair is in
    /// [`well_known_binaries`].
    Binary(String),
    /// A Homebrew tap, `user/repo` form. Satisfied by a `Tap` resource
    /// earlier in the plan. Tap synthesis already inserts the
    /// providing `Tap` before its dependent package, so a `Tap` need
    /// reaching the env probe is unlikely in practice — kept for
    /// completeness as the model grows.
    Tap(String),
    /// A filesystem path that must exist when the requirer runs.
    /// Satisfied by the live filesystem or by an earlier
    /// `Template` / `Symlink` resource that writes the path.
    Path(PathBuf),
}

impl Capability {
    /// Human-readable name surfaced in diagnostics
    /// (e.g. `"the `gpg` binary"`).
    fn label(&self) -> String {
        match self {
            Self::Binary(name) => format!("the `{name}` binary"),
            Self::Tap(name) => format!("the homebrew tap `{name}`"),
            Self::Path(p) => format!("the path `{}`", p.display()),
        }
    }

    /// Phrase used to describe the "not satisfied externally" case in
    /// the missing-provider diagnostic.
    const fn not_satisfied_phrase(&self) -> &'static str {
        match self {
            Self::Binary(_) => "is not on PATH",
            Self::Tap(_) => "is not currently tapped",
            Self::Path(_) => "does not exist on disk",
        }
    }

    /// Per-capability hint text for the missing-provider diagnostic.
    /// Single source of truth so each variant owns its own help copy.
    fn hint(&self) -> String {
        match self {
            Self::Binary(name) if name == "gpg" => {
                "add a Package reconcile that installs it before \
                 this one, e.g.:\n        reconcile brew(\"gnupg\")       # macOS\n        \
                 reconcile apt(\"gnupg\")         # Debian/Ubuntu\n        \
                 reconcile pacman(\"gnupg\")      # Arch"
                    .into()
            }
            Self::Binary(name) => format!(
                "add a Package reconcile earlier in this plan that provides the `{name}` binary."
            ),
            Self::Tap(name) => format!(
                "declare `reconcile tap(\"{name}\")` earlier in this plan, \
                 or add a brew formula whose tap is `{name}`."
            ),
            Self::Path(p) => format!(
                "ensure `{}` exists before this resource needs it — write \
                 it via an earlier `template` / `symlink` reconcile, or \
                 create it out-of-band.",
                p.display(),
            ),
        }
    }
}

/// A system-wide gate that must hold before `apply` can do useful
/// work. Distinct from [`Capability`]: a prerequisite failure means
/// the manifest can't be applied *at all* (no brew → no brew-managed
/// reconcile can run); a `Capability` failure is a per-reconcile gap.
/// Diagnostics render the two tiers in separate sections so the
/// operator can tell "fix this first" from "this resource needs X."
///
/// `SecretCli` and `SecretSession` are deliberately distinct variants:
/// "the CLI isn't installed" wants an install link; "the CLI is here
/// but you're signed out" wants the signin command. Collapsing both
/// into a single variant — as the v1 of this code did — printed an
/// install URL to users who already had the CLI and a signin command
/// to users who didn't have anything to sign into.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Prerequisite {
    /// The package manager's CLI binary must be on `$PATH`. Brew gates
    /// every brew package and tap; Cargo gates every cargo install;
    /// Winget gates every winget install. Implied by any
    /// `ResourceState::Package` (and by `ResourceState::Tap`, which is
    /// always brew). `BrewCask` is normalized to `Brew` by
    /// [`prerequisites_for`] so a manifest mixing formula + cask only
    /// surfaces one "brew is not installed" diagnostic.
    PackageManager(PackageManager),
    /// A password-manager CLI binary is not on `$PATH`. Renders the
    /// install URL only — there's nothing to sign into yet.
    SecretCli(SessionKind),
    /// A password-manager CLI is installed but has no active session.
    /// Renders the signin command only — the user already has the
    /// binary, so the install URL would be noise.
    SecretSession(SessionKind),
}

/// Identifies which password-manager CLI a secret resolution call
/// reaches for. The `secret()` intrinsic dispatches on URI scheme;
/// this enum is the post-dispatch tag that flows into the prereq
/// check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionKind {
    OnePassword,
    // future: Bitwarden (bw://), Infisical (infisical://)
}

impl SessionKind {
    /// Display name for the CLI itself (e.g. `"1Password CLI"`). Used
    /// as the noun in both "X is not installed" and "X session not
    /// active" diagnostic forms.
    const fn cli_name(self) -> &'static str {
        match self {
            Self::OnePassword => "1Password CLI",
        }
    }

    /// The CLI command the user runs to enter a session — printed
    /// verbatim in the diagnostic so it can be copy-pasted.
    const fn signin_command(self) -> &'static str {
        match self {
            Self::OnePassword => "op signin",
        }
    }

    /// Bare binary name on `$PATH`. Used by the probe to decide
    /// whether the CLI is installed before the session check fires.
    const fn binary(self) -> &'static str {
        match self {
            Self::OnePassword => "op",
        }
    }

    /// Canonical upstream install/get-started URL. Surfaced only by
    /// the `SecretCli` variant, where the user needs to install the
    /// CLI before they can sign in.
    const fn install_url(self) -> &'static str {
        match self {
            Self::OnePassword => "https://developer.1password.com/docs/cli/get-started/",
        }
    }
}

/// Three-valued result of probing a password-manager session: richer
/// than a bool so the diagnostic can distinguish "install the CLI"
/// from "sign in to the CLI." The `LiveEnvProbe` implementation runs
/// a bounded `whoami`-style probe; mocks return the value directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// CLI present on `$PATH` and a session is active.
    Active,
    /// CLI present on `$PATH` but no active session — the user needs
    /// to sign in.
    NoSession,
    /// CLI binary is not on `$PATH`.
    NotInstalled,
}

impl Prerequisite {
    /// Fixed priority used to sort failures in the combined diagnostic.
    /// Brew (gates every package/tap reconcile) leads, then other
    /// package managers, then secret-CLI install (the user needs the
    /// CLI before they can sign in), then session-active.
    ///
    /// Lower number renders first.
    const fn priority(&self) -> u8 {
        match self {
            Self::PackageManager(PackageManager::Brew | PackageManager::BrewCask) => 0,
            Self::PackageManager(PackageManager::Cargo) => 1,
            Self::PackageManager(PackageManager::Winget) => 2,
            Self::SecretCli(_) => 3,
            Self::SecretSession(_) => 4,
        }
    }

    /// First diagnostic line: what failed.
    fn label(&self) -> String {
        match self {
            Self::PackageManager(pm) => format!("`{}` is not installed", pm.label()),
            Self::SecretCli(kind) => format!("{} is not installed", kind.cli_name()),
            Self::SecretSession(kind) => format!("{} session not active", kind.cli_name()),
        }
    }

    /// Canonical upstream install URL — *only* for foundational
    /// prereqs (brew, password-manager CLIs). Narrower entries (Cargo,
    /// Winget) and the session-inactive case return `None` and render
    /// without an install line. Per the "install links: foundational
    /// only" rule: brew gates the whole package surface; password-
    /// manager *CLIs* gate the whole secret surface; signed-out is a
    /// state, not a missing binary, so an install URL would be noise.
    const fn install_url(&self) -> Option<&'static str> {
        match self {
            Self::PackageManager(PackageManager::Brew | PackageManager::BrewCask) => {
                Some("https://brew.sh")
            }
            Self::SecretCli(kind) => Some(kind.install_url()),
            // Cargo/Winget gate apply just like brew but live with
            // rustup / Windows respectively — no single canonical
            // install URL to link to. SecretSession means the CLI is
            // present, so an install URL would be misleading.
            Self::PackageManager(PackageManager::Cargo | PackageManager::Winget)
            | Self::SecretSession(_) => None,
        }
    }

    /// Optional action line printed between the label and the install
    /// URL. Only the session-inactive case surfaces a signin command —
    /// the install-missing case has no session to sign into yet.
    fn action_line(&self) -> Option<String> {
        match self {
            Self::SecretSession(kind) => Some(format!("→ sign in: {}", kind.signin_command())),
            Self::SecretCli(_) | Self::PackageManager(_) => None,
        }
    }
}

/// Collected prerequisite failures, rendered as a single self-contained
/// diagnostic. The renderer leads with "prerequisites not met — apply
/// cannot proceed" so the operator sees the tier-1 framing distinct
/// from any later capability warnings.
///
/// `failures` is sorted by [`Prerequisite::priority`] when produced by
/// [`validate_prerequisites`] / [`prereq_report`] so the operator sees
/// brew-first ordering regardless of where the resources sit in the
/// manifest.
#[derive(Debug)]
pub struct PrereqReport {
    pub failures: Vec<Prerequisite>,
}

impl Display for PrereqReport {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "prerequisites not met — apply cannot proceed")?;
        writeln!(f)?;
        for prereq in &self.failures {
            writeln!(f, "  - {}", prereq.label())?;
            if let Some(action) = prereq.action_line() {
                writeln!(f, "      {action}")?;
            }
            if let Some(url) = prereq.install_url() {
                writeln!(f, "      → install: {url}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for PrereqReport {}

/// Probe the live environment for tier-1 prerequisites. Separate from
/// [`EnvProbe`] so the two tiers stay decoupled — a future change to
/// session detection (timeouts, caching) won't perturb the existing
/// capability probe surface, and vice versa.
pub trait PrereqProbe {
    /// True when the package manager's CLI binary is on `$PATH`.
    /// Brew/BrewCask share the `brew` binary, so this is `which("brew")`
    /// for both; Cargo and Winget have their own.
    fn package_manager_available(&self, pm: PackageManager) -> bool;
    /// Tri-state probe of the password-manager CLI: `Active`,
    /// `NoSession`, or `NotInstalled`. Implementations must never
    /// capture stdout into plan state (exit code only) and must
    /// always return within a bounded time — a stuck CLI should
    /// surface as `NoSession` rather than blocking the caller.
    fn session_state(&self, kind: SessionKind) -> SessionState;
}

impl PrereqProbe for LiveEnvProbe {
    fn package_manager_available(&self, pm: PackageManager) -> bool {
        let bin = pm.label();
        if which::which(bin).is_ok() {
            return true;
        }
        // Linuxbrew commonly installs at /home/linuxbrew/.linuxbrew/bin/brew
        // or ~/.linuxbrew/bin/brew, but the user often hasn't sourced
        // `brew shellenv` so `which` misses it. Probe the canonical
        // paths directly for Brew/BrewCask — without this, a Linux
        // user who has brew gets told to install it.
        if matches!(pm, PackageManager::Brew | PackageManager::BrewCask) {
            for candidate in linuxbrew_candidates() {
                if candidate.is_file() {
                    return true;
                }
            }
        }
        false
    }
    fn session_state(&self, kind: SessionKind) -> SessionState {
        if which::which(kind.binary()).is_err() {
            return SessionState::NotInstalled;
        }
        probe_session_state(kind)
    }
}

/// Probe the password-manager CLI's session state by running its
/// `whoami`-style subcommand with a hard wall-clock budget. Stdin is
/// pinned to `/dev/null` so any interactive prompt (biometric, expired
/// session) fails immediately on EOF instead of stealing the parent
/// terminal. Stdout/stderr are also nulled because the threat model
/// forbids capturing subprocess output into plan state — exit code is
/// the only signal that reaches the caller.
///
/// On timeout the child is killed and the result is `NoSession`: a
/// hung CLI is functionally equivalent to "you can't use it right
/// now" from the operator's perspective, and `NoSession` is the safer
/// failure mode (asks the user to re-sign-in; doesn't claim the CLI
/// is missing when it isn't).
#[cfg_attr(test, mutants::skip)]
fn probe_session_state(kind: SessionKind) -> SessionState {
    let mut cmd = std::process::Command::new(kind.binary());
    cmd.arg("whoami")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return SessionState::NoSession;
    };
    let deadline = Instant::now() + SESSION_PROBE_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    SessionState::Active
                } else {
                    SessionState::NoSession
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    // Reap the killed child so it doesn't linger as a
                    // zombie; ignore the resulting status.
                    let _ = child.wait();
                    return SessionState::NoSession;
                }
                std::thread::sleep(SESSION_PROBE_POLL);
            }
            Err(_) => return SessionState::NoSession,
        }
    }
}

/// Candidate paths a Linux user is likely to have brew installed at,
/// in priority order. Both are documented in the linuxbrew docs as
/// the canonical install locations; neither lands on a default `$PATH`
/// without the user running `eval "$(brew shellenv)"` first.
fn linuxbrew_candidates() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/home/linuxbrew/.linuxbrew/bin/brew")];
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".linuxbrew/bin/brew"));
    }
    out
}

/// Static prerequisites declared by a `ResourceState`. Companion to
/// [`signals_for`] for tier 2; this function answers tier 1. Returns
/// at most one prereq per state, so the return type is `Option` rather
/// than `Vec` — it never sprouts multi-element results in practice.
///
/// `BrewCask` is normalized to `Brew` because both managers share the
/// `brew` binary; a manifest mixing formula + cask should surface one
/// "brew is not installed" failure, not two.
pub const fn prerequisites_for(state: &ResourceState) -> Option<Prerequisite> {
    match state {
        ResourceState::Package { manager, .. } => {
            Some(Prerequisite::PackageManager(normalize_for_prereq(*manager)))
        }
        // Synthesized `Tap` resources are always brew; the executor
        // calls `brew tap`, so brew must be installed regardless of
        // whether any classify probe ran.
        ResourceState::Tap(_) => Some(Prerequisite::PackageManager(PackageManager::Brew)),
        ResourceState::Template { .. }
        | ResourceState::Symlink { .. }
        | ResourceState::Shell { .. }
        | ResourceState::SshKey { .. }
        | ResourceState::GpgKey { .. } => None,
    }
}

/// Dedup `BrewCask` onto `Brew` for prereq purposes: they probe the
/// same binary, and showing two diagnostics for one missing CLI is
/// noise. Kept as a tiny helper rather than inlined so the
/// normalization rule lives in one place.
const fn normalize_for_prereq(pm: PackageManager) -> PackageManager {
    match pm {
        PackageManager::BrewCask => PackageManager::Brew,
        other => other,
    }
}

/// Walk the eval'd resource states, collect every distinct
/// package-manager prereq, and probe each against the live
/// environment. Returns one combined report when anything fails —
/// operators see the full picture, sorted by priority (brew first),
/// not a per-call trickle.
///
/// Runs *before* the classify-time package probes so a missing brew
/// surfaces as "brew is not installed → <https://brew.sh>", not
/// `exited with status 127`.
///
/// **Session prereqs are checked elsewhere.** `SecretCli` /
/// `SecretSession` failures fire from inside `eval::resolve_secret`
/// the moment a `secret("op://…")` URI is evaluated; this function
/// doesn't double-check them. The split keeps secret-resolution
/// failures attributable to the manifest line that triggered them
/// (URI in scope) while still routing through the same `PrereqReport`
/// shape.
///
/// **Once-per-kind guarantee.** Each distinct `Prerequisite` is
/// probed at most once per call: 50 brew packages still produce one
/// `package_manager_available(Brew)` probe, not 50.
pub fn validate_prerequisites(
    states: &[ResourceState],
    probe: &dyn PrereqProbe,
) -> std::result::Result<(), PrereqReport> {
    let mut failures: Vec<Prerequisite> = Vec::new();
    let mut seen: HashSet<Prerequisite> = HashSet::new();

    for state in states {
        if let Some(prereq) = prerequisites_for(state) {
            // `seen.insert` returns true iff the prereq is new; short-
            // circuiting `&&` keeps the probe call inside that branch,
            // so each distinct prereq is probed exactly once.
            if seen.insert(prereq.clone()) && !prereq_satisfied(&prereq, probe) {
                failures.push(prereq);
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        failures.sort_by_key(Prerequisite::priority);
        Err(PrereqReport { failures })
    }
}

fn prereq_satisfied(prereq: &Prerequisite, probe: &dyn PrereqProbe) -> bool {
    match prereq {
        Prerequisite::PackageManager(pm) => probe.package_manager_available(*pm),
        // `validate_prerequisites` only ever has package-manager
        // prereqs in its `failures` set today (session prereqs land
        // here only via `prereq_report`), but the match must be total.
        // Trusting the probe's tri-state result keeps the trait honest.
        Prerequisite::SecretCli(kind) => {
            !matches!(probe.session_state(*kind), SessionState::NotInstalled)
        }
        Prerequisite::SecretSession(kind) => {
            matches!(probe.session_state(*kind), SessionState::Active)
        }
    }
}

/// Build a single-failure [`PrereqReport`] for a callsite that already
/// knows which prereq is missing (e.g. `eval::resolve_secret` after a
/// `SessionState::NotInstalled` probe). Callers wrap the report in
/// `anyhow::Error` as needed.
#[must_use]
pub fn prereq_report(prereq: Prerequisite) -> PrereqReport {
    PrereqReport {
        failures: vec![prereq],
    }
}

/// What a resource contributes (`provides`) and depends on (`needs`).
/// Computed per `ResourceChange` by [`signals_for`].
#[derive(Debug, Default)]
pub struct Signals {
    pub provides: Vec<Capability>,
    pub needs: Vec<Capability>,
}

/// Static signals declared for a `ResourceChange`.
///
/// Pure: it inspects only `change.after` (the desired state) and
/// returns the capabilities the kind has registered. New kinds declare
/// their signals by extending the match below.
pub fn signals_for(change: &ResourceChange) -> Signals {
    let mut s = Signals::default();
    let Some(after) = change.after.as_ref() else {
        return s;
    };
    match after {
        ResourceState::Package { manager, name, .. } => {
            for bin in well_known_binaries(*manager, name) {
                s.provides.push(Capability::Binary((*bin).into()));
            }
        }
        ResourceState::Tap(spec) => {
            s.provides.push(Capability::Tap(spec.user_tap.clone()));
        }
        ResourceState::Template { path, .. } => {
            s.provides.push(Capability::Path(path.clone()));
        }
        ResourceState::Symlink { from, .. } => {
            s.provides.push(Capability::Path(from.clone()));
        }
        ResourceState::SshKey {
            private_path,
            public_path,
            ..
        } => {
            s.provides.push(Capability::Path(private_path.clone()));
            s.provides.push(Capability::Path(public_path.clone()));
        }
        ResourceState::GpgKey { .. } => {
            s.needs.push(Capability::Binary("gpg".into()));
        }
        ResourceState::Shell { .. } => {
            // No declared signals yet. A future per-script needs list
            // (e.g. parsed from a header comment) would land here.
        }
    }
    s
}

/// Map `(package manager, formula name)` → binaries the package is
/// known to install. Intentionally narrow: only entries that some
/// resource currently `needs` belong here.
///
/// Manager is ignored today because the single registered entry
/// (`gnupg | gnupg2 → gpg`) ships the same binary across every
/// package manager that uses those names. The signature keeps the
/// argument so future entries can vary by manager without a
/// caller-side refactor.
const fn well_known_binaries(_manager: PackageManager, name: &str) -> &'static [&'static str] {
    match name.as_bytes() {
        b"gnupg" | b"gnupg2" => &["gpg"],
        _ => &[],
    }
}

/// Probe the live environment for capabilities the plan can't yet
/// provide on its own. The trait is the test seam: production wires
/// [`LiveEnvProbe`]; tests pass a deterministic mock so validation can
/// be exercised without touching `$PATH` or the filesystem.
pub trait EnvProbe {
    fn has_binary(&self, name: &str) -> bool;
    fn path_exists(&self, path: &Path) -> bool;
    /// `false` is a safe default: in-plan `Tap` resources are the
    /// canonical providers (Tap synthesis inserts them ahead of their
    /// dependents), so the env probe never has to know.
    fn tap_installed(&self, tap: &str) -> bool;
}

/// Production [`EnvProbe`]: `which::which` for binaries, `Path::exists`
/// for paths. Tap probing is conservative (`false`) — the in-plan
/// `Tap` resources synthesised at `plan.rs:457` are the real
/// providers.
pub struct LiveEnvProbe;

impl EnvProbe for LiveEnvProbe {
    fn has_binary(&self, name: &str) -> bool {
        which::which(name).is_ok()
    }
    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn tap_installed(&self, _tap: &str) -> bool {
        false
    }
}

/// Walk the plan in source order and ensure every declared `need` has
/// a satisfying provider — in the live environment, or earlier in the
/// plan. Out-of-order or missing providers fail with a hint-bearing
/// diagnostic.
pub fn validate_capabilities(plan: &Plan, env: &dyn EnvProbe) -> Result<()> {
    for (idx, change) in plan.changes.iter().enumerate() {
        let signals = signals_for(change);
        for need in &signals.needs {
            check_need(need, change, &plan.changes, idx, env)?;
        }
    }
    Ok(())
}

fn check_need(
    need: &Capability,
    requirer: &ResourceChange,
    all_changes: &[ResourceChange],
    requirer_idx: usize,
    env: &dyn EnvProbe,
) -> Result<()> {
    if env_satisfies(need, env) {
        return Ok(());
    }
    for predecessor in &all_changes[..requirer_idx] {
        if signals_for(predecessor).provides.iter().any(|c| c == need) {
            return Ok(());
        }
    }
    // Provider declared after the requirer? Surface a sharper hint
    // that names both nodes instead of the generic "missing" message.
    for change in all_changes.iter().skip(requirer_idx + 1) {
        if signals_for(change).provides.iter().any(|c| c == need) {
            bail!(
                "`{}` requires {}, but the providing `{}` is declared *after* it in this plan\n  \
                 hint: reconciles run in source order; move the provider above the requirer, \
                 or move the requirer below the provider.",
                requirer.address,
                need.label(),
                change.address,
            );
        }
    }
    bail!(
        "`{}` requires {}, which {} and is not produced by any earlier reconcile in this plan\n  \
         hint: {}",
        requirer.address,
        need.label(),
        need.not_satisfied_phrase(),
        need.hint(),
    );
}

fn env_satisfies(need: &Capability, env: &dyn EnvProbe) -> bool {
    match need {
        Capability::Binary(name) => env.has_binary(name),
        Capability::Path(p) => env.path_exists(p),
        Capability::Tap(name) => env.tap_installed(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Action, ResourceKind, ShellKind, TapSpec};

    #[derive(Default)]
    struct MockEnvProbe {
        binaries: Vec<String>,
        paths: Vec<PathBuf>,
        taps: Vec<String>,
        package_managers: Vec<PackageManager>,
    }

    impl MockEnvProbe {
        fn with_binary(mut self, name: &str) -> Self {
            self.binaries.push(name.into());
            self
        }
        fn with_package_manager(mut self, pm: PackageManager) -> Self {
            self.package_managers.push(pm);
            self
        }
    }

    impl EnvProbe for MockEnvProbe {
        fn has_binary(&self, name: &str) -> bool {
            self.binaries.iter().any(|n| n == name)
        }
        fn path_exists(&self, p: &Path) -> bool {
            self.paths.iter().any(|x| x == p)
        }
        fn tap_installed(&self, t: &str) -> bool {
            self.taps.iter().any(|x| x == t)
        }
    }

    impl PrereqProbe for MockEnvProbe {
        fn package_manager_available(&self, pm: PackageManager) -> bool {
            self.package_managers.contains(&pm)
        }
        fn session_state(&self, _kind: SessionKind) -> SessionState {
            // capability-tier tests don't exercise session prereqs
            // (eval-tier tests in eval.rs do). Conservative default:
            // pretend nothing is installed so an unexpected call
            // surfaces.
            SessionState::NotInstalled
        }
    }

    fn make_change(kind: ResourceKind, address: &str, after: ResourceState) -> ResourceChange {
        ResourceChange {
            address: address.into(),
            kind,
            action: Action::Create,
            before: None,
            after: Some(after),
            requires_elevation: false,
            requires_force: false,
        }
    }

    fn brew_package(name: &str) -> ResourceChange {
        make_change(
            ResourceKind::Package,
            &format!("brew:{name}"),
            ResourceState::Package {
                manager: PackageManager::Brew,
                name: name.into(),
                tap: None,
            },
        )
    }

    fn cask_package(name: &str) -> ResourceChange {
        make_change(
            ResourceKind::Package,
            &format!("cask:{name}"),
            ResourceState::Package {
                manager: PackageManager::BrewCask,
                name: name.into(),
                tap: None,
            },
        )
    }

    fn gpg_key_change(fingerprint: &str) -> ResourceChange {
        make_change(
            ResourceKind::GpgKey,
            &format!("gpg:{fingerprint}"),
            ResourceState::GpgKey {
                fingerprint: fingerprint.into(),
                key: String::new(),
            },
        )
    }

    fn template_change(path: &Path) -> ResourceChange {
        make_change(
            ResourceKind::Template,
            &path.display().to_string(),
            ResourceState::Template {
                path: path.to_path_buf(),
                content: "body".into(),
                sensitive: false,
            },
        )
    }

    fn symlink_change(from: &Path, to: &Path) -> ResourceChange {
        make_change(
            ResourceKind::Symlink,
            &from.display().to_string(),
            ResourceState::Symlink {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
            },
        )
    }

    fn ssh_key_change(priv_path: &Path, pub_path: &Path) -> ResourceChange {
        make_change(
            ResourceKind::SshKey,
            &priv_path.display().to_string(),
            ResourceState::SshKey {
                private_path: priv_path.to_path_buf(),
                public_path: pub_path.to_path_buf(),
                private_key: "PRIV".into(),
                public_key: "ssh-ed25519 AAAA host".into(),
            },
        )
    }

    fn shell_change(name: &str) -> ResourceChange {
        make_change(
            ResourceKind::Shell,
            name,
            ResourceState::Shell {
                kind: ShellKind::Sh,
                name: name.into(),
                cwd: PathBuf::from("/tmp"),
                script: "true".into(),
                sensitive: false,
            },
        )
    }

    fn tap_change(user_tap: &str) -> ResourceChange {
        make_change(
            ResourceKind::Tap,
            &format!("tap:{user_tap}"),
            ResourceState::Tap(TapSpec {
                user_tap: user_tap.into(),
                url: None,
            }),
        )
    }

    #[test]
    fn signals_for_emits_binary_provides_for_gnupg_package() {
        let change = brew_package("gnupg");
        let signals = signals_for(&change);
        assert_eq!(signals.provides, vec![Capability::Binary("gpg".into())]);
        assert!(signals.needs.is_empty());
    }

    #[test]
    fn signals_for_emits_binary_provides_for_gnupg2_package() {
        let change = brew_package("gnupg2");
        let signals = signals_for(&change);
        assert_eq!(signals.provides, vec![Capability::Binary("gpg".into())]);
    }

    #[test]
    fn signals_for_emits_no_binary_provides_for_unrelated_package() {
        let change = brew_package("ripgrep");
        let signals = signals_for(&change);
        assert!(signals.provides.is_empty(), "table should be narrow");
    }

    #[test]
    fn signals_for_emits_path_provides_for_template() {
        let path = PathBuf::from("/tmp/keron-capability-template");
        let change = template_change(&path);
        let signals = signals_for(&change);
        assert_eq!(signals.provides, vec![Capability::Path(path)]);
    }

    #[test]
    fn signals_for_emits_path_provides_for_symlink_from() {
        let from = PathBuf::from("/tmp/keron-capability-symlink");
        let to = PathBuf::from("/tmp/keron-capability-target");
        let change = symlink_change(&from, &to);
        let signals = signals_for(&change);
        assert_eq!(signals.provides, vec![Capability::Path(from)]);
    }

    #[test]
    fn signals_for_emits_paths_for_ssh_key_pair() {
        let priv_path = PathBuf::from("/tmp/keron-capability-id");
        let pub_path = PathBuf::from("/tmp/keron-capability-id.pub");
        let change = ssh_key_change(&priv_path, &pub_path);
        let signals = signals_for(&change);
        assert_eq!(
            signals.provides,
            vec![Capability::Path(priv_path), Capability::Path(pub_path),],
        );
    }

    #[test]
    fn signals_for_emits_tap_provides() {
        let change = tap_change("icepuma/nanite");
        let signals = signals_for(&change);
        assert_eq!(
            signals.provides,
            vec![Capability::Tap("icepuma/nanite".into())],
        );
    }

    #[test]
    fn signals_for_emits_binary_need_for_gpg_key() {
        let change = gpg_key_change("ABCD1234");
        let signals = signals_for(&change);
        assert_eq!(signals.needs, vec![Capability::Binary("gpg".into())]);
        assert!(signals.provides.is_empty());
    }

    #[test]
    fn signals_for_empty_for_cask_and_shell() {
        let cask = signals_for(&cask_package("ghostty"));
        assert!(cask.provides.is_empty() && cask.needs.is_empty());
        let shell = signals_for(&shell_change("refresh"));
        assert!(shell.provides.is_empty() && shell.needs.is_empty());
    }

    #[test]
    fn validate_passes_when_binary_on_path() {
        let plan = Plan {
            changes: vec![gpg_key_change("ABCD1234")],
        };
        let env = MockEnvProbe::default().with_binary("gpg");
        validate_capabilities(&plan, &env).expect("env-provided gpg should satisfy the need");
    }

    #[test]
    fn validate_passes_when_predecessor_package_provides() {
        let plan = Plan {
            changes: vec![brew_package("gnupg"), gpg_key_change("ABCD1234")],
        };
        let env = MockEnvProbe::default();
        validate_capabilities(&plan, &env)
            .expect("in-plan provider should satisfy the need even with no gpg on PATH");
    }

    #[test]
    fn validate_passes_for_gnupg2_predecessor() {
        let plan = Plan {
            changes: vec![brew_package("gnupg2"), gpg_key_change("ABCD1234")],
        };
        let env = MockEnvProbe::default();
        validate_capabilities(&plan, &env).expect("gnupg2 should also provide gpg");
    }

    #[test]
    fn validate_fails_provider_after_requirer() {
        let plan = Plan {
            changes: vec![gpg_key_change("ABCD1234"), brew_package("gnupg")],
        };
        let env = MockEnvProbe::default();
        let err = validate_capabilities(&plan, &env)
            .expect_err("provider-after must surface at plan time");
        let msg = format!("{err:#}");
        assert!(msg.contains("declared *after*"), "got: {msg}");
        assert!(
            msg.contains("brew:gnupg"),
            "diagnostic should name the provider: {msg}"
        );
        assert!(
            msg.contains("gpg:ABCD1234"),
            "diagnostic should name the requirer: {msg}"
        );
    }

    #[test]
    fn validate_fails_no_provider() {
        let plan = Plan {
            changes: vec![gpg_key_change("ABCD1234")],
        };
        let env = MockEnvProbe::default();
        let err = validate_capabilities(&plan, &env)
            .expect_err("missing provider must surface at plan time");
        let msg = format!("{err:#}");
        assert!(msg.contains("is not on PATH"), "phrase missing: {msg}");
        assert!(msg.contains("brew(\"gnupg\")"), "macOS hint missing: {msg}");
        assert!(msg.contains("apt(\"gnupg\")"), "linux hint missing: {msg}");
        assert!(
            msg.contains("pacman(\"gnupg\")"),
            "arch hint missing: {msg}"
        );
    }

    #[test]
    fn check_need_path_satisfied_by_predecessor_template() {
        // No live ResourceState yet declares a Path need, but the
        // mechanism must already work for future kinds. Drive
        // check_need directly with a fabricated Path need and prove
        // the predecessor scan finds the providing Template.
        let path = PathBuf::from("/tmp/keron-capability-path-need");
        let changes = vec![template_change(&path), shell_change("after-template")];
        let need = Capability::Path(path);
        let env = MockEnvProbe::default();
        check_need(&need, &changes[1], &changes, 1, &env)
            .expect("predecessor template should satisfy a Path need");
    }

    // ---------------------------------------------------------------
    // Tier-1 prerequisites
    // ---------------------------------------------------------------

    fn brew_pkg_state(name: &str) -> ResourceState {
        ResourceState::Package {
            manager: PackageManager::Brew,
            name: name.into(),
            tap: None,
        }
    }

    fn cask_pkg_state(name: &str) -> ResourceState {
        ResourceState::Package {
            manager: PackageManager::BrewCask,
            name: name.into(),
            tap: None,
        }
    }

    fn cargo_pkg_state(name: &str) -> ResourceState {
        ResourceState::Package {
            manager: PackageManager::Cargo,
            name: name.into(),
            tap: None,
        }
    }

    fn tap_state(user_tap: &str) -> ResourceState {
        ResourceState::Tap(TapSpec {
            user_tap: user_tap.into(),
            url: None,
        })
    }

    #[test]
    fn prerequisites_for_brew_package() {
        assert_eq!(
            prerequisites_for(&brew_pkg_state("ripgrep")),
            Some(Prerequisite::PackageManager(PackageManager::Brew)),
        );
    }

    #[test]
    fn prerequisites_for_cask_package_normalizes_to_brew() {
        // BrewCask shares the `brew` binary; dedup'ing the prereq to
        // `Brew` keeps a mixed formula+cask manifest from printing two
        // "brew not installed" diagnostics.
        assert_eq!(
            prerequisites_for(&cask_pkg_state("ghostty")),
            Some(Prerequisite::PackageManager(PackageManager::Brew)),
        );
    }

    #[test]
    fn prerequisites_for_cargo_package() {
        assert_eq!(
            prerequisites_for(&cargo_pkg_state("cargo-nextest")),
            Some(Prerequisite::PackageManager(PackageManager::Cargo)),
        );
    }

    #[test]
    fn prerequisites_for_tap_implies_brew() {
        assert_eq!(
            prerequisites_for(&tap_state("icepuma/nanite")),
            Some(Prerequisite::PackageManager(PackageManager::Brew)),
        );
    }

    #[test]
    fn prerequisites_for_other_states_are_none() {
        let template = ResourceState::Template {
            path: PathBuf::from("/tmp/keron-prereq-template"),
            content: "x".into(),
            sensitive: false,
        };
        assert!(prerequisites_for(&template).is_none());
    }

    #[test]
    fn validate_prereqs_passes_when_brew_present() {
        let states = vec![brew_pkg_state("ripgrep")];
        let probe = MockEnvProbe::default().with_package_manager(PackageManager::Brew);
        validate_prerequisites(&states, &probe).expect("brew present should satisfy the prereq");
    }

    #[test]
    fn validate_prereqs_fails_when_brew_missing() {
        let states = vec![brew_pkg_state("ripgrep")];
        let probe = MockEnvProbe::default();
        let report = validate_prerequisites(&states, &probe)
            .expect_err("missing brew must surface as a prereq error");
        // Typed access: no string-grepping needed.
        assert_eq!(
            report.failures,
            vec![Prerequisite::PackageManager(PackageManager::Brew)],
        );
        let msg = format!("{report}");
        assert!(
            msg.contains("prerequisites not met"),
            "diagnostic should lead with tier-1 framing: {msg}"
        );
        assert!(
            msg.contains("`brew` is not installed"),
            "diagnostic should name the missing prereq: {msg}"
        );
        assert!(
            msg.contains("https://brew.sh"),
            "brew failure must include install URL: {msg}"
        );
    }

    #[test]
    fn validate_prereqs_dedups_brew_and_cask_into_one_failure() {
        let states = vec![brew_pkg_state("ripgrep"), cask_pkg_state("ghostty")];
        let probe = MockEnvProbe::default();
        let report = validate_prerequisites(&states, &probe).expect_err("missing brew");
        assert_eq!(
            report.failures,
            vec![Prerequisite::PackageManager(PackageManager::Brew)],
            "brew and cask share the binary; only one failure should surface"
        );
    }

    #[test]
    fn validate_prereqs_renders_no_url_for_cargo() {
        let states = vec![cargo_pkg_state("cargo-nextest")];
        let probe = MockEnvProbe::default();
        let report = validate_prerequisites(&states, &probe)
            .expect_err("missing cargo must still surface as a prereq error");
        let msg = format!("{report}");
        assert!(msg.contains("`cargo` is not installed"), "got: {msg}");
        assert!(
            !msg.contains("→ install:"),
            "narrow prereqs render without an install line: {msg}"
        );
    }

    #[test]
    fn validate_prereqs_sorts_failures_by_priority() {
        // Source order: Cargo first, then Brew. Priority order:
        // Brew first (foundational), then Cargo. The renderer must
        // surface Brew above Cargo regardless of manifest layout.
        let states = vec![cargo_pkg_state("cargo-nextest"), brew_pkg_state("ripgrep")];
        let probe = MockEnvProbe::default();
        let report = validate_prerequisites(&states, &probe).expect_err("both missing");
        assert_eq!(
            report.failures,
            vec![
                Prerequisite::PackageManager(PackageManager::Brew),
                Prerequisite::PackageManager(PackageManager::Cargo),
            ],
            "failures must sort by priority, not source order"
        );
    }

    #[test]
    fn secret_cli_prereq_renders_install_url_only() {
        let report = prereq_report(Prerequisite::SecretCli(SessionKind::OnePassword));
        let msg = format!("{report}");
        assert!(msg.contains("1Password CLI is not installed"), "got: {msg}");
        assert!(
            msg.contains("https://developer.1password.com/docs/cli/get-started/"),
            "SecretCli must include install URL: {msg}"
        );
        assert!(
            !msg.contains("op signin"),
            "SecretCli must NOT prompt for signin — there's no CLI to sign into yet: {msg}"
        );
    }

    #[test]
    fn secret_session_prereq_renders_signin_only() {
        let report = prereq_report(Prerequisite::SecretSession(SessionKind::OnePassword));
        let msg = format!("{report}");
        assert!(
            msg.contains("1Password CLI session not active"),
            "got: {msg}"
        );
        assert!(
            msg.contains("op signin"),
            "SecretSession must surface the signin command: {msg}"
        );
        assert!(
            !msg.contains("→ install:"),
            "SecretSession must NOT show install URL — the CLI is already present: {msg}"
        );
    }

    /// Probe that counts how many times each prereq is asked about,
    /// so we can pin the "once per kind regardless of resource count"
    /// guarantee called out in `validate_prerequisites`' docs.
    #[derive(Default)]
    struct CountingProbe {
        pm_calls: std::cell::Cell<usize>,
        session_calls: std::cell::Cell<usize>,
    }

    impl PrereqProbe for CountingProbe {
        fn package_manager_available(&self, _pm: PackageManager) -> bool {
            self.pm_calls.set(self.pm_calls.get() + 1);
            true
        }
        fn session_state(&self, _kind: SessionKind) -> SessionState {
            self.session_calls.set(self.session_calls.get() + 1);
            SessionState::Active
        }
    }

    #[test]
    fn validate_prereqs_probes_each_kind_at_most_once() {
        // Fifty brew packages must collapse to one package-manager
        // probe — anything else means we'd shell out N times in
        // production for an N-package manifest.
        let states: Vec<ResourceState> = (0..50)
            .map(|i| brew_pkg_state(&format!("pkg-{i}")))
            .collect();
        let probe = CountingProbe::default();
        validate_prerequisites(&states, &probe).expect("all prereqs satisfied by counting probe");
        assert_eq!(
            probe.pm_calls.get(),
            1,
            "brew probed once across 50 packages"
        );
        assert_eq!(
            probe.session_calls.get(),
            0,
            "no session prereqs in this manifest"
        );
    }

    #[test]
    fn prereq_report_renders_single_failure() {
        let report = prereq_report(Prerequisite::PackageManager(PackageManager::Brew));
        let msg = format!("{report}");
        assert!(msg.contains("prerequisites not met"));
        assert!(msg.contains("`brew` is not installed"));
        assert!(msg.contains("https://brew.sh"));
    }
}
