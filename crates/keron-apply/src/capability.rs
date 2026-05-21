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

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::plan::{PackageManager, Plan, ResourceChange, ResourceState};

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
    }

    impl MockEnvProbe {
        fn with_binary(mut self, name: &str) -> Self {
            self.binaries.push(name.into());
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
}
