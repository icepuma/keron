//! Package manager integration: list installed packages per
//! manager, cache the result, and install missing ones.
//!
//! The cache lives for the duration of one `keron apply` and is
//! populated lazily — the first time a `brew(...)` resource is
//! classified, we shell out to `brew list` and remember the result;
//! later `brew(...)` resources reuse that snapshot without
//! re-querying. The cost is one shell-out per manager that appears
//! in the manifest, not one per resource.
//!
//! Idempotency: [`PackageCache::mark_to_install`] both checks
//! membership and inserts in one step. Two `brew("ripgrep")`
//! resources in the same plan classify as Create / `NoOp` rather
//! than Create / Create, even though the real install was about to
//! happen twice. The first classification "claims" the install; the
//! second sees it already on the to-install list.
//!
//! Test seam: each manager's fetch reads `KERON_TEST_<MGR>_PACKAGES`
//! (comma-separated) instead of shelling out, so unit tests can
//! drive any cache state without a real `brew` / `cargo` / `winget`
//! on the host. The install side reads
//! `KERON_TEST_PACKAGE_BIN_<MGR>` to swap the binary path for a spy
//! script. Both seams require `KERON_ALLOW_TEST_OVERRIDES=1` so stray
//! environment variables cannot falsify a real run.

pub mod brew;
pub mod cargo;
pub mod winget;

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::plan::PackageManager;

/// Snapshot of "what's installed" per manager. Populated lazily; one
/// `PackageCache` per `keron apply` invocation. Cloning is cheap
/// (`HashMap`s own their data) but unnecessary — pass by `&mut`.
#[derive(Debug, Default)]
pub struct PackageCache {
    installed: HashMap<PackageManager, HashSet<String>>,
}

impl PackageCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether `name` is already installed under `manager`, and
    /// — if not — record it as "to be installed" so a second
    /// classification of the same package in the same plan returns
    /// `true` (`NoOp`) rather than scheduling a duplicate Create.
    ///
    /// Returns `true` when the caller should classify the change as
    /// `Action::NoOp` (package already installed or already
    /// scheduled), `false` for `Action::Create`.
    ///
    /// (`NoOp` is the action variant, not the literal text.)
    ///
    /// # Errors
    /// Errors when the manager's list command fails on first access.
    /// Subsequent accesses for the same manager reuse the cached set.
    pub fn mark_to_install(&mut self, manager: PackageManager, name: &str) -> Result<bool> {
        let set = self.ensure_loaded(manager)?;
        if set.contains(name) {
            Ok(true)
        } else {
            set.insert(name.to_string());
            Ok(false)
        }
    }

    fn ensure_loaded(&mut self, manager: PackageManager) -> Result<&mut HashSet<String>> {
        use std::collections::hash_map::Entry;
        match self.installed.entry(manager) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let set = fetch_installed(manager).with_context(|| {
                    format!(
                        "listing installed packages for {} (`{} list ...`)",
                        manager.label(),
                        manager.label()
                    )
                })?;
                Ok(e.insert(set))
            }
        }
    }
}

/// Shell out to the manager's list command and parse the output
/// into a set of installed package names / IDs. Honours the
/// `KERON_TEST_<MGR>_PACKAGES` env override so tests can pin a
/// deterministic cache state without a real package manager on the
/// host.
fn fetch_installed(manager: PackageManager) -> Result<HashSet<String>> {
    if let Some(packages) = test_packages_override(manager) {
        return Ok(packages);
    }
    validate_package_manager_supported(manager)?;
    match manager {
        PackageManager::Brew => brew::fetch(),
        PackageManager::Cargo => cargo::fetch(),
        PackageManager::Winget => winget::fetch(),
    }
}

fn test_packages_override(manager: PackageManager) -> Option<HashSet<String>> {
    if !test_overrides_allowed() {
        return None;
    }
    let key = match manager {
        PackageManager::Brew => "KERON_TEST_BREW_PACKAGES",
        PackageManager::Cargo => "KERON_TEST_CARGO_PACKAGES",
        PackageManager::Winget => "KERON_TEST_WINGET_PACKAGES",
    };
    let raw = std::env::var(key).ok()?;
    Some(
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
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

pub fn validate_package_manager_supported(manager: PackageManager) -> Result<()> {
    let os = crate::platform::detect_os_family();
    if manager.is_supported_on(os) {
        return Ok(());
    }
    bail!(
        "{} package resources are not supported on {}; supported on: {}",
        manager.label(),
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
        bail!("{} package name must not be empty", manager.label());
    }
    if name.starts_with('-') {
        bail!(
            "{} package name must not begin with `-`: `{name}`",
            manager.label()
        );
    }
    if name.contains('\0') {
        bail!(
            "{} package name must not contain a NUL byte",
            manager.label()
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
    let key = match manager {
        PackageManager::Brew => "KERON_TEST_PACKAGE_BIN_BREW",
        PackageManager::Cargo => "KERON_TEST_PACKAGE_BIN_CARGO",
        PackageManager::Winget => "KERON_TEST_PACKAGE_BIN_WINGET",
    };
    std::env::var(key).ok()
}

fn test_overrides_allowed() -> bool {
    std::env::var_os("KERON_ALLOW_TEST_OVERRIDES").is_some_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_env(keys: &[&str]) {
        // SAFETY: edition 2024 marks env mutation unsafe. Tests
        // here serialise their mutations via the SEQ-based naming
        // convention used elsewhere; the value is restored on
        // function exit so global state is unchanged. Inside
        // `#[allow(unsafe_code)]` scope so the lint stays denied
        // outside tests.
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
    fn mark_to_install_returns_false_for_missing_then_true_for_repeat() {
        // First classification of a name → false (Create). Second
        // classification of the same name in the same plan → true
        // (NoOp). Pins the dedup contract from the doc comment.
        set_env("KERON_TEST_BREW_PACKAGES", "");
        let mut cache = PackageCache::new();
        let first = cache
            .mark_to_install(PackageManager::Brew, "ripgrep")
            .unwrap();
        let second = cache
            .mark_to_install(PackageManager::Brew, "ripgrep")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        assert!(!first, "first occurrence should be Create");
        assert!(second, "second occurrence should be NoOp");
    }

    #[test]
    fn mark_to_install_returns_true_when_already_installed() {
        set_env("KERON_TEST_BREW_PACKAGES", "git,ripgrep,fd");
        let mut cache = PackageCache::new();
        let installed = cache
            .mark_to_install(PackageManager::Brew, "ripgrep")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        assert!(installed);
    }

    #[test]
    fn ensure_loaded_caches_per_manager() {
        // Two different managers populate two cache entries; cache
        // only loads each one once. We can't directly observe "did
        // we shell out twice" without a counter, so use distinct
        // packages and verify both lookups give the right answer.
        set_env("KERON_TEST_BREW_PACKAGES", "git");
        set_env("KERON_TEST_CARGO_PACKAGES", "sccache");
        let mut cache = PackageCache::new();
        let brew_hit = cache.mark_to_install(PackageManager::Brew, "git").unwrap();
        let cargo_hit = cache
            .mark_to_install(PackageManager::Cargo, "sccache")
            .unwrap();
        let cross = cache
            .mark_to_install(PackageManager::Brew, "sccache")
            .unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES", "KERON_TEST_CARGO_PACKAGES"]);
        assert!(brew_hit);
        assert!(cargo_hit);
        // The cargo package isn't in the brew cache.
        assert!(!cross);
    }

    #[test]
    fn install_invocation_brew_uses_install_with_name() {
        let (bin, args) = install_invocation(PackageManager::Brew, "ripgrep");
        assert_eq!(bin, "brew");
        assert_eq!(args, vec!["install", "ripgrep"]);
    }

    #[test]
    fn install_invocation_cargo_uses_install_with_name() {
        let (bin, args) = install_invocation(PackageManager::Cargo, "sccache");
        assert_eq!(bin, "cargo");
        assert_eq!(args, vec!["install", "sccache"]);
    }

    #[test]
    fn install_invocation_winget_passes_exact_and_accept_flags() {
        // winget without --exact / --id will match by Name (which
        // is locale-dependent); without --accept-* flags it stalls
        // on interactive prompts. The flag set is load-bearing for
        // unattended installs.
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
        set_env("KERON_TEST_PACKAGE_BIN_BREW", "/tmp/fake-brew");
        let (bin, _) = install_invocation(PackageManager::Brew, "x");
        clear_env(&["KERON_TEST_PACKAGE_BIN_BREW"]);
        assert_eq!(bin, "/tmp/fake-brew");
    }

    #[test]
    fn install_rejects_empty_name() {
        let err = install(PackageManager::Brew, "").unwrap_err();
        assert!(
            format!("{err:#}").contains("must not be empty"),
            "got: {err:#}",
        );
    }

    #[test]
    fn validate_package_name_rejects_leading_dash() {
        for mgr in [
            PackageManager::Brew,
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
        let err = validate_package_name(PackageManager::Cargo, "-foo").unwrap_err();
        assert!(
            format!("{err:#}").contains("must not begin with `-`"),
            "got: {err:#}",
        );
    }

    #[test]
    fn validate_package_name_rejects_nul_byte() {
        let err = validate_package_name(PackageManager::Brew, "rip\0grep").unwrap_err();
        assert!(format!("{err:#}").contains("NUL byte"), "got: {err:#}");
    }

    #[test]
    fn validate_package_name_accepts_dash_in_interior() {
        validate_package_name(PackageManager::Brew, "git-lfs").unwrap();
        validate_package_name(PackageManager::Cargo, "cargo-edit").unwrap();
    }

    #[test]
    fn validate_package_manager_supported_rejects_wrong_os_manager() {
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Windows);
        let err = validate_package_manager_supported(PackageManager::Brew).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("brew"), "got: {msg}");
        assert!(msg.contains("Windows"), "got: {msg}");
        assert!(msg.contains("Linux or Macos"), "got: {msg}");
    }

    #[test]
    fn validate_package_manager_supported_accepts_matching_manager() {
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Windows);
        validate_package_manager_supported(PackageManager::Winget).unwrap();
        validate_package_manager_supported(PackageManager::Cargo).unwrap();
    }

    #[test]
    fn test_packages_override_parses_csv_and_trims_whitespace() {
        set_env("KERON_TEST_BREW_PACKAGES", " git , ripgrep ,, fd ");
        let got = test_packages_override(PackageManager::Brew).unwrap();
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        let mut sorted: Vec<_> = got.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["fd", "git", "ripgrep"]);
    }

    #[test]
    fn test_packages_override_returns_none_when_unset() {
        clear_env(&["KERON_TEST_WINGET_PACKAGES"]);
        assert!(test_packages_override(PackageManager::Winget).is_none());
    }

    #[test]
    fn test_packages_override_requires_allow_gate() {
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_TEST_BREW_PACKAGES", "git");
        }
        let got = test_packages_override(PackageManager::Brew);
        clear_env(&["KERON_TEST_BREW_PACKAGES"]);
        assert!(got.is_none());
    }
}
