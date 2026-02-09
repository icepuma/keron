use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use keron_domain::{
    ApplyOperationResult, ApplyReport, CommandResource, LinkResource, PackageResource,
    PackageState, PlanReport, Resource, TemplateResource,
};

use crate::fs_util::{path_exists_including_dangling_symlink, symlink_points_to};
use crate::providers::{ProviderRegistry, apply_package, package_state};
use crate::template::render_template_string;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOptions {
    pub fail_fast: bool,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self { fail_fast: true }
    }
}

#[must_use]
pub fn apply_plan(
    plan: &PlanReport,
    providers: &ProviderRegistry,
    options: ApplyOptions,
) -> (ApplyReport, BTreeSet<String>) {
    let mut results = Vec::with_capacity(plan.operations.len());
    let mut errors = Vec::new();
    let mut sensitive_values = BTreeSet::new();

    for (index, operation) in plan.operations.iter().enumerate() {
        if let Some(error) = &operation.error {
            errors.push(format!("operation {} blocked: {error}", operation.id));
            results.push(ApplyOperationResult {
                operation_id: operation.id,
                summary: operation.summary.clone(),
                success: false,
                changed: false,
                error: Some(error.clone()),
            });
            if options.fail_fast {
                push_fail_fast_abort_message(&mut errors, index, plan.operations.len());
                break;
            }
            continue;
        }

        let applied = match &operation.resource {
            Resource::Link(link) => apply_link(link),
            Resource::Template(template) => {
                let result = apply_template(template);
                match result {
                    Ok((changed, template_sensitive)) => {
                        sensitive_values.extend(template_sensitive);
                        Ok(changed)
                    }
                    Err(error) => Err(error),
                }
            }
            Resource::Package(package) => apply_package_resource(package, providers),
            Resource::Command(command) => apply_command_resource(command),
        };

        match applied {
            Ok(changed) => results.push(ApplyOperationResult {
                operation_id: operation.id,
                summary: operation.summary.clone(),
                success: true,
                changed,
                error: None,
            }),
            Err(error) => {
                let message = error.to_string();
                errors.push(format!("operation {} failed: {message}", operation.id));
                results.push(ApplyOperationResult {
                    operation_id: operation.id,
                    summary: operation.summary.clone(),
                    success: false,
                    changed: false,
                    error: Some(message),
                });
                if options.fail_fast {
                    push_fail_fast_abort_message(&mut errors, index, plan.operations.len());
                    break;
                }
            }
        }
    }

    (
        ApplyReport {
            plan: plan.clone(),
            results,
            errors,
        },
        sensitive_values,
    )
}

fn push_fail_fast_abort_message(errors: &mut Vec<String>, failed_index: usize, total: usize) {
    let remaining = total.saturating_sub(failed_index + 1);
    errors.push(format!(
        "apply aborted after first failure due to fail-fast ({remaining} operation(s) not attempted)"
    ));
}

fn apply_package_resource(package: &PackageResource, providers: &ProviderRegistry) -> Result<bool> {
    if let Ok((_provider, installed)) =
        package_state(providers, &package.name, package.provider_hint.as_deref())
    {
        match (package.state, installed) {
            (PackageState::Present, false) => {
                let _ = apply_package(
                    providers,
                    &package.name,
                    package.provider_hint.as_deref(),
                    true,
                )?;
                Ok(true)
            }
            (PackageState::Absent, true) => {
                let _ = apply_package(
                    providers,
                    &package.name,
                    package.provider_hint.as_deref(),
                    false,
                )?;
                Ok(true)
            }
            (_, _) => Ok(false),
        }
    } else {
        let install = matches!(package.state, PackageState::Present);
        let _ = apply_package(
            providers,
            &package.name,
            package.provider_hint.as_deref(),
            install,
        )?;
        Ok(true)
    }
}

fn apply_command_resource(command: &CommandResource) -> Result<bool> {
    let binary_path = which::which(&command.binary)
        .with_context(|| format!("binary \"{}\" not found on PATH", command.binary))?;

    let status = Command::new(binary_path)
        .args(&command.args)
        .status()
        .with_context(|| format!("failed to execute command: {}", command.binary))?;

    if status.success() {
        Ok(true)
    } else {
        bail!("command exited with non-zero status: {status}");
    }
}

fn apply_link(link: &LinkResource) -> Result<bool> {
    let source_exists = path_exists_including_dangling_symlink(&link.src)
        .with_context(|| format!("failed to inspect source {}", link.src.display()))?;
    if !source_exists {
        bail!("source does not exist: {}", link.src.display());
    }

    let destination_exists = path_exists_including_dangling_symlink(&link.dest)
        .with_context(|| format!("failed to inspect destination {}", link.dest.display()))?;
    if destination_exists {
        if symlink_points_to(&link.dest, &link.src)
            .with_context(|| format!("failed to inspect symlink {}", link.dest.display()))?
        {
            return Ok(false);
        }

        if !link.force {
            bail!(
                "destination exists and differs (set force=true to replace): {}",
                link.dest.display()
            );
        }

        remove_existing_path(&link.dest)?;
    }

    if link.mkdirs
        && let Some(parent) = link.dest.parent()
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create destination directory: {}",
                parent.display()
            )
        })?;
    }

    create_symlink(&link.src, &link.dest)?;
    Ok(true)
}

fn apply_template(template: &TemplateResource) -> Result<(bool, BTreeSet<String>)> {
    let source_exists =
        path_exists_including_dangling_symlink(&template.src).with_context(|| {
            format!(
                "failed to inspect template source {}",
                template.src.display()
            )
        })?;
    if !source_exists {
        bail!("template source does not exist: {}", template.src.display());
    }

    let source = fs::read_to_string(&template.src)
        .with_context(|| format!("failed to read template source {}", template.src.display()))?;
    let (rendered, template_sensitive) = render_template_string(&source, &template.vars)
        .with_context(|| format!("failed to render template {}", template.src.display()))?;

    let destination_exists =
        path_exists_including_dangling_symlink(&template.dest).with_context(|| {
            format!(
                "failed to inspect template destination {}",
                template.dest.display()
            )
        })?;
    if destination_exists {
        match fs::read_to_string(&template.dest) {
            Ok(current) if current == rendered => {
                return Ok((false, template_sensitive));
            }
            Ok(_) => {}
            Err(error) => {
                if !template.force {
                    bail!(
                        "destination exists and cannot be read (set force=true to replace): {} ({error})",
                        template.dest.display()
                    );
                }
            }
        }

        if !template.force {
            bail!(
                "destination exists and differs (set force=true to replace): {}",
                template.dest.display()
            );
        }

        remove_existing_path(&template.dest)?;
    }

    if template.mkdirs
        && let Some(parent) = template.dest.parent()
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create destination directory: {}",
                parent.display()
            )
        })?;
    }

    fs::write(&template.dest, rendered).with_context(|| {
        format!(
            "failed to write rendered template destination: {}",
            template.dest.display()
        )
    })?;

    Ok((true, template_sensitive))
}

fn remove_existing_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;

    if metadata.file_type().is_symlink() || metadata.file_type().is_file() {
        fs::remove_file(path).with_context(|| format!("failed to remove file {}", path.display()))
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove directory {}", path.display()))
    } else {
        bail!("unsupported existing path type: {}", path.display());
    }
}

fn create_symlink(src: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dest).with_context(|| {
            format!(
                "failed to create symlink {} -> {}",
                dest.display(),
                src.display()
            )
        })?;
    }

    #[cfg(windows)]
    {
        let metadata = fs::metadata(src)
            .with_context(|| format!("failed to inspect source {}", src.display()))?;

        let result = if metadata.is_dir() {
            std::os::windows::fs::symlink_dir(src, dest)
        } else {
            std::os::windows::fs::symlink_file(src, dest)
        };

        result.with_context(|| {
            format!(
                "failed to create symlink {} -> {}. Hint: enable Developer Mode or run an elevated shell",
                dest.display(),
                src.display()
            )
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use crate::providers::ProviderRegistry;
    #[cfg(unix)]
    use keron_domain::LinkResource;
    use keron_domain::{PlanAction, PlanReport, PlannedOperation, Resource, TemplateResource};

    use super::{ApplyOptions, apply_plan};

    fn template_op(id: usize, src: PathBuf, dest: PathBuf) -> PlannedOperation {
        PlannedOperation {
            id,
            manifest: PathBuf::from("main.lua"),
            action: PlanAction::TemplateCreate,
            resource: Resource::Template(TemplateResource {
                src,
                dest,
                vars: BTreeMap::new(),
                force: false,
                mkdirs: true,
            }),
            summary: "render template".to_string(),
            would_change: true,
            conflict: false,
            hint: None,
            error: None,
            content_hash: None,
            dest_content_hash: None,
        }
    }

    #[test]
    fn apply_fail_fast_stops_after_first_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_src = temp.path().join("missing.tmpl");
        let ok_src = temp.path().join("files/ok.tmpl");
        let ok_dest = temp.path().join("out/ok.conf");
        fs::create_dir_all(
            ok_src
                .parent()
                .expect("template source should have a parent directory"),
        )
        .expect("mkdir");
        fs::write(&ok_src, "hello").expect("write template");

        let plan = PlanReport {
            discovered_manifests: vec![PathBuf::from("main.lua")],
            execution_order: vec![PathBuf::from("main.lua")],
            operations: vec![
                template_op(1, missing_src, temp.path().join("out/fail.conf")),
                template_op(2, ok_src, ok_dest.clone()),
            ],
            warnings: vec![],
            errors: vec![],
        };

        let providers = ProviderRegistry::from_providers(vec![]);
        let (report, _sensitive) = apply_plan(&plan, &providers, ApplyOptions::default());

        assert_eq!(report.results.len(), 1);
        assert!(!report.results[0].success);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("apply aborted after first failure")),
            "errors: {:?}",
            report.errors
        );
        assert!(!ok_dest.exists(), "second operation should not run");
    }

    #[test]
    fn apply_best_effort_runs_remaining_operations() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_src = temp.path().join("missing.tmpl");
        let ok_src = temp.path().join("files/ok.tmpl");
        let ok_dest = temp.path().join("out/ok.conf");
        fs::create_dir_all(
            ok_src
                .parent()
                .expect("template source should have a parent directory"),
        )
        .expect("mkdir");
        fs::write(&ok_src, "hello").expect("write template");

        let plan = PlanReport {
            discovered_manifests: vec![PathBuf::from("main.lua")],
            execution_order: vec![PathBuf::from("main.lua")],
            operations: vec![
                template_op(1, missing_src, temp.path().join("out/fail.conf")),
                template_op(2, ok_src, ok_dest.clone()),
            ],
            warnings: vec![],
            errors: vec![],
        };

        let providers = ProviderRegistry::from_providers(vec![]);
        let (report, _sensitive) = apply_plan(&plan, &providers, ApplyOptions { fail_fast: false });

        assert_eq!(report.results.len(), 2);
        assert!(!report.results[0].success);
        assert!(report.results[1].success);
        assert_eq!(fs::read_to_string(&ok_dest).expect("rendered"), "hello");
    }

    #[cfg(unix)]
    #[test]
    fn apply_link_force_replaces_dangling_symlink() {
        let temp = tempfile::tempdir().expect("tempdir");
        let src = temp.path().join("files/source.txt");
        let dest = temp.path().join("home/link.txt");
        let dangling_target = temp.path().join("home/missing-target.txt");
        fs::create_dir_all(
            src.parent()
                .expect("source file should have a parent directory"),
        )
        .expect("mkdir source parent");
        fs::create_dir_all(
            dest.parent()
                .expect("destination file should have a parent directory"),
        )
        .expect("mkdir destination parent");
        fs::write(&src, "hello").expect("write source");
        std::os::unix::fs::symlink(&dangling_target, &dest).expect("create dangling symlink");

        let plan = PlanReport {
            discovered_manifests: vec![PathBuf::from("main.lua")],
            execution_order: vec![PathBuf::from("main.lua")],
            operations: vec![PlannedOperation {
                id: 1,
                manifest: PathBuf::from("main.lua"),
                action: PlanAction::LinkReplace,
                resource: Resource::Link(LinkResource {
                    src: src.clone(),
                    dest: dest.clone(),
                    force: true,
                    mkdirs: false,
                }),
                summary: "replace link".to_string(),
                would_change: true,
                conflict: true,
                hint: None,
                error: None,
                content_hash: None,
                dest_content_hash: None,
            }],
            warnings: vec![],
            errors: vec![],
        };

        let providers = ProviderRegistry::from_providers(vec![]);
        let (report, _sensitive) = apply_plan(&plan, &providers, ApplyOptions::default());

        assert_eq!(report.results.len(), 1);
        assert!(report.results[0].success);
        let target = fs::read_link(&dest).expect("dest should be a symlink");
        assert_eq!(target, src);
    }
}
