use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use keron_domain::{
    CommandResource, LinkResource, ManifestSpec, PackageName, PackageResource, PackageState,
    PlanAction, PlanReport, PlannedOperation, Resource, TemplateResource,
};

use crate::error::{PlanningError, ProviderError};
use crate::fs_util::{path_exists_including_dangling_symlink, symlink_points_to};
use crate::providers::{ProviderRegistry, ProviderSnapshot, package_states_bulk};
use crate::template::render_template_string;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
struct PackageQueryKey {
    name: PackageName,
    provider_hint: Option<String>,
}

type PackageDiagnostics = HashMap<PackageQueryKey, Vec<String>>;
type PackageStateStatus = Result<(String, bool), PackageStateError>;
type PackageStateEntry = (PackageQueryKey, PackageStateStatus);

#[derive(Debug, Clone, Error)]
enum PackageStateError {
    #[error(transparent)]
    Provider(#[from] Arc<ProviderError>),
}

#[derive(Debug, Error)]
enum LinkInspectError {
    #[error("failed to inspect link {label} {path}")]
    Path {
        label: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to inspect link destination {dest}")]
    Target {
        dest: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Clone)]
struct OperationContext {
    id: usize,
    manifest_path: PathBuf,
    resource: Resource,
}

impl OperationContext {
    const fn new(id: usize, manifest_path: PathBuf, resource: Resource) -> Self {
        Self {
            id,
            manifest_path,
            resource,
        }
    }

    fn planned_operation(&self, decision: OperationDecision) -> PlannedOperation {
        PlannedOperation {
            id: self.id,
            manifest: self.manifest_path.clone(),
            action: decision.action,
            resource: self.resource.clone(),
            summary: decision.summary,
            would_change: decision.would_change,
            conflict: decision.conflict,
            hint: decision.hint,
            error: decision.error,
            content_hash: decision.content_hash,
            dest_content_hash: decision.dest_content_hash,
        }
    }
}

#[derive(Debug, Clone)]
struct OperationDecision {
    action: PlanAction,
    summary: String,
    would_change: bool,
    conflict: bool,
    hint: Option<String>,
    error: Option<String>,
    content_hash: Option<String>,
    dest_content_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct PlanWorkItem {
    id: usize,
    manifest_path: PathBuf,
    resource: Resource,
}

/// Build a plan report from discovered manifests and resolved execution order.
///
/// # Errors
///
/// Returns an error when provider planning fails unexpectedly.
pub fn build_plan(
    discovered_manifests: &[PathBuf],
    specs: &[ManifestSpec],
    execution_order: &[PathBuf],
    providers: &ProviderRegistry,
) -> std::result::Result<(PlanReport, BTreeSet<String>), PlanningError> {
    let spec_map: HashMap<_, _> = specs
        .iter()
        .map(|manifest| (manifest.id.path.to_path_buf(), manifest))
        .collect();

    let provider_snapshot = providers.snapshot();
    let (package_diagnostics, package_states) = rayon::join(
        || collect_package_diagnostics(specs, &provider_snapshot),
        || collect_package_states(specs, providers, &provider_snapshot),
    );
    let mut errors = Vec::new();
    let work_items = collect_plan_work_items(execution_order, &spec_map, &mut errors);
    let mut operations = Vec::with_capacity(work_items.len());
    let mut sensitive_values = BTreeSet::new();

    let mut planned_work_items = work_items
        .par_iter()
        .enumerate()
        .map(|(index, work_item)| {
            (
                index,
                plan_resource_operation(
                    work_item.id,
                    &work_item.manifest_path,
                    &work_item.resource,
                    &package_states,
                    &package_diagnostics,
                ),
            )
        })
        .collect::<Vec<_>>();
    planned_work_items.sort_by_key(|(index, _)| *index);

    for (_index, (operation, operation_sensitive_values)) in planned_work_items {
        sensitive_values.extend(operation_sensitive_values);
        operations.push(operation);
    }

    Ok((
        PlanReport {
            discovered_manifests: discovered_manifests.to_vec(),
            execution_order: execution_order.to_vec(),
            operations,
            warnings: Vec::new(),
            errors,
        },
        sensitive_values,
    ))
}

fn collect_plan_work_items(
    execution_order: &[PathBuf],
    spec_map: &HashMap<PathBuf, &ManifestSpec>,
    errors: &mut Vec<String>,
) -> Vec<PlanWorkItem> {
    let mut work_items = Vec::new();
    let mut next_id = 1usize;

    for manifest_path in execution_order {
        let Some(spec) = spec_map.get(manifest_path) else {
            errors.push(format!(
                "graph referenced unknown manifest: {}",
                manifest_path.display()
            ));
            continue;
        };

        for resource in &spec.resources {
            work_items.push(PlanWorkItem {
                id: next_id,
                manifest_path: manifest_path.clone(),
                resource: resource.clone(),
            });
            next_id += 1;
        }
    }

    work_items
}

fn plan_resource_operation(
    id: usize,
    manifest_path: &Path,
    resource: &Resource,
    package_states: &HashMap<PackageQueryKey, PackageStateStatus>,
    package_diagnostics: &PackageDiagnostics,
) -> (PlannedOperation, BTreeSet<String>) {
    match resource {
        Resource::Link(link) => (
            plan_link_operation(id, manifest_path.to_path_buf(), resource.clone(), link),
            BTreeSet::new(),
        ),
        Resource::Template(template) => {
            plan_template_operation(id, manifest_path.to_path_buf(), resource.clone(), template)
        }
        Resource::Package(package) => (
            plan_package_operation(
                id,
                manifest_path.to_path_buf(),
                resource.clone(),
                package,
                package_states,
                package_diagnostics,
            ),
            BTreeSet::new(),
        ),
        Resource::Command(command) => (
            plan_command_operation(id, manifest_path.to_path_buf(), resource.clone(), command),
            BTreeSet::new(),
        ),
    }
}

fn plan_command_operation(
    id: usize,
    manifest_path: PathBuf,
    resource: Resource,
    command: &CommandResource,
) -> PlannedOperation {
    let display = if command.args.is_empty() {
        command.binary.clone()
    } else {
        format!("{} {}", command.binary, command.args.join(" "))
    };

    match which::which(&command.binary) {
        Ok(_) => PlannedOperation {
            id,
            manifest: manifest_path,
            action: PlanAction::CommandRun,
            resource,
            summary: format!("run command: {display}"),
            would_change: true,
            conflict: false,
            hint: None,
            error: None,
            content_hash: None,
            dest_content_hash: None,
        },
        Err(_) => PlannedOperation {
            id,
            manifest: manifest_path,
            action: PlanAction::CommandRun,
            resource,
            summary: format!("run command: {display}"),
            would_change: false,
            conflict: true,
            hint: Some(format!("binary \"{}\" not found on PATH", command.binary)),
            error: Some(format!("binary \"{}\" not found on PATH", command.binary)),
            content_hash: None,
            dest_content_hash: None,
        },
    }
}

fn normalize_provider_hint(hint: Option<&str>) -> Option<String> {
    hint.map(str::to_ascii_lowercase)
}

fn collect_grouped_package_queries(
    specs: &[ManifestSpec],
) -> Vec<(Option<String>, BTreeSet<PackageName>)> {
    let mut grouped: HashMap<Option<String>, BTreeSet<PackageName>> = HashMap::new();

    for manifest in specs {
        for resource in &manifest.resources {
            if let Resource::Package(package) = resource {
                grouped
                    .entry(normalize_provider_hint(package.provider_hint.as_deref()))
                    .or_default()
                    .insert(package.name.clone());
            }
        }
    }

    let mut grouped_items: Vec<_> = grouped.into_iter().collect();
    grouped_items.sort_by(|left, right| left.0.cmp(&right.0));
    grouped_items
}

fn collect_package_state_group(
    providers: &ProviderRegistry,
    snapshot: &ProviderSnapshot,
    hint: Option<&str>,
    normalized_hint: Option<&str>,
    package_names: &BTreeSet<PackageName>,
) -> Vec<PackageStateEntry> {
    match package_states_bulk(providers, snapshot, hint, package_names) {
        Ok((provider_name, installed_states)) => package_names
            .iter()
            .map(|package_name| {
                let lookup_key = PackageQueryKey {
                    name: package_name.clone(),
                    provider_hint: normalized_hint.map(str::to_string),
                };
                let installed = installed_states.get(package_name).copied().unwrap_or(false);
                (lookup_key, Ok((provider_name.clone(), installed)))
            })
            .collect(),
        Err(error) => {
            let state_error = PackageStateError::from(Arc::new(error));
            package_names
                .iter()
                .cloned()
                .map(|package_name| {
                    (
                        PackageQueryKey {
                            name: package_name,
                            provider_hint: normalized_hint.map(str::to_string),
                        },
                        Err(state_error.clone()),
                    )
                })
                .collect()
        }
    }
}

fn collect_package_states(
    specs: &[ManifestSpec],
    providers: &ProviderRegistry,
    snapshot: &ProviderSnapshot,
) -> HashMap<PackageQueryKey, PackageStateStatus> {
    let grouped_items = collect_grouped_package_queries(specs);

    let mut states = HashMap::new();
    let mut grouped_states = grouped_items
        .par_iter()
        .enumerate()
        .map(|(index, (hint, package_names))| {
            (
                index,
                collect_package_state_group(
                    providers,
                    snapshot,
                    hint.as_deref(),
                    hint.as_deref(),
                    package_names,
                ),
            )
        })
        .collect::<Vec<_>>();
    grouped_states.sort_by_key(|(index, _)| *index);

    for (_index, entries) in grouped_states {
        for (lookup_key, state) in entries {
            states.insert(lookup_key, state);
        }
    }

    states
}

fn collect_package_diagnostics(
    specs: &[ManifestSpec],
    snapshot: &ProviderSnapshot,
) -> PackageDiagnostics {
    let (provider_diagnostics, path_diagnostics) = rayon::join(
        || precheck_package_providers(specs, snapshot),
        || precheck_package_path_presence(specs),
    );
    let mut diagnostics = provider_diagnostics;
    merge_package_diagnostics(&mut diagnostics, path_diagnostics);
    diagnostics
}

fn precheck_package_providers(
    specs: &[ManifestSpec],
    snapshot: &ProviderSnapshot,
) -> PackageDiagnostics {
    let mut diagnostics = PackageDiagnostics::new();
    let supported = snapshot
        .supported_names()
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let supported_text = if supported.is_empty() {
        "<none>".to_string()
    } else {
        supported.join(", ")
    };
    let any_provider_available = snapshot.any_available();

    for manifest in specs {
        for resource in &manifest.resources {
            let Resource::Package(package) = resource else {
                continue;
            };

            let lookup_key = PackageQueryKey {
                name: package.name.clone(),
                provider_hint: normalize_provider_hint(package.provider_hint.as_deref()),
            };

            if let Some(hint) = package.provider_hint.as_deref() {
                if !snapshot.is_supported(hint) {
                    push_package_diagnostic(
                        &mut diagnostics,
                        lookup_key,
                        format!(
                            "package provider '{hint}' is not supported by keron (supported: {supported_text})"
                        ),
                    );
                    continue;
                }

                if !snapshot.is_available(hint) {
                    push_package_diagnostic(
                        &mut diagnostics,
                        lookup_key,
                        format!(
                            "package provider '{hint}' is not installed or not available on this host"
                        ),
                    );
                }
            } else if !any_provider_available {
                push_package_diagnostic(
                    &mut diagnostics,
                    lookup_key,
                    format!(
                        "no package manager is available for package resources (checked: {supported_text})"
                    ),
                );
            }
        }
    }

    diagnostics
}

fn precheck_package_path_presence(specs: &[ManifestSpec]) -> PackageDiagnostics {
    let mut diagnostics = PackageDiagnostics::new();
    let mut checked = BTreeSet::new();

    for manifest in specs {
        for resource in &manifest.resources {
            let Resource::Package(package) = resource else {
                continue;
            };

            // This warning is only relevant when package resources are manager-driven installs.
            let Some(provider_hint) = package.provider_hint.as_deref() else {
                continue;
            };
            if !matches!(package.state, PackageState::Present) {
                continue;
            }

            let normalized_provider = provider_hint.to_ascii_lowercase();
            let lookup_key = PackageQueryKey {
                name: package.name.clone(),
                provider_hint: Some(normalized_provider.clone()),
            };
            if !checked.insert(lookup_key.clone()) {
                continue;
            }

            let default_install_folders = provider_default_install_folders(&normalized_provider);
            if default_install_folders.is_empty() {
                continue;
            }

            if let Ok(path) = which::which(package.name.as_str())
                && !is_within_any_default_install_folder(&path, &default_install_folders)
            {
                let folders = default_install_folders
                    .iter()
                    .map(|folder| folder.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                push_package_diagnostic(
                    &mut diagnostics,
                    lookup_key,
                    format!(
                        "package '{}' resolves to {} which is outside default '{}' install folders (default folders: {folders})",
                        package.name,
                        path.display(),
                        provider_hint
                    ),
                );
            }
        }
    }

    diagnostics
}

fn push_package_diagnostic(
    diagnostics: &mut PackageDiagnostics,
    key: PackageQueryKey,
    message: String,
) {
    let entry = diagnostics.entry(key).or_default();
    if !entry.contains(&message) {
        entry.push(message);
    }
}

fn merge_package_diagnostics(into: &mut PackageDiagnostics, from: PackageDiagnostics) {
    for (key, messages) in from {
        let entry = into.entry(key).or_default();
        for message in messages {
            if !entry.contains(&message) {
                entry.push(message);
            }
        }
    }
}

fn is_within_any_default_install_folder(path: &Path, folders: &[PathBuf]) -> bool {
    folders.iter().any(|folder| path.starts_with(folder))
}

fn provider_default_install_folders(provider: &str) -> Vec<PathBuf> {
    match provider {
        "brew" => vec![
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/opt/homebrew/sbin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/usr/local/sbin"),
            PathBuf::from("/home/linuxbrew/.linuxbrew/bin"),
            PathBuf::from("/home/linuxbrew/.linuxbrew/sbin"),
        ],
        "apt" => vec![
            PathBuf::from("/usr/bin"),
            PathBuf::from("/usr/sbin"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
        ],
        "winget" => {
            let mut folders = Vec::new();
            if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
                folders.push(
                    PathBuf::from(local_app_data)
                        .join("Microsoft")
                        .join("WinGet")
                        .join("Links"),
                );
            }
            if let Some(program_files) = std::env::var_os("ProgramFiles") {
                folders.push(PathBuf::from(program_files).join("WindowsApps"));
            }
            if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
                folders.push(PathBuf::from(program_files_x86).join("WindowsApps"));
            }
            folders
        }
        "cargo" => {
            let mut folders = Vec::new();
            if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
                folders.push(PathBuf::from(cargo_home).join("bin"));
            } else if let Some(home) = dirs::home_dir() {
                folders.push(home.join(".cargo").join("bin"));
            }
            folders
        }
        _ => Vec::new(),
    }
}

fn sha256_file(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| sha256_bytes(&bytes))
}

fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn link_summary(link: &LinkResource) -> String {
    format!("link {} -> {}", link.src.display(), link.dest.display())
}

fn link_inspect_path(path: &Path, label: &'static str) -> Result<bool, LinkInspectError> {
    path_exists_including_dangling_symlink(path).map_err(|source| LinkInspectError::Path {
        label,
        path: path.to_path_buf(),
        source,
    })
}

fn link_inspect_target(dest: &Path, src: &Path) -> Result<bool, LinkInspectError> {
    symlink_points_to(dest, src).map_err(|source| LinkInspectError::Target {
        dest: dest.to_path_buf(),
        source,
    })
}

fn link_conflict_operation(
    ctx: &OperationContext,
    summary: String,
    hint: Option<String>,
    error: String,
    content_hash: Option<String>,
    dest_content_hash: Option<String>,
) -> PlannedOperation {
    ctx.planned_operation(OperationDecision {
        action: PlanAction::LinkConflict,
        summary,
        would_change: false,
        conflict: true,
        hint,
        error: Some(error),
        content_hash,
        dest_content_hash,
    })
}

fn link_create_operation(ctx: &OperationContext, link: &LinkResource) -> PlannedOperation {
    ctx.planned_operation(OperationDecision {
        action: PlanAction::LinkCreate,
        summary: format!(
            "create symlink {} -> {}",
            link.dest.display(),
            link.src.display()
        ),
        would_change: true,
        conflict: false,
        hint: None,
        error: None,
        content_hash: sha256_file(&link.src),
        dest_content_hash: None,
    })
}

fn link_noop_operation(ctx: &OperationContext, link: &LinkResource) -> PlannedOperation {
    ctx.planned_operation(OperationDecision {
        action: PlanAction::LinkNoop,
        summary: format!("link already up to date: {}", link.dest.display()),
        would_change: false,
        conflict: false,
        hint: None,
        error: None,
        content_hash: sha256_file(&link.src),
        dest_content_hash: None,
    })
}

fn link_replace_or_conflict_operation(
    ctx: &OperationContext,
    link: &LinkResource,
) -> PlannedOperation {
    let src_hash = sha256_file(&link.src);
    let dest_hash = sha256_file(&link.dest);
    let content_matches = src_hash.is_some() && dest_hash.is_some() && src_hash == dest_hash;

    let hint = if link.force {
        "will replace existing path due to force=true".to_string()
    } else if content_matches {
        "destination content matches source but is not a symlink; set force=true to create proper symlink".to_string()
    } else {
        "set force=true or remove destination manually".to_string()
    };

    ctx.planned_operation(OperationDecision {
        action: if link.force {
            PlanAction::LinkReplace
        } else {
            PlanAction::LinkConflict
        },
        summary: format!(
            "destination already exists and differs: {}",
            link.dest.display()
        ),
        would_change: link.force,
        conflict: true,
        hint: Some(hint),
        error: if link.force {
            None
        } else {
            Some("destination conflicts with requested symlink".to_string())
        },
        content_hash: src_hash,
        dest_content_hash: dest_hash,
    })
}

fn plan_link_operation(
    id: usize,
    manifest_path: PathBuf,
    resource: Resource,
    link: &LinkResource,
) -> PlannedOperation {
    let ctx = OperationContext::new(id, manifest_path, resource);
    let summary = link_summary(link);

    let source_exists = match link_inspect_path(&link.src, "source") {
        Ok(exists) => exists,
        Err(error) => {
            return link_conflict_operation(&ctx, summary, None, error.to_string(), None, None);
        }
    };
    if !source_exists {
        return link_conflict_operation(
            &ctx,
            summary,
            Some("source file does not exist".to_string()),
            format!("link source missing: {}", link.src.display()),
            None,
            None,
        );
    }

    let destination_exists = match link_inspect_path(&link.dest, "destination") {
        Ok(exists) => exists,
        Err(error) => {
            return link_conflict_operation(
                &ctx,
                link_summary(link),
                None,
                error.to_string(),
                None,
                None,
            );
        }
    };
    if !destination_exists {
        return link_create_operation(&ctx, link);
    }

    let same_target = match link_inspect_target(&link.dest, &link.src) {
        Ok(same_target) => same_target,
        Err(error) => {
            return link_conflict_operation(
                &ctx,
                link_summary(link),
                None,
                error.to_string(),
                None,
                None,
            );
        }
    };
    if same_target {
        return link_noop_operation(&ctx, link);
    }

    link_replace_or_conflict_operation(&ctx, link)
}

fn plan_package_operation(
    id: usize,
    manifest_path: PathBuf,
    resource: Resource,
    package: &PackageResource,
    package_states: &HashMap<PackageQueryKey, PackageStateStatus>,
    package_diagnostics: &PackageDiagnostics,
) -> PlannedOperation {
    let lookup_key = PackageQueryKey {
        name: package.name.clone(),
        provider_hint: normalize_provider_hint(package.provider_hint.as_deref()),
    };
    let diagnostics_for_operation = package_diagnostics
        .get(&lookup_key)
        .cloned()
        .unwrap_or_default();

    match package_states.get(&lookup_key) {
        Some(Ok((provider_name, installed))) => {
            let installed = *installed;
            let (action, would_change, summary) =
                package_action_and_summary(package, provider_name, installed);

            PlannedOperation {
                id,
                manifest: manifest_path,
                action,
                resource,
                summary,
                would_change,
                conflict: false,
                hint: compose_hint(None, &diagnostics_for_operation),
                error: None,
                content_hash: None,
                dest_content_hash: None,
            }
        }
        Some(Err(error)) => {
            let mut diagnostics = diagnostics_for_operation;
            diagnostics.push(format!(
                "package status unknown for {}: {error}",
                package.name
            ));
            package_unknown_operation(id, manifest_path, resource, package, &diagnostics)
        }
        None => {
            let mut diagnostics = diagnostics_for_operation;
            diagnostics.push(format!(
                "package status unknown for {}: package state cache missing",
                package.name
            ));
            package_unknown_operation(id, manifest_path, resource, package, &diagnostics)
        }
    }
}

fn compose_hint(existing: Option<String>, diagnostics: &[String]) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(existing) = existing {
        parts.push(existing);
    }
    for diagnostic in diagnostics {
        if !parts.contains(diagnostic) {
            parts.push(diagnostic.clone());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn package_action_and_summary(
    package: &PackageResource,
    provider_name: &str,
    installed: bool,
) -> (PlanAction, bool, String) {
    match (package.state, installed) {
        (PackageState::Present, false) => (
            PlanAction::PackageInstall,
            true,
            format!("install package {} via {provider_name}", package.name),
        ),
        (PackageState::Absent, true) => (
            PlanAction::PackageRemove,
            true,
            format!("remove package {} via {provider_name}", package.name),
        ),
        (_, _) => (
            PlanAction::PackageNoop,
            false,
            format!(
                "package already in desired state: {} via {provider_name}",
                package.name
            ),
        ),
    }
}

const fn desired_package_action(state: PackageState) -> PlanAction {
    match state {
        PackageState::Present => PlanAction::PackageInstall,
        PackageState::Absent => PlanAction::PackageRemove,
    }
}

fn package_unknown_operation(
    id: usize,
    manifest_path: PathBuf,
    resource: Resource,
    package: &PackageResource,
    diagnostics: &[String],
) -> PlannedOperation {
    PlannedOperation {
        id,
        manifest: manifest_path,
        action: desired_package_action(package.state),
        resource,
        summary: format!(
            "package state unknown, needs reconciliation: {}",
            package.name
        ),
        would_change: true,
        conflict: false,
        hint: compose_hint(
            Some("no available provider detected for this host".to_string()),
            diagnostics,
        ),
        error: None,
        content_hash: None,
        dest_content_hash: None,
    }
}

fn template_summary(template: &TemplateResource) -> String {
    format!(
        "template {} -> {}",
        template.src.display(),
        template.dest.display()
    )
}

type TemplatePlanResult = (PlannedOperation, BTreeSet<String>);

#[derive(Debug)]
struct RenderedTemplate {
    content: String,
    content_hash: String,
    sensitive: BTreeSet<String>,
}

fn template_conflict_operation(
    ctx: &OperationContext,
    summary: String,
    hint: Option<String>,
    error: String,
    content_hash: Option<String>,
    dest_content_hash: Option<String>,
    sensitive: BTreeSet<String>,
) -> TemplatePlanResult {
    (
        ctx.planned_operation(OperationDecision {
            action: PlanAction::TemplateConflict,
            summary,
            would_change: false,
            conflict: true,
            hint,
            error: Some(error),
            content_hash,
            dest_content_hash,
        }),
        sensitive,
    )
}

fn render_template_for_planning(
    ctx: &OperationContext,
    template: &TemplateResource,
    summary: &str,
) -> Result<RenderedTemplate, Box<TemplatePlanResult>> {
    let source_exists = match path_exists_including_dangling_symlink(&template.src) {
        Ok(exists) => exists,
        Err(error) => {
            return Err(Box::new(template_conflict_operation(
                ctx,
                summary.to_string(),
                None,
                format!(
                    "failed to inspect template source {}: {error}",
                    template.src.display()
                ),
                None,
                None,
                BTreeSet::new(),
            )));
        }
    };
    if !source_exists {
        return Err(Box::new(template_conflict_operation(
            ctx,
            summary.to_string(),
            Some("template source file does not exist".to_string()),
            format!("template source missing: {}", template.src.display()),
            None,
            None,
            BTreeSet::new(),
        )));
    }

    let source = match fs::read_to_string(&template.src) {
        Ok(content) => content,
        Err(error) => {
            return Err(Box::new(template_conflict_operation(
                ctx,
                summary.to_string(),
                None,
                format!(
                    "failed to read template source {}: {error}",
                    template.src.display()
                ),
                None,
                None,
                BTreeSet::new(),
            )));
        }
    };

    let (rendered, template_sensitive) = match render_template_string(&source, &template.vars) {
        Ok(result) => result,
        Err(error) => {
            return Err(Box::new(template_conflict_operation(
                ctx,
                summary.to_string(),
                None,
                format!(
                    "failed to render template {}: {error}",
                    template.src.display()
                ),
                None,
                None,
                BTreeSet::new(),
            )));
        }
    };

    Ok(RenderedTemplate {
        content_hash: sha256_bytes(rendered.as_bytes()),
        content: rendered,
        sensitive: template_sensitive,
    })
}

fn template_create_operation(
    ctx: &OperationContext,
    template: &TemplateResource,
    rendered_hash: String,
) -> PlannedOperation {
    ctx.planned_operation(OperationDecision {
        action: PlanAction::TemplateCreate,
        summary: format!(
            "create rendered template {} from {}",
            template.dest.display(),
            template.src.display()
        ),
        would_change: true,
        conflict: false,
        hint: None,
        error: None,
        content_hash: Some(rendered_hash),
        dest_content_hash: None,
    })
}

fn template_unreadable_destination_operation(
    ctx: &OperationContext,
    template: &TemplateResource,
    rendered_hash: String,
) -> PlannedOperation {
    ctx.planned_operation(OperationDecision {
        action: PlanAction::TemplateUpdate,
        summary: format!(
            "destination exists and is unreadable: {}",
            template.dest.display()
        ),
        would_change: true,
        conflict: false,
        hint: Some("will overwrite destination due to force=true".to_string()),
        error: None,
        content_hash: Some(rendered_hash),
        dest_content_hash: None,
    })
}

fn template_noop_operation(
    ctx: &OperationContext,
    template: &TemplateResource,
    rendered_hash: String,
    current_hash: String,
) -> PlannedOperation {
    ctx.planned_operation(OperationDecision {
        action: PlanAction::TemplateNoop,
        summary: format!("template already up to date: {}", template.dest.display()),
        would_change: false,
        conflict: false,
        hint: None,
        error: None,
        content_hash: Some(rendered_hash),
        dest_content_hash: Some(current_hash),
    })
}

fn template_update_or_conflict_operation(
    ctx: &OperationContext,
    template: &TemplateResource,
    rendered_hash: String,
    current_hash: String,
) -> PlannedOperation {
    ctx.planned_operation(OperationDecision {
        action: if template.force {
            PlanAction::TemplateUpdate
        } else {
            PlanAction::TemplateConflict
        },
        summary: format!(
            "destination differs from rendered template: {}",
            template.dest.display()
        ),
        would_change: template.force,
        conflict: !template.force,
        hint: Some(if template.force {
            "will overwrite destination due to force=true".to_string()
        } else {
            "set force=true or update destination manually".to_string()
        }),
        error: if template.force {
            None
        } else {
            Some("destination content differs from rendered template".to_string())
        },
        content_hash: Some(rendered_hash),
        dest_content_hash: Some(current_hash),
    })
}

fn plan_template_destination(
    ctx: &OperationContext,
    template: &TemplateResource,
    rendered: RenderedTemplate,
) -> TemplatePlanResult {
    let summary = template_summary(template);

    let destination_exists = match path_exists_including_dangling_symlink(&template.dest) {
        Ok(exists) => exists,
        Err(error) => {
            return template_conflict_operation(
                ctx,
                summary,
                None,
                format!(
                    "failed to inspect template destination {}: {error}",
                    template.dest.display()
                ),
                Some(rendered.content_hash),
                None,
                rendered.sensitive,
            );
        }
    };
    if !destination_exists {
        return (
            template_create_operation(ctx, template, rendered.content_hash),
            rendered.sensitive,
        );
    }

    let current = match fs::read_to_string(&template.dest) {
        Ok(content) => content,
        Err(error) => {
            if template.force {
                return (
                    template_unreadable_destination_operation(ctx, template, rendered.content_hash),
                    rendered.sensitive,
                );
            }

            return template_conflict_operation(
                ctx,
                format!(
                    "destination exists and is unreadable: {}",
                    template.dest.display()
                ),
                Some("set force=true to replace unreadable destination".to_string()),
                format!(
                    "failed to read destination {}: {error}",
                    template.dest.display()
                ),
                Some(rendered.content_hash),
                None,
                rendered.sensitive,
            );
        }
    };

    let current_hash = sha256_bytes(current.as_bytes());
    if current == rendered.content {
        return (
            template_noop_operation(ctx, template, rendered.content_hash, current_hash),
            rendered.sensitive,
        );
    }

    (
        template_update_or_conflict_operation(ctx, template, rendered.content_hash, current_hash),
        rendered.sensitive,
    )
}

fn plan_template_operation(
    id: usize,
    manifest_path: PathBuf,
    resource: Resource,
    template: &TemplateResource,
) -> TemplatePlanResult {
    let ctx = OperationContext::new(id, manifest_path, resource);
    let summary = template_summary(template);

    let rendered = match render_template_for_planning(&ctx, template, &summary) {
        Ok(rendered) => rendered,
        Err(result) => return *result,
    };

    plan_template_destination(&ctx, template, rendered)
}

#[cfg(test)]
mod tests;
