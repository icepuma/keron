//! Package manager integration: list installed packages per
//! manager, cache the result, and install missing ones.
//!
//! Scope: `keron apply` only ensures *presence*. Upgrading installed
//! packages is left to the underlying manager (the user runs
//! `brew upgrade` / `cargo install --force` / … themselves) — that
//! keeps the apply phase predictable and avoids surprising version
//! bumps mid-reconcile. As a consequence the classifier only emits
//! `Create` (missing) or `NoOp` (already installed); there is no
//! `Update` action for packages, and the executor has no
//! upgrade-by-name path. (Taps still classify `Update` for URL
//! drift — that's a different shape: re-tap with `--custom-remote`,
//! not "upgrade".)
//!
//! The cache lives for the duration of one `keron apply` and is
//! populated lazily — the first time a `brew(...)` resource is
//! classified, we shell out to `brew list` and remember the result;
//! later `brew(...)` resources reuse that snapshot without
//! re-querying. The cost is one shell-out per (manager, query) that
//! appears in the manifest, not one per resource.
//!
//! Idempotency: [`PackageCache::classify_package`] returns the
//! `Action` to apply (`Create` / `NoOp`). It compares against the
//! "installed" set by the *bare* tail of the qualified name (because
//! `brew list` reports tap-installed formulae without the tap
//! prefix). The classifier also records each name it returned
//! `Create` for, so two `brew("ripgrep")` resources in the same plan
//! classify as `Create` / `NoOp` rather than `Create` / `Create`.
//!
//! Test seam: each manager's fetch reads `KERON_TEST_<MGR>_PACKAGES`
//! (comma-separated) instead of shelling out, so unit tests can
//! drive any cache state without a real `brew` / `cargo` / `winget`
//! on the host. Brew has additional seams for casks
//! (`KERON_TEST_BREW_CASK_PACKAGES`), installed taps
//! (`KERON_TEST_BREW_TAPS`), and per-tap remote URLs
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

use crate::plan::{Action, PackageManager, ResourceState, TapSpec};
use crate::platform::OsFamily;

/// Snapshot of "what's installed / tapped" per query. Populated
/// lazily; one `PackageCache` per `keron apply` invocation.
#[derive(Debug)]
pub struct PackageCache {
    /// Host OS snapshot taken at construction. The planner's prewarm
    /// fans probes out across worker threads, so we cannot rely on
    /// the thread-local test-override fallback in `detect_os_family`
    /// — every probe gets the OS as a value instead.
    os: OsFamily,
    /// Bare names of installed packages, keyed by manager. For
    /// `Brew` this is `brew list --formula -1`; for `BrewCask`,
    /// `brew list --cask -1`; for cargo / winget, the manager's own
    /// list command. Loaded lazily on first access per manager.
    installed: HashMap<PackageManager, HashSet<String>>,
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
    pub fn new(os: OsFamily) -> Self {
        Self {
            os,
            installed: HashMap::new(),
            installed_taps: None,
            tap_remotes: HashMap::new(),
            scheduled: HashMap::new(),
            scheduled_taps: HashSet::new(),
        }
    }

    /// Test helper: build a cache snapshotting whatever the current
    /// thread's `detect_os_family()` reports (which honours the
    /// `OsOverride` test seam). Production callers pass an explicit
    /// `OsFamily` via [`Self::new`].
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self::new(crate::platform::detect_os_family())
    }

    /// Pre-warm every probe the upcoming classify pass will touch by
    /// running them concurrently. Walks `resources` to determine which
    /// `(manager, query)` shell-outs would otherwise be incurred
    /// lazily by [`Self::ensure_installed_loaded`] /
    /// [`Self::ensure_taps_loaded`] / [`Self::tap_remote`], then fans
    /// them out across `std::thread::scope` worker threads. Each
    /// worker is I/O-bound (waiting on a subprocess), so the wall
    /// time collapses to roughly the slowest probe rather than the
    /// sum of all probes.
    ///
    /// Idempotent: probes whose target slot is already populated (e.g.
    /// from a prior `prewarm` or a lazy `classify_*` call) are skipped.
    /// Safe to call with an empty `resources` slice — returns Ok with
    /// no work done.
    ///
    /// On any probe failure, returns the first error encountered with
    /// the same `with_context` shape as the lazy paths, so callers see
    /// identical diagnostics whether the cache was warmed eagerly or
    /// loaded on demand.
    ///
    /// # Errors
    /// Errors when any of the underlying probes fails (process spawn,
    /// non-zero exit, malformed output).
    pub fn prewarm(&mut self, resources: &[ResourceState]) -> Result<()> {
        let mut needed_installed: HashSet<PackageManager> = HashSet::new();
        let mut need_taps = false;
        let mut needed_tap_remotes: Vec<String> = Vec::new();

        for state in resources {
            match state {
                ResourceState::Package { manager, .. } => {
                    if !self.installed.contains_key(manager) {
                        needed_installed.insert(*manager);
                    }
                }
                ResourceState::Tap(spec) => {
                    if self.installed_taps.is_none() {
                        need_taps = true;
                    }
                    if spec.url.is_some() && !self.tap_remotes.contains_key(&spec.user_tap) {
                        needed_tap_remotes.push(spec.user_tap.clone());
                    }
                }
                ResourceState::Symlink { .. }
                | ResourceState::Template { .. }
                | ResourceState::Shell { .. }
                | ResourceState::SshKey { .. }
                | ResourceState::GpgKey { .. } => {}
            }
        }

        needed_tap_remotes.sort();
        needed_tap_remotes.dedup();

        if needed_installed.is_empty() && !need_taps && needed_tap_remotes.is_empty() {
            return Ok(());
        }

        let installed_managers: Vec<PackageManager> = needed_installed.into_iter().collect();
        let os = self.os;

        // `collect::<Vec<_>>()` on the spawn iterators below is
        // load-bearing — it materialises every spawn before any join
        // so the probes actually run in parallel. A chained
        // `.map(spawn).map(join)` would interleave spawn-then-join
        // lazily and serialise the prewarm.
        let ProbeResults {
            installed_results,
            taps_result,
            tap_remote_results,
        } = std::thread::scope(|s| {
            #[allow(clippy::needless_collect)]
            let installed_handles: Vec<(PackageManager, _)> = installed_managers
                .iter()
                .copied()
                .map(|m| (m, s.spawn(move || fetch_installed(m, os))))
                .collect();
            let taps_handle = if need_taps {
                Some(s.spawn(fetch_taps))
            } else {
                None
            };
            #[allow(clippy::needless_collect)]
            let tap_remote_handles: Vec<(String, _)> = needed_tap_remotes
                .iter()
                .cloned()
                .map(|t| {
                    let key = t.clone();
                    (key, s.spawn(move || fetch_tap_remote(&t)))
                })
                .collect();

            let installed_results: Vec<(PackageManager, Result<HashSet<String>>)> =
                installed_handles
                    .into_iter()
                    .map(|(m, h)| (m, join_probe(h)))
                    .collect();
            let taps_result = taps_handle.map(join_probe);
            let tap_remote_results: Vec<(String, Result<Option<String>>)> = tap_remote_handles
                .into_iter()
                .map(|(t, h)| (t, join_probe(h)))
                .collect();

            ProbeResults {
                installed_results,
                taps_result,
                tap_remote_results,
            }
        });

        for (m, r) in installed_results {
            let set = r.with_context(|| {
                format!(
                    "listing installed packages for {} (`{} list ...`)",
                    m.kind_label(),
                    m.label()
                )
            })?;
            self.installed.insert(m, set);
        }
        if let Some(r) = taps_result {
            let set = r.context("listing installed brew taps (`brew tap`)")?;
            self.installed_taps = Some(set);
        }
        for (t, r) in tap_remote_results {
            let remote = r.with_context(|| {
                format!("reading remote URL for tap `{t}` (`brew tap-info --json`)")
            })?;
            self.tap_remotes.insert(t, remote);
        }

        Ok(())
    }

    /// Classify a package resource against the live state.
    ///
    /// Returns the action the planner should record:
    ///   - `NoOp` — name is installed, OR another resource in this
    ///     same plan already claimed a `Create` for it.
    ///   - `Create` — name isn't installed yet.
    ///
    /// "Installed" is compared by the *bare* tail of the qualified
    /// name (because `brew list` reports tap-installed formulae by
    /// bare name).
    ///
    /// `keron apply` does **not** upgrade installed packages — that's
    /// the user's job via the underlying manager — so `Update` is
    /// never returned here. See the module docs for the rationale.
    ///
    /// # Errors
    /// Errors when the underlying probe fails on first access. Cached
    /// snapshots on subsequent calls don't re-probe.
    pub fn classify_package(&mut self, manager: PackageManager, name: &str) -> Result<Action> {
        let bare = bare_name(name);
        self.ensure_installed_loaded(manager)?;
        let installed = self
            .installed
            .get(&manager)
            .expect("just loaded above")
            .contains(bare);
        if installed {
            return Ok(Action::NoOp);
        }
        let scheduled = self.scheduled.entry(manager).or_default();
        Ok(if scheduled.contains(name) {
            Action::NoOp
        } else {
            scheduled.insert(name.to_string());
            Action::Create
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
                let set = fetch_installed(manager, self.os).with_context(|| {
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

/// Bundle of probe outcomes returned by [`PackageCache::prewarm`]'s
/// `std::thread::scope` closure. Factored out so the closure's return
/// type stays simple (clippy flags the bare tuple as too complex).
struct ProbeResults {
    installed_results: Vec<(PackageManager, Result<HashSet<String>>)>,
    taps_result: Option<Result<HashSet<String>>>,
    tap_remote_results: Vec<(String, Result<Option<String>>)>,
}

/// Join a probe worker, surfacing a panic as a hard error rather than
/// silently swallowing it. A probe panic indicates a bug in this
/// crate (parsing, env handling, …) — preserve it for diagnosis.
fn join_probe<T>(handle: std::thread::ScopedJoinHandle<'_, Result<T>>) -> Result<T> {
    match handle.join() {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload.downcast_ref::<&'static str>().map_or_else(
                || {
                    payload
                        .downcast_ref::<String>()
                        .map_or_else(|| "probe worker panicked".to_string(), String::clone)
                },
                |s| (*s).to_string(),
            );
            bail!("probe worker panicked: {msg}")
        }
    }
}

/// Shell out to the manager's list command and parse the output
/// into a set of installed package names / IDs.
fn fetch_installed(manager: PackageManager, os: OsFamily) -> Result<HashSet<String>> {
    if let Some(packages) = test_packages_override(manager) {
        return Ok(packages);
    }
    validate_package_manager_supported(manager, os)?;
    match manager {
        PackageManager::Brew => brew::fetch_formulae(),
        PackageManager::BrewCask => brew::fetch_casks(),
        PackageManager::Cargo => cargo::fetch(),
        PackageManager::Winget => winget::fetch(),
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

/// How a batched install/upgrade routes its child stdio.
///
/// - `Inherit` — the manager's progress bars / download status pass
///   directly to the user's terminal. Use when only one batch is
///   running at a time so the output isn't interleaved with other
///   managers' streams.
/// - `Capture` — stdout and stderr are captured into byte buffers
///   so the caller can flush them in a deterministic order after a
///   parallel phase joins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchStdio {
    Inherit,
    Capture,
}

/// Output captured from a batch run. Empty when the batch ran with
/// [`BatchStdio::Inherit`] — there's nothing to return because the
/// child wrote directly to the parent terminal.
#[derive(Debug, Default, Clone)]
pub struct BatchOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Install one or more packages in a single subprocess where the
/// manager supports a multi-name argv. Brew, brew-cask, and cargo
/// collapse into one invocation; winget loops internally one name at
/// a time (its install command takes a single query). Stdio is
/// inherited so the user sees live progress.
///
/// Validates every name (`validate_package_name`) and the manager
/// (`validate_package_manager_supported`) up front so a malformed
/// entry can't smuggle a flag into the batched argv mid-batch.
///
/// # Errors
/// Errors when validation fails, the manager binary is missing,
/// fails to spawn, or exits non-zero.
pub fn install_many(manager: PackageManager, names: &[&str], os: OsFamily) -> Result<()> {
    install_many_with_stdio(manager, names, BatchStdio::Inherit, os).map(|_| ())
}

/// [`install_many`] variant that captures child stdio for the caller
/// to flush after a parallel phase completes. See [`BatchStdio`].
///
/// # Errors
/// Same shape as [`install_many`].
pub fn install_many_captured(
    manager: PackageManager,
    names: &[&str],
    os: OsFamily,
) -> Result<BatchOutput> {
    install_many_with_stdio(manager, names, BatchStdio::Capture, os)
}

fn install_many_with_stdio(
    manager: PackageManager,
    names: &[&str],
    stdio: BatchStdio,
    os: OsFamily,
) -> Result<BatchOutput> {
    if names.is_empty() {
        return Ok(BatchOutput::default());
    }
    validate_package_manager_supported(manager, os)?;
    for n in names {
        validate_package_name(manager, n)?;
    }
    if let Some((binary, args)) = install_many_invocation(manager, names) {
        return spawn_batch(&binary, &args, stdio);
    }
    // Managers without a native multi-name argv (winget) loop
    // internally and concatenate any captured output.
    let mut combined = BatchOutput::default();
    for name in names {
        let (binary, args) = install_invocation(manager, name);
        let chunk = spawn_batch(&binary, &args, stdio)?;
        if stdio == BatchStdio::Capture {
            combined.stdout.extend_from_slice(&chunk.stdout);
            combined.stderr.extend_from_slice(&chunk.stderr);
        }
    }
    Ok(combined)
}

fn spawn_batch(binary: &str, args: &[String], stdio: BatchStdio) -> Result<BatchOutput> {
    let mut cmd = Command::new(binary);
    cmd.args(args);
    match stdio {
        BatchStdio::Inherit => {
            cmd.stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            let status = cmd
                .status()
                .with_context(|| format!("spawning `{binary} {}`", args.join(" ")))?;
            if !status.success() {
                bail!("`{binary} {}` exited with status {status}", args.join(" "));
            }
            Ok(BatchOutput::default())
        }
        BatchStdio::Capture => {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let out = cmd
                .output()
                .with_context(|| format!("spawning `{binary} {}`", args.join(" ")))?;
            if !out.status.success() {
                bail!(
                    "`{binary} {}` exited with status {}; stderr: {}",
                    args.join(" "),
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim(),
                );
            }
            Ok(BatchOutput {
                stdout: out.stdout,
                stderr: out.stderr,
            })
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

pub fn validate_package_manager_supported(manager: PackageManager, os: OsFamily) -> Result<()> {
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

/// Argv for a multi-name `install` invocation. Returns `None` for
/// managers without a native batch (`winget`) — the caller falls back
/// to looping single-name installs.
fn install_many_invocation(
    manager: PackageManager,
    names: &[&str],
) -> Option<(String, Vec<String>)> {
    let binary = test_binary_override(manager).unwrap_or_else(|| manager.label().to_string());
    let args = match manager {
        PackageManager::Brew | PackageManager::Cargo => {
            let mut a = Vec::with_capacity(names.len() + 1);
            a.push("install".to_string());
            a.extend(names.iter().map(|s| (*s).to_string()));
            a
        }
        PackageManager::BrewCask => {
            let mut a = Vec::with_capacity(names.len() + 2);
            a.push("install".to_string());
            a.push("--cask".to_string());
            a.extend(names.iter().map(|s| (*s).to_string()));
            a
        }
        PackageManager::Winget => return None,
    };
    Some((binary, args))
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

    fn brew_pkg(name: &str) -> ResourceState {
        ResourceState::Package {
            manager: PackageManager::Brew,
            name: name.to_string(),
            tap: None,
        }
    }

    fn cask_pkg(name: &str) -> ResourceState {
        ResourceState::Package {
            manager: PackageManager::BrewCask,
            name: name.to_string(),
            tap: None,
        }
    }

    fn cargo_pkg(name: &str) -> ResourceState {
        ResourceState::Package {
            manager: PackageManager::Cargo,
            name: name.to_string(),
            tap: None,
        }
    }

    fn tap_state(user_tap: &str, url: Option<&str>) -> ResourceState {
        ResourceState::Tap(TapSpec {
            user_tap: user_tap.to_string(),
            url: url.map(str::to_string),
        })
    }

    #[test]
    fn prewarm_populates_every_needed_probe_in_one_pass() {
        // Pins the contract that prewarm leaves the cache hot for
        // every classify path the resource list will touch.
        let _g = lock_env();
        set_env("KERON_TEST_BREW_PACKAGES", "ripgrep,git");
        set_env("KERON_TEST_BREW_CASK_PACKAGES", "alacritty");
        set_env("KERON_TEST_CARGO_PACKAGES", "sccache");
        set_env("KERON_TEST_BREW_TAPS", "icepuma/keron");
        set_env(
            "KERON_TEST_BREW_TAP_REMOTES",
            "icepuma/keron=https://github.com/icepuma/keron",
        );
        let resources = vec![
            brew_pkg("ripgrep"),
            brew_pkg("fd"),
            cask_pkg("alacritty"),
            cargo_pkg("sccache"),
            tap_state("icepuma/keron", Some("https://github.com/icepuma/keron")),
        ];
        let mut cache = PackageCache::for_tests();
        cache.prewarm(&resources).unwrap();
        // Drop the env seams so any further probe would fall through
        // to real `brew` / `cargo` and fail. The classify calls below
        // must therefore hit the cache exclusively.
        clear_env(&[
            "KERON_TEST_BREW_PACKAGES",
            "KERON_TEST_BREW_CASK_PACKAGES",
            "KERON_TEST_CARGO_PACKAGES",
            "KERON_TEST_BREW_TAPS",
            "KERON_TEST_BREW_TAP_REMOTES",
        ]);
        assert_eq!(
            cache
                .classify_package(PackageManager::Brew, "ripgrep")
                .unwrap(),
            Action::NoOp,
        );
        assert_eq!(
            cache.classify_package(PackageManager::Brew, "fd").unwrap(),
            Action::Create,
        );
        assert_eq!(
            cache
                .classify_package(PackageManager::BrewCask, "alacritty")
                .unwrap(),
            Action::NoOp,
        );
        assert_eq!(
            cache
                .classify_package(PackageManager::Cargo, "sccache")
                .unwrap(),
            Action::NoOp,
        );
        let tap_action = cache
            .classify_tap(&TapSpec {
                user_tap: "icepuma/keron".into(),
                url: Some("https://github.com/icepuma/keron".into()),
            })
            .unwrap();
        assert_eq!(tap_action, Action::NoOp);
    }

    #[test]
    fn prewarm_is_a_noop_when_no_package_or_tap_resources_present() {
        // Walks resources without any Package/Tap and confirms prewarm
        // doesn't spawn probes (no env seams set — a real `brew` shell-out
        // would fail in CI). Pins that the empty-needs early return holds.
        let _g = lock_env();
        clear_env(&[]);
        let resources = vec![ResourceState::Symlink {
            from: std::path::PathBuf::from("/tmp/keron-prewarm-noop-link"),
            to: std::path::PathBuf::from("/tmp/keron-prewarm-noop-target"),
        }];
        let mut cache = PackageCache::for_tests();
        cache.prewarm(&resources).unwrap();
        assert!(cache.installed.is_empty());
        assert!(cache.installed_taps.is_none());
        assert!(cache.tap_remotes.is_empty());
    }

    #[test]
    fn prewarm_loads_taps_when_only_tap_resources_are_present() {
        // Pins the prewarm early-return guard: when no Package resources
        // are pending (so `needed_installed.is_empty()` is true) but a
        // Tap resource still needs probing, the function MUST spawn the
        // `brew tap` probe rather than short-circuiting. Catches the
        // `&& -> ||` and `delete !` mutations on line 157 that would let
        // the empty-installed branch poison the early-return decision
        // and silently leave `installed_taps` at `None`.
        let _g = lock_env();
        set_env("KERON_TEST_BREW_TAPS", "icepuma/keron");
        let resources = vec![tap_state("icepuma/keron", None)];
        let mut cache = PackageCache::for_tests();
        cache.prewarm(&resources).unwrap();
        clear_env(&["KERON_TEST_BREW_TAPS"]);
        assert!(
            cache.installed_taps.is_some(),
            "prewarm must populate installed_taps when a tap resource is pending",
        );
        assert!(
            cache
                .installed_taps
                .as_ref()
                .unwrap()
                .contains("icepuma/keron"),
            "installed_taps must reflect the env-seam contents",
        );
    }

    #[test]
    fn prewarm_loads_tap_remotes_for_url_qualified_taps_even_with_no_packages() {
        // Companion of the above: with the second `&&` on line 157
        // flipped to `||`, the `needed_tap_remotes` branch is bypassed
        // when both packages and taps are already cached. Force a probe
        // by supplying a URL-qualified tap and an env seam, then assert
        // the per-tap remote memo got populated.
        let _g = lock_env();
        set_env("KERON_TEST_BREW_TAPS", "icepuma/keron");
        set_env(
            "KERON_TEST_BREW_TAP_REMOTES",
            "icepuma/keron=https://github.com/icepuma/keron",
        );
        // Pre-load installed_taps so `need_taps` is false; only the
        // tap-remote slot still needs probing.
        let resources_pre = vec![tap_state("icepuma/keron", None)];
        let mut cache = PackageCache::for_tests();
        cache.prewarm(&resources_pre).unwrap();
        // Now prewarm with a URL-qualified tap. needed_installed is
        // empty, need_taps is false, needed_tap_remotes has one entry.
        let resources = vec![tap_state(
            "icepuma/keron",
            Some("https://github.com/icepuma/keron"),
        )];
        cache.prewarm(&resources).unwrap();
        clear_env(&["KERON_TEST_BREW_TAPS", "KERON_TEST_BREW_TAP_REMOTES"]);
        assert!(
            cache.tap_remotes.contains_key("icepuma/keron"),
            "prewarm must probe the tap remote when only that slot is pending",
        );
    }

    #[test]
    fn prewarm_skips_probes_already_populated_by_prior_lazy_load() {
        // Pins idempotence: prewarm must not re-probe a slot that a
        // prior classify_* already populated.
        let _g = lock_env();
        set_env("KERON_TEST_BREW_PACKAGES", "ripgrep");
        let mut cache = PackageCache::for_tests();
        // Lazy load: classify_package fills `installed` for Brew with
        // the env-seam contents.
        cache
            .classify_package(PackageManager::Brew, "ripgrep")
            .unwrap();
        // Re-bind env seam to a *different* value — if prewarm
        // re-probes, the cache would change.
        set_env("KERON_TEST_BREW_PACKAGES", "fd");
        let resources = vec![brew_pkg("ripgrep")];
        cache.prewarm(&resources).unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        // Cache still reflects the first probe's snapshot.
        assert_eq!(
            cache
                .classify_package(PackageManager::Brew, "ripgrep")
                .unwrap(),
            Action::NoOp,
        );
    }

    #[test]
    fn classify_package_returns_create_for_missing_then_noop_for_repeat() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_PACKAGES", "");
        let mut cache = PackageCache::for_tests();
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
    fn classify_package_returns_noop_when_already_installed() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_PACKAGES", "git,ripgrep,fd");
        let mut cache = PackageCache::for_tests();
        let action = cache
            .classify_package(PackageManager::Brew, "ripgrep")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        assert_eq!(action, Action::NoOp);
    }

    #[test]
    fn classify_package_never_returns_update_even_when_outdated() {
        // `keron apply` only ensures presence — upgrading installed
        // packages is left to the underlying manager. The classifier
        // therefore must not return Update for any package, regardless
        // of how stale it is locally. Pins the contract documented in
        // the module-level doc.
        let _g = lock_env();
        set_env("KERON_TEST_BREW_PACKAGES", "ripgrep");
        let mut cache = PackageCache::for_tests();
        let action = cache
            .classify_package(PackageManager::Brew, "ripgrep")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        assert_eq!(
            action,
            Action::NoOp,
            "installed brew package must classify NoOp, never Update",
        );
    }

    #[test]
    fn classify_package_strips_tap_prefix_for_installed_lookup() {
        let _g = lock_env();
        // `brew list` reports tap-installed `icepuma/keron/keron` as
        // bare `keron`. A manifest naming the qualified form must
        // still classify as NoOp.
        set_env("KERON_TEST_BREW_PACKAGES", "keron");
        let mut cache = PackageCache::for_tests();
        let action = cache
            .classify_package(PackageManager::Brew, "icepuma/keron/keron")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        assert_eq!(action, Action::NoOp);
    }

    #[test]
    fn classify_package_brew_cask_uses_its_own_installed_set() {
        let _g = lock_env();
        // Casks live in a separate namespace; a cask named "alacritty"
        // mustn't be confused with the formula "alacritty".
        set_env("KERON_TEST_BREW_PACKAGES", "git");
        set_env("KERON_TEST_BREW_CASK_PACKAGES", "alacritty");
        let mut cache = PackageCache::for_tests();
        let formula = cache
            .classify_package(PackageManager::Brew, "alacritty")
            .unwrap();
        let cask = cache
            .classify_package(PackageManager::BrewCask, "alacritty")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES", "KERON_TEST_BREW_CASK_PACKAGES"]);
        assert_eq!(formula, Action::Create);
        assert_eq!(cask, Action::NoOp);
    }

    #[test]
    fn classify_tap_returns_create_when_not_installed() {
        let _g = lock_env();
        set_env("KERON_TEST_BREW_TAPS", "");
        let mut cache = PackageCache::for_tests();
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
        let mut cache = PackageCache::for_tests();
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
        let mut cache = PackageCache::for_tests();
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
        let mut cache = PackageCache::for_tests();
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
        let mut cache = PackageCache::for_tests();
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
    fn install_many_rejects_empty_name() {
        let _g = lock_env();
        let err = install_many(PackageManager::Brew, &[""], OsFamily::Macos).unwrap_err();
        assert!(
            format!("{err:#}").contains("must not be empty"),
            "got: {err:#}",
        );
    }

    #[test]
    fn install_many_invocation_brew_collapses_into_one_argv() {
        let _g = lock_env();
        let (bin, args) = install_many_invocation(PackageManager::Brew, &["ripgrep", "bat", "fd"])
            .expect("brew supports multi-name install");
        assert_eq!(bin, "brew");
        assert_eq!(args, vec!["install", "ripgrep", "bat", "fd"]);
    }

    #[test]
    fn install_many_invocation_cask_keeps_flag_then_names() {
        let _g = lock_env();
        let (bin, args) =
            install_many_invocation(PackageManager::BrewCask, &["alacritty", "ghostty"])
                .expect("cask supports multi-name install");
        assert_eq!(bin, "brew");
        assert_eq!(args, vec!["install", "--cask", "alacritty", "ghostty"]);
    }

    #[test]
    fn install_many_invocation_cargo_collapses_into_one_argv() {
        let _g = lock_env();
        let (bin, args) =
            install_many_invocation(PackageManager::Cargo, &["sccache", "cargo-edit"])
                .expect("cargo supports multi-name install");
        assert_eq!(bin, "cargo");
        assert_eq!(args, vec!["install", "sccache", "cargo-edit"]);
    }

    #[test]
    fn install_many_invocation_winget_is_none_so_caller_loops() {
        // winget loses its `--id` field-restriction safety in batch
        // mode, so we deliberately keep its install path single-name.
        // The caller falls back to looping `install_invocation`
        // per-name internally.
        let _g = lock_env();
        assert!(
            install_many_invocation(PackageManager::Winget, &["Microsoft.PowerShell"]).is_none()
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
        let err = validate_package_manager_supported(PackageManager::Brew, OsFamily::Windows)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("brew"), "got: {msg}");
        assert!(msg.contains("Windows"), "got: {msg}");
        assert!(msg.contains("Linux or Macos"), "got: {msg}");
    }

    #[test]
    fn validate_package_manager_supported_accepts_matching_manager() {
        let _g = lock_env();
        validate_package_manager_supported(PackageManager::Winget, OsFamily::Windows).unwrap();
        validate_package_manager_supported(PackageManager::Cargo, OsFamily::Windows).unwrap();
    }

    #[test]
    fn validate_package_manager_brew_cask_is_macos_only() {
        let _g = lock_env();
        let err = validate_package_manager_supported(PackageManager::BrewCask, OsFamily::Linux)
            .unwrap_err();
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

    #[cfg(unix)]
    #[test]
    fn install_many_captured_concatenates_winget_per_name_chunks() {
        // Winget loops single-name installs internally (its install
        // command takes one query). When BatchStdio::Capture is in
        // effect, each chunk's stdout/stderr must be appended to the
        // returned BatchOutput so a downstream `flush_phase_outputs`
        // can render the full transcript. Pins the `== BatchStdio::Capture`
        // gate on line 547: an `==` -> `!=` mutation would only
        // accumulate output under Inherit (where every chunk is empty
        // by construction), leaving the captured buffer empty and the
        // user staring at a blank install log.
        use std::os::unix::fs::PermissionsExt;
        let _g = lock_env();
        let d = std::env::temp_dir().join(format!(
            "keron-imc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |t| t.subsec_nanos()),
        ));
        std::fs::create_dir_all(&d).unwrap();
        let spy = d.join("winget-spy.sh");
        // The spy echoes the package name (last argv) to stdout. The
        // winget invocation has the name at a known argv slot (we use
        // "$@"'s last positional) — easier to just splat the whole argv.
        let script = "#!/bin/sh\necho \"installed $@\"\n";
        std::fs::write(&spy, script).unwrap();
        let mut perm = std::fs::metadata(&spy).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&spy, perm).unwrap();
        set_env("KERON_TEST_PACKAGE_BIN_WINGET", spy.to_str().unwrap());

        let out = install_many_captured(
            PackageManager::Winget,
            &["Microsoft.PowerShell", "Foo.Bar"],
            crate::platform::OsFamily::Windows,
        )
        .expect("spy succeeds");

        clear_env(&["KERON_TEST_PACKAGE_BIN_WINGET"]);
        let _ = std::fs::remove_dir_all(&d);
        let stdout = String::from_utf8(out.stdout).unwrap();
        // Both per-name chunks must appear — concatenation proves the
        // capture-mode accumulation is wired up.
        assert!(
            stdout.contains("Microsoft.PowerShell"),
            "first chunk lost; got: {stdout:?}",
        );
        assert!(
            stdout.contains("Foo.Bar"),
            "second chunk lost; got: {stdout:?}",
        );
    }
}
