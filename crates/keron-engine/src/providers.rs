use std::collections::{BTreeSet, HashMap, HashSet};
use std::process::{Command, ExitStatus, Stdio};

use keron_domain::PackageName;

use crate::error::ProviderError;

type ProviderResult<T> = std::result::Result<T, ProviderError>;

pub trait PackageProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn detect(&self) -> bool;
    fn is_installed(&self, package: &str) -> ProviderResult<bool>;
    fn installed_packages(&self) -> ProviderResult<HashSet<String>>;
    fn install(&self, package: &str) -> ProviderResult<()>;
    fn remove(&self, package: &str) -> ProviderResult<()>;
}

#[derive(Debug, Clone)]
pub struct ProviderSnapshot {
    supported_names: Vec<String>,
    supported_by_name: HashSet<String>,
    available_by_name: HashMap<String, usize>,
    default_available: Option<usize>,
}

impl ProviderSnapshot {
    #[must_use]
    pub fn supported_names(&self) -> &[String] {
        &self.supported_names
    }

    #[must_use]
    pub fn is_supported(&self, name: &str) -> bool {
        self.supported_by_name.contains(&name.to_ascii_lowercase())
    }

    #[must_use]
    pub fn is_available(&self, name: &str) -> bool {
        self.available_by_name
            .contains_key(&name.to_ascii_lowercase())
    }

    #[must_use]
    pub fn any_available(&self) -> bool {
        self.default_available.is_some()
    }
}

#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Box<dyn PackageProvider + Send + Sync>>,
}

impl ProviderRegistry {
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            providers: vec![
                Box::new(BrewProvider),
                Box::new(AptProvider),
                Box::new(WingetProvider),
                Box::new(CargoProvider),
            ],
        }
    }

    #[must_use]
    pub fn from_providers(providers: Vec<Box<dyn PackageProvider + Send + Sync>>) -> Self {
        Self { providers }
    }

    #[must_use]
    pub fn snapshot(&self) -> ProviderSnapshot {
        let mut supported_names = Vec::with_capacity(self.providers.len());
        let mut supported_by_name = HashSet::with_capacity(self.providers.len());
        let mut available_by_name = HashMap::new();
        let mut default_available = None;

        for (index, provider) in self.providers.iter().enumerate() {
            let provider_name = provider.name();
            supported_names.push(provider_name.to_string());
            supported_by_name.insert(provider_name.to_ascii_lowercase());
            if provider.detect() {
                available_by_name.insert(provider_name.to_ascii_lowercase(), index);
                if default_available.is_none() {
                    default_available = Some(index);
                }
            }
        }

        ProviderSnapshot {
            supported_names,
            supported_by_name,
            available_by_name,
            default_available,
        }
    }

    fn select_with_snapshot<'a>(
        &'a self,
        snapshot: &ProviderSnapshot,
        hint: Option<&str>,
    ) -> Option<&'a (dyn PackageProvider + Send + Sync)> {
        if let Some(hint) = hint {
            let index = snapshot.available_by_name.get(&hint.to_ascii_lowercase())?;
            return self.providers.get(*index).map(std::ops::Deref::deref);
        }

        snapshot
            .default_available
            .and_then(|index| self.providers.get(index))
            .map(std::ops::Deref::deref)
    }
}

fn run_status(program: &'static str, args: &[&str]) -> ProviderResult<ExitStatus> {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| ProviderError::CommandSpawn {
            program,
            args: args.join(" "),
            source,
        })
}

fn run_stdout(program: &'static str, args: &[&str]) -> ProviderResult<String> {
    let output = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|source| ProviderError::CommandSpawn {
            program,
            args: args.join(" "),
            source,
        })?;

    if !output.status.success() {
        return Err(ProviderError::CommandFailed {
            program,
            args: args.join(" "),
            status: output.status,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn ensure_success(program: &'static str, args: &[&str]) -> ProviderResult<()> {
    let status = run_status(program, args)?;
    if status.success() {
        Ok(())
    } else {
        Err(ProviderError::CommandFailed {
            program,
            args: args.join(" "),
            status,
        })
    }
}

struct BrewProvider;

impl PackageProvider for BrewProvider {
    fn name(&self) -> &'static str {
        "brew"
    }

    fn detect(&self) -> bool {
        which::which("brew").is_ok()
    }

    fn is_installed(&self, package: &str) -> ProviderResult<bool> {
        let status = run_status("brew", &["list", "--formula", package])?;
        Ok(status.success())
    }

    fn installed_packages(&self) -> ProviderResult<HashSet<String>> {
        let stdout = run_stdout("brew", &["list", "--formula", "--versions"])?;
        Ok(stdout
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_string)
            .collect())
    }

    fn install(&self, package: &str) -> ProviderResult<()> {
        ensure_success("brew", &["install", package])
    }

    fn remove(&self, package: &str) -> ProviderResult<()> {
        ensure_success("brew", &["uninstall", package])
    }
}

struct AptProvider;

impl PackageProvider for AptProvider {
    fn name(&self) -> &'static str {
        "apt"
    }

    fn detect(&self) -> bool {
        which::which("apt-get").is_ok() && which::which("dpkg").is_ok()
    }

    fn is_installed(&self, package: &str) -> ProviderResult<bool> {
        let status = run_status("dpkg", &["-s", package])?;
        Ok(status.success())
    }

    fn installed_packages(&self) -> ProviderResult<HashSet<String>> {
        let stdout = run_stdout("dpkg-query", &["-W", "-f=${binary:Package}\\n"])?;
        Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn install(&self, package: &str) -> ProviderResult<()> {
        ensure_success("apt-get", &["install", "-y", package])
    }

    fn remove(&self, package: &str) -> ProviderResult<()> {
        ensure_success("apt-get", &["remove", "-y", package])
    }
}

struct WingetProvider;

impl PackageProvider for WingetProvider {
    fn name(&self) -> &'static str {
        "winget"
    }

    fn detect(&self) -> bool {
        which::which("winget").is_ok()
    }

    fn is_installed(&self, package: &str) -> ProviderResult<bool> {
        let status = run_status("winget", &["list", "--exact", "--id", package])?;
        Ok(status.success())
    }

    fn installed_packages(&self) -> ProviderResult<HashSet<String>> {
        let stdout = run_stdout(
            "winget",
            &[
                "list",
                "--accept-source-agreements",
                "--disable-interactivity",
            ],
        )?;

        let mut packages = HashSet::new();
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("Name") || is_separator_line(trimmed) {
                continue;
            }

            let columns = split_columns(trimmed);
            if let Some(id) = columns.get(1) {
                let id = id.trim();
                if !id.is_empty() {
                    packages.insert(id.to_string());
                }
            }
        }

        Ok(packages)
    }

    fn install(&self, package: &str) -> ProviderResult<()> {
        ensure_success(
            "winget",
            &[
                "install",
                "--exact",
                "--id",
                package,
                "--accept-source-agreements",
                "--accept-package-agreements",
            ],
        )
    }

    fn remove(&self, package: &str) -> ProviderResult<()> {
        ensure_success("winget", &["uninstall", "--exact", "--id", package])
    }
}

struct CargoProvider;

impl PackageProvider for CargoProvider {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn detect(&self) -> bool {
        which::which("cargo").is_ok()
    }

    fn is_installed(&self, package: &str) -> ProviderResult<bool> {
        let installed = self.installed_packages()?;
        Ok(installed.contains(package))
    }

    fn installed_packages(&self) -> ProviderResult<HashSet<String>> {
        let stdout = run_stdout("cargo", &["install", "--list"])?;
        Ok(parse_cargo_installed_packages(&stdout))
    }

    fn install(&self, package: &str) -> ProviderResult<()> {
        ensure_success("cargo", &["install", package])
    }

    fn remove(&self, package: &str) -> ProviderResult<()> {
        ensure_success("cargo", &["uninstall", package])
    }
}

pub fn package_state(
    registry: &ProviderRegistry,
    snapshot: &ProviderSnapshot,
    package_name: &PackageName,
    hint: Option<&str>,
) -> ProviderResult<(String, bool)> {
    let Some(provider) = registry.select_with_snapshot(snapshot, hint) else {
        return Err(ProviderError::NoProviderAvailable {
            hint: hint.map(str::to_string),
        });
    };
    let installed = provider.is_installed(package_name.as_str())?;
    Ok((provider.name().to_string(), installed))
}

pub fn package_states_bulk(
    registry: &ProviderRegistry,
    snapshot: &ProviderSnapshot,
    hint: Option<&str>,
    package_names: &BTreeSet<PackageName>,
) -> ProviderResult<(String, HashMap<PackageName, bool>)> {
    let Some(provider) = registry.select_with_snapshot(snapshot, hint) else {
        return Err(ProviderError::NoProviderAvailable {
            hint: hint.map(str::to_string),
        });
    };

    let installed = provider.installed_packages()?;
    let states = package_names
        .iter()
        .map(|package| (package.clone(), installed.contains(package.as_str())))
        .collect();

    Ok((provider.name().to_string(), states))
}

pub fn apply_package(
    registry: &ProviderRegistry,
    snapshot: &ProviderSnapshot,
    package_name: &PackageName,
    hint: Option<&str>,
    install: bool,
) -> ProviderResult<String> {
    let Some(provider) = registry.select_with_snapshot(snapshot, hint) else {
        return Err(ProviderError::NoProviderAvailable {
            hint: hint.map(str::to_string),
        });
    };

    if install {
        provider.install(package_name.as_str())?;
    } else {
        provider.remove(package_name.as_str())?;
    }

    Ok(provider.name().to_string())
}

fn split_columns(line: &str) -> Vec<&str> {
    let mut columns = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let bytes = line.as_bytes();

    while i < bytes.len() {
        if bytes[i] == b' ' {
            let mut j = i;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }

            if j - i >= 2 {
                let field = line[start..i].trim();
                if !field.is_empty() {
                    columns.push(field);
                }
                start = j;
            }

            i = j;
            continue;
        }

        i += 1;
    }

    let tail = line[start..].trim();
    if !tail.is_empty() {
        columns.push(tail);
    }

    columns
}

fn is_separator_line(line: &str) -> bool {
    line.chars().all(|ch| ch == '-' || ch == ' ')
}

fn parse_cargo_installed_packages(stdout: &str) -> HashSet<String> {
    stdout
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || line.starts_with(char::is_whitespace) {
                return None;
            }
            if !trimmed.ends_with(':') {
                return None;
            }
            trimmed
                .split_whitespace()
                .next()
                .map(|name| name.trim_end_matches(':').to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::parse_cargo_installed_packages;

    #[test]
    fn parses_cargo_install_list_output() {
        let output = r"
ripgrep v14.1.1:
    rg
cargo-watch v8.5.3:
    cargo-watch
serde-json-fmt v0.1.0 (path+file:///tmp/serde-json-fmt):
    serde-json-fmt
";
        let parsed = parse_cargo_installed_packages(output);

        let expected = HashSet::from([
            "ripgrep".to_string(),
            "cargo-watch".to_string(),
            "serde-json-fmt".to_string(),
        ]);
        assert_eq!(parsed, expected);
    }
}
