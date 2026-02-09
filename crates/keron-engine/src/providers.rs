use std::collections::{BTreeSet, HashMap, HashSet};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

pub trait PackageProvider {
    fn name(&self) -> &'static str;
    fn detect(&self) -> bool;
    fn is_installed(&self, package: &str) -> Result<bool>;
    fn installed_packages(&self) -> Result<HashSet<String>>;
    fn install(&self, package: &str) -> Result<()>;
    fn remove(&self, package: &str) -> Result<()>;
}

#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Box<dyn PackageProvider>>,
}

impl ProviderRegistry {
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            providers: vec![
                Box::new(BrewProvider),
                Box::new(AptProvider),
                Box::new(WingetProvider),
            ],
        }
    }

    #[must_use]
    pub fn from_providers(providers: Vec<Box<dyn PackageProvider>>) -> Self {
        Self { providers }
    }

    #[must_use]
    pub fn supported_names(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .map(|provider| provider.name())
            .collect()
    }

    #[must_use]
    pub fn is_supported(&self, name: &str) -> bool {
        self.providers
            .iter()
            .any(|provider| provider.name().eq_ignore_ascii_case(name))
    }

    #[must_use]
    pub fn is_available(&self, name: &str) -> bool {
        self.providers
            .iter()
            .any(|provider| provider.name().eq_ignore_ascii_case(name) && provider.detect())
    }

    #[must_use]
    pub fn any_available(&self) -> bool {
        self.providers.iter().any(|provider| provider.detect())
    }

    pub fn select(&self, hint: Option<&str>) -> Option<&dyn PackageProvider> {
        if let Some(hint) = hint {
            for provider in &self.providers {
                if provider.name().eq_ignore_ascii_case(hint) && provider.detect() {
                    return Some(provider.as_ref());
                }
            }
            return None;
        }

        self.providers
            .iter()
            .find(|provider| provider.detect())
            .map(std::ops::Deref::deref)
    }
}

fn run_status(program: &str, args: &[&str]) -> Result<std::process::ExitStatus> {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to execute {program} {}", args.join(" ")))
}

fn run_stdout(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("failed to execute {program} {}", args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "command failed: {program} {} (exit: {})",
            args.join(" "),
            output.status
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn ensure_success(program: &str, args: &[&str]) -> Result<()> {
    let status = run_status(program, args)?;
    if status.success() {
        Ok(())
    } else {
        bail!(
            "command failed: {program} {} (exit: {status})",
            args.join(" ")
        );
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

    fn is_installed(&self, package: &str) -> Result<bool> {
        let status = run_status("brew", &["list", "--formula", package])?;
        Ok(status.success())
    }

    fn installed_packages(&self) -> Result<HashSet<String>> {
        let stdout = run_stdout("brew", &["list", "--formula", "--versions"])?;
        Ok(stdout
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_string)
            .collect())
    }

    fn install(&self, package: &str) -> Result<()> {
        ensure_success("brew", &["install", package])
    }

    fn remove(&self, package: &str) -> Result<()> {
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

    fn is_installed(&self, package: &str) -> Result<bool> {
        let status = run_status("dpkg", &["-s", package])?;
        Ok(status.success())
    }

    fn installed_packages(&self) -> Result<HashSet<String>> {
        let stdout = run_stdout("dpkg-query", &["-W", "-f=${binary:Package}\\n"])?;
        Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn install(&self, package: &str) -> Result<()> {
        ensure_success("apt-get", &["install", "-y", package])
    }

    fn remove(&self, package: &str) -> Result<()> {
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

    fn is_installed(&self, package: &str) -> Result<bool> {
        let status = run_status("winget", &["list", "--exact", "--id", package])?;
        Ok(status.success())
    }

    fn installed_packages(&self) -> Result<HashSet<String>> {
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

    fn install(&self, package: &str) -> Result<()> {
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

    fn remove(&self, package: &str) -> Result<()> {
        ensure_success("winget", &["uninstall", "--exact", "--id", package])
    }
}

pub fn package_state(
    registry: &ProviderRegistry,
    package_name: &str,
    hint: Option<&str>,
) -> Result<(String, bool)> {
    let Some(provider) = registry.select(hint) else {
        return Err(anyhow!("no package provider available (hint: {hint:?})"));
    };
    let installed = provider.is_installed(package_name)?;
    Ok((provider.name().to_string(), installed))
}

pub fn package_states_bulk(
    registry: &ProviderRegistry,
    hint: Option<&str>,
    package_names: &BTreeSet<String>,
) -> Result<(String, HashMap<String, bool>)> {
    let Some(provider) = registry.select(hint) else {
        return Err(anyhow!("no package provider available (hint: {hint:?})"));
    };

    let installed = provider.installed_packages()?;
    let states = package_names
        .iter()
        .map(|package| (package.clone(), installed.contains(package)))
        .collect();

    Ok((provider.name().to_string(), states))
}

pub fn apply_package(
    registry: &ProviderRegistry,
    package_name: &str,
    hint: Option<&str>,
    install: bool,
) -> Result<String> {
    let Some(provider) = registry.select(hint) else {
        return Err(anyhow!("no package provider available (hint: {hint:?})"));
    };

    if install {
        provider.install(package_name)?;
    } else {
        provider.remove(package_name)?;
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
