//! Package manager integration: list installed packages per
//! manager, cache the result, and install / upgrade missing or
//! outdated ones.
//!
//! The cache lives for the duration of one `keron apply` and is
//! populated lazily — the first time a `brew(...)` resource is
//! classified, we shell out to `brew list` and remember the result;
//! later `brew(...)` resources reuse that snapshot without
//! re-querying. The cost is one shell-out per (manager, query) that
//! appears in the manifest, not one per resource.
//!
//! Idempotency: [`PackageCache::classify_package`] returns the
//! `Action` to apply (Create / `NoOp` / Update). For brew/cask resources
//! it consults both the "installed" set (by *bare* name — `brew list`
//! reports tap-installed formulae without the tap prefix) and the
//! "outdated" set (by *qualified* name) so an outdated tap-qualified
//! formula upgrades correctly. The classifier also records each name
//! it returned Create for, so two `brew("ripgrep")` resources in the
//! same plan classify as Create / `NoOp` rather than Create / Create.
//!
//! Test seam: each manager's fetch reads `KERON_TEST_<MGR>_PACKAGES`
//! (comma-separated) instead of shelling out, so unit tests can
//! drive any cache state without a real `brew` / `cargo` / `winget`
//! on the host. Brew has additional seams for casks
//! (`KERON_TEST_BREW_CASK_PACKAGES`), outdated formulae / casks
//! (`KERON_TEST_BREW_OUTDATED`, `KERON_TEST_BREW_CASK_OUTDATED`),
//! installed taps (`KERON_TEST_BREW_TAPS`), and per-tap remote URLs
//! (`KERON_TEST_BREW_TAP_REMOTES=user/repo=URL;user2/repo2=URL2`).
//! The install side reads `KERON_TEST_PACKAGE_BIN_<MGR>` to swap the
//! binary path for a spy script. All seams require
//! `KERON_ALLOW_TEST_OVERRIDES=1` so stray environment variables
//! cannot falsify a real run.

pub mod brew;
pub mod cargo;
pub mod winget;

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::plan::{Action, PackageManager, TapSpec};

/// Snapshot of "what's installed / outdated / tapped" per query.
/// Populated lazily; one `PackageCache` per `keron apply` invocation.
#[derive(Debug, Default)]
pub struct PackageCache {
    /// Bare names of installed packages, keyed by manager. For
    /// `Brew` this is `brew list --formula -1`; for `BrewCask`,
    /// `brew list --cask -1`; for cargo / winget, the manager's own
    /// list command. Loaded lazily on first access per manager.
    installed: HashMap<PackageManager, HashSet<String>>,
    /// Qualified names (`user/tap/formula` or bare formula) of
    /// outdated brew/cask packages. Loaded lazily on first
    /// `classify_package` call for the relevant manager. Empty for
    /// cargo / winget — their outdated semantics differ enough to
    /// warrant separate work.
    outdated: HashMap<PackageManager, HashSet<String>>,
    /// `user/repo` strings from `brew tap`. Loaded lazily on first
    /// `classify_tap` call.
    installed_taps: Option<HashSet<String>>,
    /// Per-tap remote URL memo, populated on demand via
    /// `brew tap-info --json`. Only consulted when a tap is already
    /// installed AND the manifest declared a custom URL.
    tap_remotes: HashMap<String, Option<String>>,
    /// Names already classified as Create in this run — second
    /// occurrence collapses to `NoOp`.
    scheduled: HashMap<PackageManager, HashSet<String>>,
    /// `user/tap` already classified as Create in this run.
    scheduled_taps: HashSet<String>,
}

impl PackageCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Classify a package resource against the live state.
    ///
    /// Returns the action the planner should record:
    ///   - `Update` — name is installed AND its qualified form is in
    ///     the outdated set.
    ///   - `NoOp` — name is installed (and not outdated), OR another
    ///     resource in this same plan already claimed a Create for it.
    ///   - `Create` — name isn't installed yet.
    ///
    /// "Installed" is compared by the *bare* tail of the qualified
    /// name (because `brew list` reports tap-installed formulae by
    /// bare name); "outdated" by the full qualified name (matching
    /// `brew outdated --quiet` output).
    ///
    /// # Errors
    /// Errors when the underlying probes fail on first access. Cached
    /// snapshots on subsequent calls don't re-probe.
    pub fn classify_package(&mut self, manager: PackageManager, name: &str) -> Result<Action> {
        let bare = bare_name(name);
        self.ensure_installed_loaded(manager)?;
        let installed = self
            .installed
            .get(&manager)
            .expect("just loaded above")
            .contains(bare);
        if !installed {
            // Not installed — short-circuit. No need to consult the
            // outdated probe (saves a shell-out when nothing on the
            // host is tapped yet).
            let scheduled = self.scheduled.entry(manager).or_default();
            return Ok(if scheduled.contains(name) {
                Action::NoOp
            } else {
                scheduled.insert(name.to_string());
                Action::Create
            });
        }
        if !manager_uses_outdated_probe(manager) {
            return Ok(Action::NoOp);
        }
        self.ensure_outdated_loaded(manager)?;
        let outdated = self
            .outdated
            .get(&manager)
            .expect("just loaded above")
            .contains(name);
        Ok(if outdated {
            Action::Update
        } else {
            Action::NoOp
        })
    }

    /// Classify a tap registration against the live state.
    ///
    ///   - `Update` — tap is installed but its remote URL differs
    ///     from the requested one. Only checked when the manifest
    ///     declared a custom URL.
    ///   - `NoOp` — tap is installed (URL matches, OR no URL was
    ///     declared so any remote is acceptable), OR another tap
    ///     resource in this plan already claimed a Create.
    ///   - `Create` — tap isn't installed yet.
    pub fn classify_tap(&mut self, spec: &TapSpec) -> Result<Action> {
        self.ensure_taps_loaded()?;
        let installed = self
            .installed_taps
            .as_ref()
            .expect("just loaded above")
            .contains(&spec.user_tap);
        if !installed {
            if self.scheduled_taps.contains(&spec.user_tap) {
                return Ok(Action::NoOp);
            }
            self.scheduled_taps.insert(spec.user_tap.clone());
            return Ok(Action::Create);
        }
        let Some(want_url) = spec.url.as_deref() else {
            return Ok(Action::NoOp);
        };
        let actual = self.tap_remote(&spec.user_tap)?;
        if actual.as_deref() == Some(want_url) {
            Ok(Action::NoOp)
        } else {
            Ok(Action::Update)
        }
    }

    fn ensure_installed_loaded(&mut self, manager: PackageManager) -> Result<()> {
        use std::collections::hash_map::Entry;
        match self.installed.entry(manager) {
            Entry::Occupied(_) => Ok(()),
            Entry::Vacant(e) => {
                let set = fetch_installed(manager).with_context(|| {
                    format!(
                        "listing installed packages for {} (`{} list ...`)",
                        manager.kind_label(),
                        manager.label()
                    )
                })?;
                e.insert(set);
                Ok(())
            }
        }
    }

    fn ensure_outdated_loaded(&mut self, manager: PackageManager) -> Result<()> {
        use std::collections::hash_map::Entry;
        match self.outdated.entry(manager) {
            Entry::Occupied(_) => Ok(()),
            Entry::Vacant(e) => {
                let set = fetch_outdated(manager).with_context(|| {
                    format!(
                        "listing outdated packages for {} (`{} outdated ...`)",
                        manager.kind_label(),
                        manager.label()
                    )
                })?;
                e.insert(set);
                Ok(())
            }
        }
    }

    fn ensure_taps_loaded(&mut self) -> Result<()> {
        if self.installed_taps.is_some() {
            return Ok(());
        }
        let set = fetch_taps().context("listing installed brew taps (`brew tap`)")?;
        self.installed_taps = Some(set);
        Ok(())
    }

    fn tap_remote(&mut self, user_tap: &str) -> Result<Option<String>> {
        if let Some(cached) = self.tap_remotes.get(user_tap) {
            return Ok(cached.clone());
        }
        let remote = fetch_tap_remote(user_tap).with_context(|| {
            format!("reading remote URL for tap `{user_tap}` (`brew tap-info --json`)")
        })?;
        self.tap_remotes
            .insert(user_tap.to_string(), remote.clone());
        Ok(remote)
    }
}

/// Strip any `user/tap/` prefix from a manifest name, leaving the
/// bare formula/cask name brew uses when reporting it in `brew list`.
fn bare_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

const fn manager_uses_outdated_probe(manager: PackageManager) -> bool {
    matches!(manager, PackageManager::Brew | PackageManager::BrewCask)
}

/// Shell out to the manager's list command and parse the output
/// into a set of installed package names / IDs.
fn fetch_installed(manager: PackageManager) -> Result<HashSet<String>> {
    if let Some(packages) = test_packages_override(manager) {
        return Ok(packages);
    }
    validate_package_manager_supported(manager)?;
    match manager {
        PackageManager::Brew => brew::fetch_formulae(),
        PackageManager::BrewCask => brew::fetch_casks(),
        PackageManager::Cargo => cargo::fetch(),
        PackageManager::Winget => winget::fetch(),
    }
}

fn fetch_outdated(manager: PackageManager) -> Result<HashSet<String>> {
    if let Some(packages) = test_outdated_override(manager) {
        return Ok(packages);
    }
    validate_package_manager_supported(manager)?;
    match manager {
        PackageManager::Brew => brew::fetch_outdated_formulae(),
        PackageManager::BrewCask => brew::fetch_outdated_casks(),
        // Cargo/winget don't share brew's outdated semantics — they
        // get the empty set here, which classify_package treats as
        // "no updates available" and falls back to NoOp. Adding a
        // real probe per manager is a separate piece of work.
        PackageManager::Cargo | PackageManager::Winget => Ok(HashSet::new()),
    }
}

fn fetch_taps() -> Result<HashSet<String>> {
    if let Some(taps) = test_taps_override() {
        return Ok(taps);
    }
    brew::fetch_taps()
}

fn fetch_tap_remote(user_tap: &str) -> Result<Option<String>> {
    if let Some(remote) = test_tap_remote_override(user_tap) {
        return Ok(remote);
    }
    brew::fetch_tap_remote(user_tap)
}

fn test_packages_override(manager: PackageManager) -> Option<HashSet<String>> {
    if !test_overrides_allowed() {
        return None;
    }
    let key = match manager {
        PackageManager::Brew => "KERON_TEST_BREW_PACKAGES",
        PackageManager::BrewCask => "KERON_TEST_BREW_CASK_PACKAGES",
        PackageManager::Cargo => "KERON_TEST_CARGO_PACKAGES",
        PackageManager::Winget => "KERON_TEST_WINGET_PACKAGES",
    };
    let raw = std::env::var(key).ok()?;
    Some(parse_csv(&raw))
}

fn test_outdated_override(manager: PackageManager) -> Option<HashSet<String>> {
    if !test_overrides_allowed() {
        return None;
    }
    let key = match manager {
        PackageManager::Brew => "KERON_TEST_BREW_OUTDATED",
        PackageManager::BrewCask => "KERON_TEST_BREW_CASK_OUTDATED",
        // No env seam for cargo/winget — fetch_outdated returns
        // empty for those managers regardless.
        PackageManager::Cargo | PackageManager::Winget => return None,
    };
    let raw = std::env::var(key).ok()?;
    Some(parse_csv(&raw))
}

fn test_taps_override() -> Option<HashSet<String>> {
    if !test_overrides_allowed() {
        return None;
    }
    let raw = std::env::var("KERON_TEST_BREW_TAPS").ok()?;
    Some(parse_csv(&raw))
}

/// Test seam for `brew tap-info`. The env var format is
/// `user/repo=URL;user2/repo2=URL2`. An entry of `user/repo=` (empty
/// value) maps to `Some(None)` — i.e. "tap is installed but has no
/// known remote", which exercises the `None` arm of
/// [`PackageCache::tap_remote`].
#[allow(clippy::option_option)]
fn test_tap_remote_override(user_tap: &str) -> Option<Option<String>> {
    if !test_overrides_allowed() {
        return None;
    }
    let raw = std::env::var("KERON_TEST_BREW_TAP_REMOTES").ok()?;
    for entry in raw.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (key, val) = entry.split_once('=')?;
        if key.trim() == user_tap {
            let val = val.trim();
            return Some(if val.is_empty() {
                None
            } else {
                Some(val.to_string())
            });
        }
    }
    None
}

fn parse_csv(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Install one package. Stdio is inherited so the user sees the
/// underlying manager's output (progress bars, download status).
/// The binary path is overridable per-manager via
/// `KERON_TEST_PACKAGE_BIN_<MGR>` so tests can swap in a spy
/// script.
///
/// # Errors
/// Errors when the manager binary is missing, fails to spawn, or
/// exits non-zero.
pub fn install(manager: PackageManager, name: &str) -> Result<()> {
    validate_package_name(manager, name)?;
    validate_package_manager_supported(manager)?;
    let (binary, args) = install_invocation(manager, name);
    let status = Command::new(&binary)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("spawning `{binary} {}` to install `{name}`", args.join(" ")))?;
    if !status.success() {
        bail!("`{binary} {}` exited with status {status}", args.join(" "));
    }
    Ok(())
}

/// Upgrade one already-installed package. Used by the Update action;
/// only meaningful for brew / brew-cask today (cargo / winget would
/// need their own upgrade story).
///
/// # Errors
/// Errors when the manager isn't brew-family, the binary is missing,
/// fails to spawn, or exits non-zero.
pub fn upgrade(manager: PackageManager, name: &str) -> Result<()> {
    validate_package_name(manager, name)?;
    validate_package_manager_supported(manager)?;
    match manager {
        PackageManager::Brew => brew::do_upgrade(name, false),
        PackageManager::BrewCask => brew::do_upgrade(name, true),
        PackageManager::Cargo | PackageManager::Winget => {
            bail!(
                "upgrade not supported for {} packages yet — only brew formulae and casks have an outdated probe",
                manager.kind_label()
            )
        }
    }
}

/// Register a homebrew tap. Idempotent on brew's side, but callers
/// typically gate this on [`PackageCache::classify_tap`] returning
/// Create or Update so it doesn't shell out when the tap is already
/// configured correctly.
pub fn tap(spec: &TapSpec, action: Action) -> Result<()> {
    if let Some(url) = spec.url.as_deref() {
        brew::validate_tap_url(url)?;
    }
    let custom_remote = matches!(action, Action::Update);
    brew::do_tap(&spec.user_tap, spec.url.as_deref(), custom_remote)
}

pub fn validate_package_manager_supported(manager: PackageManager) -> Result<()> {
    let os = crate::platform::detect_os_family();
    if manager.is_supported_on(os) {
        return Ok(());
    }
    bail!(
        "{} package resources are not supported on {}; supported on: {}",
        manager.kind_label(),
        os.label(),
        manager.supported_os_label(),
    );
}

/// Reject package names that would be parsed as CLI flags or
/// otherwise smuggle behavior into the manager invocation.
/// The package-manager invocations pass `name` as a positional
/// argument; a name beginning with `-` becomes a flag the CLI
/// interprets — e.g. `cargo install --git=…` would run arbitrary
/// build scripts as the user. Also forbid embedded NUL since
/// argv can't carry it.
///
/// # Errors
/// Errors when `name` is empty, begins with `-`, or contains a NUL byte.
pub fn validate_package_name(manager: PackageManager, name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("{} package name must not be empty", manager.kind_label());
    }
    if name.starts_with('-') {
        bail!(
            "{} package name must not begin with `-`: `{name}`",
            manager.kind_label()
        );
    }
    if name.contains('\0') {
        bail!(
            "{} package name must not contain a NUL byte",
            manager.kind_label()
        );
    }
    Ok(())
}

fn install_invocation(manager: PackageManager, name: &str) -> (String, Vec<String>) {
    let binary = test_binary_override(manager).unwrap_or_else(|| manager.label().to_string());
    let args = match manager {
        PackageManager::Brew | PackageManager::Cargo => {
            vec!["install".to_string(), name.to_string()]
        }
        PackageManager::BrewCask => vec![
            "install".to_string(),
            "--cask".to_string(),
            name.to_string(),
        ],
        PackageManager::Winget => vec![
            "install".to_string(),
            "--exact".to_string(),
            "--id".to_string(),
            name.to_string(),
            "--accept-package-agreements".to_string(),
            "--accept-source-agreements".to_string(),
            "--disable-interactivity".to_string(),
        ],
    };
    (binary, args)
}

fn test_binary_override(manager: PackageManager) -> Option<String> {
    if !test_overrides_allowed() {
        return None;
    }
    // `Brew` and `BrewCask` share a binary (and a binary-override
    // env var) since they both invoke `brew`.
    let key = match manager {
        PackageManager::Brew | PackageManager::BrewCask => "KERON_TEST_PACKAGE_BIN_BREW",
        PackageManager::Cargo => "KERON_TEST_PACKAGE_BIN_CARGO",
        PackageManager::Winget => "KERON_TEST_PACKAGE_BIN_WINGET",
    };
    std::env::var(key).ok()
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn test_overrides_allowed() -> bool {
    std::env::var_os("KERON_ALLOW_TEST_OVERRIDES").is_some_and(|v| v == "1")
}

/// Process-wide lock that serialises every test in this crate which
/// mutates the `KERON_TEST_*` / `KERON_ALLOW_TEST_OVERRIDES` env vars.
/// Cargo runs tests in parallel by default; env vars are global, so
/// one test's `clear_env` can otherwise knock out another test's gate
/// flag mid-classify and produce a flaky "fell through to real `brew`"
/// failure. Promoted to crate-wide scope so the parallel test modules
/// (`packages::tests`, `execute::tests`) share a single lock.
#[cfg(test)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::lock_env;

    fn clear_env(keys: &[&str]) {
        #[allow(unsafe_code)]
        unsafe {
            for k in keys {
                std::env::remove_var(k);
            }
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
        }
    }

    fn set_env(key: &str, value: &str) {
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
            std::env::set_var(key, value);
        }
    }

    #[test]
    fn classify_package_returns_create_for_missing_then_noop_for_repeat() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_PACKAGES", "");
        let mut cache = PackageCache::new();
        let first = cache
            .classify_package(PackageManager::Brew, "ripgrep")
            .unwrap();
        let second = cache
            .classify_package(PackageManager::Brew, "ripgrep")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        assert_eq!(first, Action::Create);
        assert_eq!(second, Action::NoOp);
    }

    #[test]
    fn classify_package_returns_noop_when_already_installed_and_not_outdated() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_PACKAGES", "git,ripgrep,fd");
        // Empty outdated list — the test's whole point is "installed
        // and *not* outdated → NoOp", so the outdated probe must be
        // bypassed (otherwise it shells out to a real `brew`).
        set_env("KERON_TEST_BREW_OUTDATED", "");
        let mut cache = PackageCache::new();
        let action = cache
            .classify_package(PackageManager::Brew, "ripgrep")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES", "KERON_TEST_BREW_OUTDATED"]);
        assert_eq!(action, Action::NoOp);
    }

    #[test]
    fn classify_package_returns_update_when_installed_and_outdated() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_PACKAGES", "ripgrep");
        set_env("KERON_TEST_BREW_OUTDATED", "ripgrep");
        let mut cache = PackageCache::new();
        let action = cache
            .classify_package(PackageManager::Brew, "ripgrep")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES", "KERON_TEST_BREW_OUTDATED"]);
        assert_eq!(action, Action::Update);
    }

    #[test]
    fn classify_package_strips_tap_prefix_for_installed_lookup() {
        let _g = lock_env();
        // `brew list` reports tap-installed `icepuma/keron/keron` as
        // bare `keron`. A manifest naming the qualified form must
        // still classify as NoOp.
        set_env("KERON_TEST_BREW_PACKAGES", "keron");
        set_env("KERON_TEST_BREW_OUTDATED", "");
        let mut cache = PackageCache::new();
        let action = cache
            .classify_package(PackageManager::Brew, "icepuma/keron/keron")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES", "KERON_TEST_BREW_OUTDATED"]);
        assert_eq!(action, Action::NoOp);
    }

    #[test]
    fn classify_package_uses_qualified_name_for_outdated_lookup() {
        let _g = lock_env();
        // `brew outdated` reports tap-installed formulae by the full
        // qualified name; pin that the classifier matches that shape.
        set_env("KERON_TEST_BREW_PACKAGES", "keron");
        set_env("KERON_TEST_BREW_OUTDATED", "icepuma/keron/keron");
        let mut cache = PackageCache::new();
        let action = cache
            .classify_package(PackageManager::Brew, "icepuma/keron/keron")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES", "KERON_TEST_BREW_OUTDATED"]);
        assert_eq!(action, Action::Update);
    }

    #[test]
    fn classify_package_brew_cask_uses_its_own_installed_set() {
        let _g = lock_env();
        // Casks live in a separate namespace; a cask named "alacritty"
        // mustn't be confused with the formula "alacritty".
        set_env("KERON_TEST_BREW_PACKAGES", "git");
        set_env("KERON_TEST_BREW_CASK_PACKAGES", "alacritty");
        set_env("KERON_TEST_BREW_OUTDATED", "");
        set_env("KERON_TEST_BREW_CASK_OUTDATED", "");
        let mut cache = PackageCache::new();
        let formula = cache
            .classify_package(PackageManager::Brew, "alacritty")
            .unwrap();
        let cask = cache
            .classify_package(PackageManager::BrewCask, "alacritty")
            .unwrap();
        clear_env(&[
            "KERON_TEST_BREW_PACKAGES",
            "KERON_TEST_BREW_CASK_PACKAGES",
            "KERON_TEST_BREW_OUTDATED",
            "KERON_TEST_BREW_CASK_OUTDATED",
        ]);
        assert_eq!(formula, Action::Create);
        assert_eq!(cask, Action::NoOp);
    }

    #[test]
    fn classify_tap_returns_create_when_not_installed() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_TAPS", "");
        let mut cache = PackageCache::new();
        let spec = TapSpec {
            user_tap: "icepuma/keron".into(),
            url: None,
        };
        let action = cache.classify_tap(&spec).unwrap();
        clear_env(&["KERON_TEST_BREW_TAPS"]);
        assert_eq!(action, Action::Create);
    }

    #[test]
    fn classify_tap_returns_noop_when_installed_and_no_url_required() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_TAPS", "icepuma/keron");
        let mut cache = PackageCache::new();
        let spec = TapSpec {
            user_tap: "icepuma/keron".into(),
            url: None,
        };
        let action = cache.classify_tap(&spec).unwrap();
        clear_env(&["KERON_TEST_BREW_TAPS"]);
        assert_eq!(action, Action::NoOp);
    }

    #[test]
    fn classify_tap_returns_noop_when_url_matches() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_TAPS", "icepuma/keron");
        set_env(
            "KERON_TEST_BREW_TAP_REMOTES",
            "icepuma/keron=https://github.com/icepuma/keron",
        );
        let mut cache = PackageCache::new();
        let spec = TapSpec {
            user_tap: "icepuma/keron".into(),
            url: Some("https://github.com/icepuma/keron".into()),
        };
        let action = cache.classify_tap(&spec).unwrap();
        clear_env(&["KERON_TEST_BREW_TAPS", "KERON_TEST_BREW_TAP_REMOTES"]);
        assert_eq!(action, Action::NoOp);
    }

    #[test]
    fn classify_tap_returns_update_when_url_differs() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_TAPS", "icepuma/keron");
        set_env(
            "KERON_TEST_BREW_TAP_REMOTES",
            "icepuma/keron=https://github.com/old/url",
        );
        let mut cache = PackageCache::new();
        let spec = TapSpec {
            user_tap: "icepuma/keron".into(),
            url: Some("https://github.com/icepuma/keron".into()),
        };
        let action = cache.classify_tap(&spec).unwrap();
        clear_env(&["KERON_TEST_BREW_TAPS", "KERON_TEST_BREW_TAP_REMOTES"]);
        assert_eq!(action, Action::Update);
    }

    #[test]
    fn classify_tap_dedup_in_same_run_returns_noop_on_repeat() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_TAPS", "");
        let mut cache = PackageCache::new();
        let spec = TapSpec {
            user_tap: "icepuma/keron".into(),
            url: None,
        };
        let first = cache.classify_tap(&spec).unwrap();
        let second = cache.classify_tap(&spec).unwrap();
        clear_env(&["KERON_TEST_BREW_TAPS"]);
        assert_eq!(first, Action::Create);
        assert_eq!(second, Action::NoOp);
    }

    #[test]
    fn bare_name_strips_user_tap_prefix() {
        let _g = lock_env();
        assert_eq!(bare_name("ripgrep"), "ripgrep");
        assert_eq!(bare_name("icepuma/keron/keron"), "keron");
        assert_eq!(bare_name("fluxcd/tap/flux"), "flux");
    }

    #[test]
    fn install_invocation_brew_uses_install_with_name() {
        let _g = lock_env();
        let (bin, args) = install_invocation(PackageManager::Brew, "ripgrep");
        assert_eq!(bin, "brew");
        assert_eq!(args, vec!["install", "ripgrep"]);
    }

    #[test]
    fn install_invocation_brew_cask_passes_cask_flag() {
        let _g = lock_env();
        let (bin, args) = install_invocation(PackageManager::BrewCask, "font-jetbrains-mono");
        assert_eq!(bin, "brew");
        assert_eq!(args, vec!["install", "--cask", "font-jetbrains-mono"]);
    }

    #[test]
    fn install_invocation_cargo_uses_install_with_name() {
        let _g = lock_env();
        let (bin, args) = install_invocation(PackageManager::Cargo, "sccache");
        assert_eq!(bin, "cargo");
        assert_eq!(args, vec!["install", "sccache"]);
    }

    #[test]
    fn install_invocation_winget_passes_exact_and_accept_flags() {
        let _g = lock_env();
        let (bin, args) = install_invocation(PackageManager::Winget, "Microsoft.PowerShell");
        assert_eq!(bin, "winget");
        assert!(args.contains(&"--exact".to_string()), "got: {args:?}");
        assert!(args.contains(&"--id".to_string()), "got: {args:?}");
        assert!(
            args.contains(&"--accept-package-agreements".to_string()),
            "got: {args:?}",
        );
        assert!(
            args.contains(&"--accept-source-agreements".to_string()),
            "got: {args:?}",
        );
        assert!(
            args.contains(&"--disable-interactivity".to_string()),
            "got: {args:?}",
        );
    }

    #[test]
    fn install_invocation_honours_binary_override() {
        let _g = lock_env();
        set_env("KERON_TEST_PACKAGE_BIN_BREW", "/tmp/fake-brew");
        let (bin, _) = install_invocation(PackageManager::Brew, "x");
        let (cask_bin, _) = install_invocation(PackageManager::BrewCask, "x");
        clear_env(&["KERON_TEST_PACKAGE_BIN_BREW"]);
        assert_eq!(bin, "/tmp/fake-brew");
        assert_eq!(cask_bin, "/tmp/fake-brew", "cask shares the brew binary");
    }

    #[test]
    fn install_rejects_empty_name() {
        let _g = lock_env();
        let err = install(PackageManager::Brew, "").unwrap_err();
        assert!(
            format!("{err:#}").contains("must not be empty"),
            "got: {err:#}",
        );
    }

    #[test]
    fn validate_package_name_rejects_leading_dash() {
        let _g = lock_env();
        for mgr in [
            PackageManager::Brew,
            PackageManager::BrewCask,
            PackageManager::Cargo,
            PackageManager::Winget,
        ] {
            let err = validate_package_name(mgr, "--git=https://attacker/evil").unwrap_err();
            assert!(
                format!("{err:#}").contains("must not begin with `-`"),
                "got: {err:#}",
            );
        }
    }

    #[test]
    fn validate_package_name_rejects_single_dash_prefix() {
        let _g = lock_env();
        let err = validate_package_name(PackageManager::Cargo, "-foo").unwrap_err();
        assert!(
            format!("{err:#}").contains("must not begin with `-`"),
            "got: {err:#}",
        );
    }

    #[test]
    fn validate_package_name_rejects_nul_byte() {
        let _g = lock_env();
        let err = validate_package_name(PackageManager::Brew, "rip\0grep").unwrap_err();
        assert!(format!("{err:#}").contains("NUL byte"), "got: {err:#}");
    }

    #[test]
    fn validate_package_name_accepts_dash_in_interior() {
        let _g = lock_env();
        validate_package_name(PackageManager::Brew, "git-lfs").unwrap();
        validate_package_name(PackageManager::Cargo, "cargo-edit").unwrap();
    }

    #[test]
    fn validate_package_manager_supported_rejects_wrong_os_manager() {
        let _g = lock_env();
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Windows);
        let err = validate_package_manager_supported(PackageManager::Brew).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("brew"), "got: {msg}");
        assert!(msg.contains("Windows"), "got: {msg}");
        assert!(msg.contains("Linux or Macos"), "got: {msg}");
    }

    #[test]
    fn validate_package_manager_supported_accepts_matching_manager() {
        let _g = lock_env();
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Windows);
        validate_package_manager_supported(PackageManager::Winget).unwrap();
        validate_package_manager_supported(PackageManager::Cargo).unwrap();
    }

    #[test]
    fn validate_package_manager_brew_cask_is_macos_only() {
        let _g = lock_env();
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Linux);
        let err = validate_package_manager_supported(PackageManager::BrewCask).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("cask"), "got: {msg}");
        assert!(msg.contains("Macos"), "got: {msg}");
    }

    #[test]
    fn test_packages_override_parses_csv_and_trims_whitespace() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_PACKAGES", " git , ripgrep ,, fd ");
        let got = test_packages_override(PackageManager::Brew).unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        let mut sorted: Vec<_> = got.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["fd", "git", "ripgrep"]);
    }

    #[test]
    fn test_packages_override_returns_none_when_unset() {
        let _g = lock_env();
        clear_env(&["KERON_TEST_WINGET_PACKAGES"]);
        assert!(test_packages_override(PackageManager::Winget).is_none());
    }

    #[test]
    fn test_packages_override_requires_allow_gate() {
        let _g = lock_env();
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_TEST_BREW_PACKAGES", "git");
        }
        let got = test_packages_override(PackageManager::Brew);
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        assert!(got.is_none());
    }

    #[test]
    fn test_tap_remote_override_parses_kv_pairs() {
        let _g = lock_env();
        set_env(
            "KERON_TEST_BREW_TAP_REMOTES",
            "icepuma/keron=https://github.com/icepuma/keron;foo/bar=git@host:foo/bar",
        );
        let a = test_tap_remote_override("icepuma/keron").unwrap();
        let b = test_tap_remote_override("foo/bar").unwrap();
        let c = test_tap_remote_override("missing/one");
        clear_env(&["KERON_TEST_BREW_TAP_REMOTES"]);
        assert_eq!(a, Some("https://github.com/icepuma/keron".into()));
        assert_eq!(b, Some("git@host:foo/bar".into()));
        assert!(c.is_none());
    }

    #[test]
    fn test_tap_remote_override_empty_value_means_unknown_remote() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_TAP_REMOTES", "icepuma/keron=");
        let got = test_tap_remote_override("icepuma/keron").unwrap();
        clear_env(&["KERON_TEST_BREW_TAP_REMOTES"]);
        assert_eq!(got, None);
    }
}
