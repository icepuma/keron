use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainValidationError {
    #[error("path must be absolute: {path}")]
    PathMustBeAbsolute { path: PathBuf },
    #[error("package name must not be empty")]
    EmptyPackageName,
    #[error("package manager name must not be empty")]
    EmptyPackageManagerName,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "PathBuf", into = "PathBuf")]
pub struct AbsolutePath(PathBuf);

impl AbsolutePath {
    /// Create an absolute path wrapper, rejecting relative paths.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` is not absolute.
    pub fn new(path: PathBuf) -> Result<Self, DomainValidationError> {
        if path.is_absolute() {
            Ok(Self(path))
        } else {
            Err(DomainValidationError::PathMustBeAbsolute { path })
        }
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    #[must_use]
    pub fn into_inner(self) -> PathBuf {
        self.0
    }
}

impl TryFrom<PathBuf> for AbsolutePath {
    type Error = DomainValidationError;

    fn try_from(value: PathBuf) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&Path> for AbsolutePath {
    type Error = DomainValidationError;

    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        Self::new(value.to_path_buf())
    }
}

impl AsRef<Path> for AbsolutePath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl Deref for AbsolutePath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_path()
    }
}

impl fmt::Display for AbsolutePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(formatter)
    }
}

impl From<AbsolutePath> for PathBuf {
    fn from(value: AbsolutePath) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PackageName(String);

impl PackageName {
    /// Create a package name wrapper, rejecting blank names.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is empty after trimming.
    pub fn new(name: String) -> Result<Self, DomainValidationError> {
        if name.trim().is_empty() {
            Err(DomainValidationError::EmptyPackageName)
        } else {
            Ok(Self(name))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PackageName {
    type Error = DomainValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for PackageName {
    type Error = DomainValidationError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value.to_string())
    }
}

impl AsRef<str> for PackageName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for PackageName {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<PackageName> for String {
    fn from(value: PackageName) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PackageManagerName(String);

impl PackageManagerName {
    /// Create a package manager name wrapper, rejecting blank names.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is empty after trimming.
    pub fn new(name: String) -> Result<Self, DomainValidationError> {
        if name.trim().is_empty() {
            Err(DomainValidationError::EmptyPackageManagerName)
        } else {
            Ok(Self(name))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PackageManagerName {
    type Error = DomainValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for PackageManagerName {
    type Error = DomainValidationError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value.to_string())
    }
}

impl AsRef<str> for PackageManagerName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for PackageManagerName {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for PackageManagerName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<PackageManagerName> for String {
    fn from(value: PackageManagerName) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ManifestPath(PathBuf);

impl ManifestPath {
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self(path)
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        self.0.clone()
    }

    #[must_use]
    pub fn into_inner(self) -> PathBuf {
        self.0
    }
}

impl From<PathBuf> for ManifestPath {
    fn from(value: PathBuf) -> Self {
        Self::new(value)
    }
}

impl From<&Path> for ManifestPath {
    fn from(value: &Path) -> Self {
        Self::new(value.to_path_buf())
    }
}

impl From<ManifestPath> for PathBuf {
    fn from(value: ManifestPath) -> Self {
        value.0
    }
}

impl AsRef<Path> for ManifestPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl Deref for ManifestPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_path()
    }
}

impl fmt::Display for ManifestPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ManifestId {
    pub path: ManifestPath,
}

impl ManifestId {
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self {
            path: ManifestPath::new(path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSpec {
    pub id: ManifestId,
    pub dependencies: Vec<ManifestPath>,
    pub resources: Vec<Resource>,
}

impl ManifestSpec {
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self {
            id: ManifestId::new(path),
            dependencies: Vec::new(),
            resources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resource", rename_all = "snake_case")]
pub enum Resource {
    Link(LinkResource),
    Template(TemplateResource),
    Package(PackageResource),
    Command(CommandResource),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkResource {
    pub src: PathBuf,
    pub dest: AbsolutePath,
    pub force: bool,
    pub mkdirs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateResource {
    pub src: PathBuf,
    pub dest: AbsolutePath,
    pub vars: BTreeMap<String, String>,
    pub force: bool,
    pub mkdirs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageResource {
    pub name: PackageName,
    pub provider_hint: Option<PackageManagerName>,
    pub state: PackageState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PackageState {
    #[default]
    Present,
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResource {
    pub binary: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    LinkCreate,
    LinkReplace,
    LinkNoop,
    LinkConflict,
    TemplateCreate,
    TemplateUpdate,
    TemplateNoop,
    TemplateConflict,
    PackageInstall,
    PackageRemove,
    PackageNoop,
    CommandRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedOperation {
    pub id: usize,
    pub manifest: PathBuf,
    pub action: PlanAction,
    pub resource: Resource,
    pub summary: String,
    pub would_change: bool,
    pub conflict: bool,
    pub hint: Option<String>,
    pub error: Option<String>,
    pub content_hash: Option<String>,
    pub dest_content_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
    Hint,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationDiagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
}

impl PlannedOperation {
    #[must_use]
    pub fn diagnostics(&self) -> Vec<OperationDiagnostic> {
        let mut diagnostics = Vec::new();
        if let Some(hint) = &self.hint {
            diagnostics.push(OperationDiagnostic {
                level: DiagnosticLevel::Hint,
                message: hint.clone(),
            });
        }
        if let Some(error) = &self.error {
            diagnostics.push(OperationDiagnostic {
                level: DiagnosticLevel::Error,
                message: error.clone(),
            });
        }
        diagnostics
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReport {
    pub discovered_manifests: Vec<PathBuf>,
    pub execution_order: Vec<PathBuf>,
    pub operations: Vec<PlannedOperation>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl PlanReport {
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty() || self.operations.iter().any(|op| op.error.is_some())
    }

    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        self.operations.iter().any(|op| op.conflict)
    }

    #[must_use]
    pub fn has_drift(&self) -> bool {
        self.operations
            .iter()
            .any(|op| op.would_change || op.conflict || op.error.is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyOperationResult {
    pub operation_id: usize,
    pub summary: String,
    pub success: bool,
    pub changed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub plan: PlanReport,
    pub results: Vec<ApplyOperationResult>,
    pub errors: Vec<String>,
}

impl ApplyReport {
    #[must_use]
    pub fn has_failures(&self) -> bool {
        !self.errors.is_empty() || self.results.iter().any(|result| !result.success)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{AbsolutePath, DomainValidationError, PackageManagerName, PackageName};

    #[test]
    fn absolute_path_rejects_relative_values() {
        let error = AbsolutePath::try_from(std::path::PathBuf::from("relative/path"))
            .expect_err("relative paths must be rejected");
        assert!(matches!(
            error,
            DomainValidationError::PathMustBeAbsolute { .. }
        ));
    }

    #[test]
    fn package_name_rejects_blank_values() {
        let error = PackageName::try_from("   ").expect_err("blank names must be rejected");
        assert!(matches!(error, DomainValidationError::EmptyPackageName));
    }

    #[test]
    fn package_manager_name_rejects_blank_values() {
        let error =
            PackageManagerName::try_from(" ").expect_err("blank manager names must be rejected");
        assert!(matches!(
            error,
            DomainValidationError::EmptyPackageManagerName
        ));
    }
}
