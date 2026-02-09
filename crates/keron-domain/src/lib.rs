use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ManifestId {
    pub path: PathBuf,
}

impl ManifestId {
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSpec {
    pub id: ManifestId,
    pub dependencies: Vec<PathBuf>,
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
    pub dest: PathBuf,
    pub force: bool,
    pub mkdirs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateResource {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub vars: BTreeMap<String, String>,
    pub force: bool,
    pub mkdirs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageResource {
    pub name: String,
    pub provider_hint: Option<String>,
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
