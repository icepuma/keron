use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use keron_domain::{
    LinkResource, ManifestSpec, PackageResource, PackageState, PlanAction, PlanReport,
    PlannedOperation, Resource, TemplateResource,
};

use crate::fs_util::{path_exists_including_dangling_symlink, symlink_points_to};
use crate::providers::{ProviderRegistry, package_states_bulk};
use crate::template::render_template_string;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PackageQueryKey {
    name: String,
    provider_hint: Option<String>,
}

/// Build a plan report from discovered manifests and resolved execution order.
///
/// # Errors
///
/// Returns an error if package/provider state collection requires fallible operations
/// that cannot be completed.
pub fn build_plan(
    discovered_manifests: &[PathBuf],
    specs: &[ManifestSpec],
    execution_order: &[PathBuf],
    providers: &ProviderRegistry,
) -> Result<(PlanReport, BTreeSet<String>)> {
    let spec_map: HashMap<_, _> = specs
        .iter()
        .map(|manifest| (manifest.id.path.clone(), manifest))
        .collect();

    let mut warnings = Vec::new();
    warnings.extend(precheck_package_providers(specs, providers));
    let package_states = collect_package_states(specs, providers);
    let mut errors = Vec::new();
    let mut operations = Vec::new();
    let mut sensitive_values = BTreeSet::new();
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
            let operation = match resource {
                Resource::Link(link) => {
                    plan_link_operation(next_id, manifest_path.clone(), resource.clone(), link)
                }
                Resource::Template(template) => {
                    let (op, template_sensitive) = plan_template_operation(
                        next_id,
                        manifest_path.clone(),
                        resource.clone(),
                        template,
                    );
                    sensitive_values.extend(template_sensitive);
                    op
                }
                Resource::Package(package) => plan_package_operation(
                    next_id,
                    manifest_path.clone(),
                    resource.clone(),
                    package,
                    &package_states,
                    &mut warnings,
                ),
                Resource::Command(command) => {
                    let display = if command.args.is_empty() {
                        command.binary.clone()
                    } else {
                        format!("{} {}", command.binary, command.args.join(" "))
                    };

                    match which::which(&command.binary) {
                        Ok(_) => PlannedOperation {
                            id: next_id,
                            manifest: manifest_path.clone(),
                            action: PlanAction::CommandRun,
                            resource: resource.clone(),
                            summary: format!("run command: {display}"),
                            would_change: true,
                            conflict: false,
                            hint: None,
                            error: None,
                            content_hash: None,
                            dest_content_hash: None,
                        },
                        Err(_) => PlannedOperation {
                            id: next_id,
                            manifest: manifest_path.clone(),
                            action: PlanAction::CommandRun,
                            resource: resource.clone(),
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
            };

            if let Some(error) = &operation.error {
                errors.push(format!(
                    "manifest {} operation {}: {}",
                    manifest_path.display(),
                    operation.id,
                    error
                ));
            }

            operations.push(operation);
            next_id += 1;
        }
    }

    Ok((
        PlanReport {
            discovered_manifests: discovered_manifests.to_vec(),
            execution_order: execution_order.to_vec(),
            operations,
            warnings,
            errors,
        },
        sensitive_values,
    ))
}

fn normalize_provider_hint(hint: Option<&str>) -> Option<String> {
    hint.map(str::to_ascii_lowercase)
}

fn collect_package_states(
    specs: &[ManifestSpec],
    providers: &ProviderRegistry,
) -> HashMap<PackageQueryKey, Result<(String, bool), String>> {
    let mut grouped: HashMap<Option<String>, BTreeSet<String>> = HashMap::new();

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

    let mut states = HashMap::new();
    for (hint, package_names) in grouped {
        match package_states_bulk(providers, hint.as_deref(), &package_names) {
            Ok((provider_name, installed_states)) => {
                for package_name in package_names {
                    states.insert(
                        PackageQueryKey {
                            name: package_name.clone(),
                            provider_hint: hint.clone(),
                        },
                        Ok((
                            provider_name.clone(),
                            installed_states
                                .get(&package_name)
                                .copied()
                                .unwrap_or(false),
                        )),
                    );
                }
            }
            Err(error) => {
                let message = error.to_string();
                for package_name in package_names {
                    states.insert(
                        PackageQueryKey {
                            name: package_name,
                            provider_hint: hint.clone(),
                        },
                        Err(message.clone()),
                    );
                }
            }
        }
    }

    states
}

fn precheck_package_providers(specs: &[ManifestSpec], providers: &ProviderRegistry) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut hinted = BTreeSet::new();
    let mut has_unhinted_packages = false;

    for manifest in specs {
        for resource in &manifest.resources {
            if let Resource::Package(package) = resource {
                if let Some(hint) = package.provider_hint.as_deref() {
                    hinted.insert(hint.to_string());
                } else {
                    has_unhinted_packages = true;
                }
            }
        }
    }

    let supported = providers.supported_names();
    let supported_text = if supported.is_empty() {
        "<none>".to_string()
    } else {
        supported.join(", ")
    };

    for hint in hinted {
        if !providers.is_supported(&hint) {
            warnings.push(format!(
                "package provider '{hint}' is not supported by keron (supported: {supported_text})"
            ));
            continue;
        }

        if !providers.is_available(&hint) {
            warnings.push(format!(
                "package provider '{hint}' is not installed or not available on this host"
            ));
        }
    }

    if has_unhinted_packages && !providers.any_available() {
        warnings.push(format!(
            "no package manager is available for package resources (checked: {supported_text})"
        ));
    }

    warnings
}

fn sha256_file(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| sha256_bytes(&bytes))
}

fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

// Link planning carries many explicit conflict/error branches for user-facing diagnostics.
#[allow(clippy::too_many_lines)]
fn plan_link_operation(
    id: usize,
    manifest_path: PathBuf,
    resource: Resource,
    link: &LinkResource,
) -> PlannedOperation {
    let source_exists = match path_exists_including_dangling_symlink(&link.src) {
        Ok(exists) => exists,
        Err(error) => {
            return PlannedOperation {
                id,
                manifest: manifest_path,
                action: PlanAction::LinkConflict,
                resource,
                summary: format!("link {} -> {}", link.src.display(), link.dest.display()),
                would_change: false,
                conflict: true,
                hint: None,
                error: Some(format!(
                    "failed to inspect link source {}: {error}",
                    link.src.display()
                )),
                content_hash: None,
                dest_content_hash: None,
            };
        }
    };

    if !source_exists {
        return PlannedOperation {
            id,
            manifest: manifest_path,
            action: PlanAction::LinkConflict,
            resource,
            summary: format!("link {} -> {}", link.src.display(), link.dest.display()),
            would_change: false,
            conflict: true,
            hint: Some("source file does not exist".to_string()),
            error: Some(format!("link source missing: {}", link.src.display())),
            content_hash: None,
            dest_content_hash: None,
        };
    }

    let destination_exists = match path_exists_including_dangling_symlink(&link.dest) {
        Ok(exists) => exists,
        Err(error) => {
            return PlannedOperation {
                id,
                manifest: manifest_path,
                action: PlanAction::LinkConflict,
                resource,
                summary: format!("link {} -> {}", link.src.display(), link.dest.display()),
                would_change: false,
                conflict: true,
                hint: None,
                error: Some(format!(
                    "failed to inspect link destination {}: {error}",
                    link.dest.display()
                )),
                content_hash: None,
                dest_content_hash: None,
            };
        }
    };

    if !destination_exists {
        return PlannedOperation {
            id,
            manifest: manifest_path,
            action: PlanAction::LinkCreate,
            resource,
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
        };
    }

    let same_target = match symlink_points_to(&link.dest, &link.src) {
        Ok(value) => value,
        Err(error) => {
            return PlannedOperation {
                id,
                manifest: manifest_path,
                action: PlanAction::LinkConflict,
                resource,
                summary: format!("link {} -> {}", link.src.display(), link.dest.display()),
                would_change: false,
                conflict: true,
                hint: None,
                error: Some(format!(
                    "failed to inspect link destination {}: {error}",
                    link.dest.display()
                )),
                content_hash: None,
                dest_content_hash: None,
            };
        }
    };

    if same_target {
        return PlannedOperation {
            id,
            manifest: manifest_path,
            action: PlanAction::LinkNoop,
            resource,
            summary: format!("link already up to date: {}", link.dest.display()),
            would_change: false,
            conflict: false,
            hint: None,
            error: None,
            content_hash: sha256_file(&link.src),
            dest_content_hash: None,
        };
    }

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

    PlannedOperation {
        id,
        manifest: manifest_path,
        action: if link.force {
            PlanAction::LinkReplace
        } else {
            PlanAction::LinkConflict
        },
        resource,
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
    }
}

fn plan_package_operation(
    id: usize,
    manifest_path: PathBuf,
    resource: Resource,
    package: &PackageResource,
    package_states: &HashMap<PackageQueryKey, Result<(String, bool), String>>,
    warnings: &mut Vec<String>,
) -> PlannedOperation {
    let lookup_key = PackageQueryKey {
        name: package.name.clone(),
        provider_hint: normalize_provider_hint(package.provider_hint.as_deref()),
    };

    match package_states.get(&lookup_key) {
        Some(Ok((provider_name, installed))) => {
            let installed = *installed;
            let (action, would_change, summary) = match (package.state, installed) {
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
            };

            PlannedOperation {
                id,
                manifest: manifest_path,
                action,
                resource,
                summary,
                would_change,
                conflict: false,
                hint: None,
                error: None,
                content_hash: None,
                dest_content_hash: None,
            }
        }
        Some(Err(error)) => {
            warnings.push(format!(
                "package status unknown for {}: {error}",
                package.name
            ));

            PlannedOperation {
                id,
                manifest: manifest_path,
                action: match package.state {
                    PackageState::Present => PlanAction::PackageInstall,
                    PackageState::Absent => PlanAction::PackageRemove,
                },
                resource,
                summary: format!(
                    "package state unknown, needs reconciliation: {}",
                    package.name
                ),
                would_change: true,
                conflict: false,
                hint: Some("no available provider detected for this host".to_string()),
                error: None,
                content_hash: None,
                dest_content_hash: None,
            }
        }
        None => {
            warnings.push(format!(
                "package status unknown for {}: package state cache missing",
                package.name
            ));

            PlannedOperation {
                id,
                manifest: manifest_path,
                action: match package.state {
                    PackageState::Present => PlanAction::PackageInstall,
                    PackageState::Absent => PlanAction::PackageRemove,
                },
                resource,
                summary: format!(
                    "package state unknown, needs reconciliation: {}",
                    package.name
                ),
                would_change: true,
                conflict: false,
                hint: Some("no available provider detected for this host".to_string()),
                error: None,
                content_hash: None,
                dest_content_hash: None,
            }
        }
    }
}

// Template planning mirrors link planning and keeps all conflict branches adjacent for clarity.
#[allow(clippy::too_many_lines)]
fn plan_template_operation(
    id: usize,
    manifest_path: PathBuf,
    resource: Resource,
    template: &TemplateResource,
) -> (PlannedOperation, BTreeSet<String>) {
    let source_exists = match path_exists_including_dangling_symlink(&template.src) {
        Ok(exists) => exists,
        Err(error) => {
            return (
                PlannedOperation {
                    id,
                    manifest: manifest_path,
                    action: PlanAction::TemplateConflict,
                    resource,
                    summary: format!(
                        "template {} -> {}",
                        template.src.display(),
                        template.dest.display()
                    ),
                    would_change: false,
                    conflict: true,
                    hint: None,
                    error: Some(format!(
                        "failed to inspect template source {}: {error}",
                        template.src.display()
                    )),
                    content_hash: None,
                    dest_content_hash: None,
                },
                BTreeSet::new(),
            );
        }
    };

    if !source_exists {
        return (
            PlannedOperation {
                id,
                manifest: manifest_path,
                action: PlanAction::TemplateConflict,
                resource,
                summary: format!(
                    "template {} -> {}",
                    template.src.display(),
                    template.dest.display()
                ),
                would_change: false,
                conflict: true,
                hint: Some("template source file does not exist".to_string()),
                error: Some(format!(
                    "template source missing: {}",
                    template.src.display()
                )),
                content_hash: None,
                dest_content_hash: None,
            },
            BTreeSet::new(),
        );
    }

    let source = match fs::read_to_string(&template.src) {
        Ok(content) => content,
        Err(error) => {
            return (
                PlannedOperation {
                    id,
                    manifest: manifest_path,
                    action: PlanAction::TemplateConflict,
                    resource,
                    summary: format!(
                        "template {} -> {}",
                        template.src.display(),
                        template.dest.display()
                    ),
                    would_change: false,
                    conflict: true,
                    hint: None,
                    error: Some(format!(
                        "failed to read template source {}: {error}",
                        template.src.display()
                    )),
                    content_hash: None,
                    dest_content_hash: None,
                },
                BTreeSet::new(),
            );
        }
    };

    let (rendered, template_sensitive) = match render_template_string(&source, &template.vars) {
        Ok(result) => result,
        Err(error) => {
            return (
                PlannedOperation {
                    id,
                    manifest: manifest_path,
                    action: PlanAction::TemplateConflict,
                    resource,
                    summary: format!(
                        "template {} -> {}",
                        template.src.display(),
                        template.dest.display()
                    ),
                    would_change: false,
                    conflict: true,
                    hint: None,
                    error: Some(format!(
                        "failed to render template {}: {error}",
                        template.src.display()
                    )),
                    content_hash: None,
                    dest_content_hash: None,
                },
                BTreeSet::new(),
            );
        }
    };

    let rendered_hash = sha256_bytes(rendered.as_bytes());

    let destination_exists = match path_exists_including_dangling_symlink(&template.dest) {
        Ok(exists) => exists,
        Err(error) => {
            return (
                PlannedOperation {
                    id,
                    manifest: manifest_path,
                    action: PlanAction::TemplateConflict,
                    resource,
                    summary: format!(
                        "template {} -> {}",
                        template.src.display(),
                        template.dest.display()
                    ),
                    would_change: false,
                    conflict: true,
                    hint: None,
                    error: Some(format!(
                        "failed to inspect template destination {}: {error}",
                        template.dest.display()
                    )),
                    content_hash: Some(rendered_hash),
                    dest_content_hash: None,
                },
                template_sensitive,
            );
        }
    };

    if !destination_exists {
        return (
            PlannedOperation {
                id,
                manifest: manifest_path,
                action: PlanAction::TemplateCreate,
                resource,
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
            },
            template_sensitive,
        );
    }

    let current = match fs::read_to_string(&template.dest) {
        Ok(content) => content,
        Err(error) => {
            if template.force {
                return (
                    PlannedOperation {
                        id,
                        manifest: manifest_path,
                        action: PlanAction::TemplateUpdate,
                        resource,
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
                    },
                    template_sensitive,
                );
            }

            return (
                PlannedOperation {
                    id,
                    manifest: manifest_path,
                    action: PlanAction::TemplateConflict,
                    resource,
                    summary: format!(
                        "destination exists and is unreadable: {}",
                        template.dest.display()
                    ),
                    would_change: false,
                    conflict: true,
                    hint: Some("set force=true to replace unreadable destination".to_string()),
                    error: Some(format!(
                        "failed to read destination {}: {error}",
                        template.dest.display()
                    )),
                    content_hash: Some(rendered_hash),
                    dest_content_hash: None,
                },
                template_sensitive,
            );
        }
    };

    let current_hash = sha256_bytes(current.as_bytes());

    if current == rendered {
        return (
            PlannedOperation {
                id,
                manifest: manifest_path,
                action: PlanAction::TemplateNoop,
                resource,
                summary: format!("template already up to date: {}", template.dest.display()),
                would_change: false,
                conflict: false,
                hint: None,
                error: None,
                content_hash: Some(rendered_hash),
                dest_content_hash: Some(current_hash),
            },
            template_sensitive,
        );
    }

    (
        PlannedOperation {
            id,
            manifest: manifest_path,
            action: if template.force {
                PlanAction::TemplateUpdate
            } else {
                PlanAction::TemplateConflict
            },
            resource,
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
        },
        template_sensitive,
    )
}

#[cfg(test)]
mod tests;
