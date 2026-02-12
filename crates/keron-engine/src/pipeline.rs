use std::collections::BTreeSet;
use std::path::Path;

use keron_domain::{PlanAction, PlanReport, Resource};

use crate::{
    PipelineError, ProviderRegistry, build_execution_order, build_plan, discover_manifests,
    evaluate_many_with_warnings,
};

/// Build a plan report for all manifests under a folder.
///
/// # Errors
///
/// Returns an error when manifest discovery/evaluation fails or no manifests are found.
pub fn build_plan_for_folder(
    folder: &Path,
    providers: &ProviderRegistry,
) -> std::result::Result<(PlanReport, BTreeSet<String>), PipelineError> {
    let manifests = discover_manifests(folder)?;
    if manifests.is_empty() {
        return Err(PipelineError::NoManifests {
            folder: folder.to_path_buf(),
        });
    }

    let (specs, manifest_warnings, mut sensitive_values) = evaluate_many_with_warnings(&manifests)?;
    match build_execution_order(&specs) {
        Ok(order) => {
            let (mut report, plan_sensitive) = build_plan(&manifests, &specs, &order, providers)?;
            report.warnings.extend(manifest_warnings);
            sensitive_values.extend(plan_sensitive);
            Ok((report, sensitive_values))
        }
        Err(error) => {
            // Keep plan output renderable even with a broken dependency graph.
            let mut fallback_order = manifests.clone();
            fallback_order.sort();
            let (mut report, plan_sensitive) =
                build_plan(&manifests, &specs, &fallback_order, providers)?;
            report.errors.push(error.to_string());
            report.warnings.extend(manifest_warnings);
            sensitive_values.extend(plan_sensitive);
            Ok((report, sensitive_values))
        }
    }
}

#[must_use]
pub fn has_potentially_destructive_forced_changes(report: &PlanReport) -> bool {
    report
        .operations
        .iter()
        .any(|operation| match (&operation.action, &operation.resource) {
            (PlanAction::LinkReplace, Resource::Link(link)) => link.force,
            (PlanAction::TemplateUpdate, Resource::Template(template)) => template.force,
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use keron_domain::{
        AbsolutePath, LinkResource, PlanAction, PlanReport, PlannedOperation, Resource,
        TemplateResource,
    };

    use super::has_potentially_destructive_forced_changes;

    fn abs(path: PathBuf) -> AbsolutePath {
        AbsolutePath::try_from(path).expect("test path should be absolute")
    }

    fn temp_abs(name: &str) -> PathBuf {
        std::env::temp_dir().join("keron-pipeline-tests").join(name)
    }

    fn make_operation(action: PlanAction, resource: Resource) -> PlannedOperation {
        PlannedOperation {
            id: 1,
            manifest: PathBuf::from("/tmp/main.lua"),
            action,
            resource,
            summary: String::new(),
            would_change: true,
            conflict: false,
            hint: None,
            error: None,
            content_hash: None,
            dest_content_hash: None,
        }
    }

    #[test]
    fn detects_force_link_replace() {
        let report = PlanReport {
            discovered_manifests: vec![],
            execution_order: vec![],
            operations: vec![make_operation(
                PlanAction::LinkReplace,
                Resource::Link(LinkResource {
                    src: PathBuf::from("/tmp/src"),
                    dest: abs(temp_abs("dest-link-replace")),
                    force: true,
                    mkdirs: false,
                }),
            )],
            warnings: vec![],
            errors: vec![],
        };

        assert!(has_potentially_destructive_forced_changes(&report));
    }

    #[test]
    fn ignores_non_replacement_force_create() {
        let report = PlanReport {
            discovered_manifests: vec![],
            execution_order: vec![],
            operations: vec![make_operation(
                PlanAction::LinkCreate,
                Resource::Link(LinkResource {
                    src: PathBuf::from("/tmp/src"),
                    dest: abs(temp_abs("dest-link-create")),
                    force: true,
                    mkdirs: false,
                }),
            )],
            warnings: vec![],
            errors: vec![],
        };

        assert!(!has_potentially_destructive_forced_changes(&report));
    }

    #[test]
    fn detects_force_template_update() {
        let report = PlanReport {
            discovered_manifests: vec![],
            execution_order: vec![],
            operations: vec![make_operation(
                PlanAction::TemplateUpdate,
                Resource::Template(TemplateResource {
                    src: PathBuf::from("/tmp/src.tmpl"),
                    dest: abs(temp_abs("dest-template-update")),
                    vars: BTreeMap::new(),
                    force: true,
                    mkdirs: false,
                }),
            )],
            warnings: vec![],
            errors: vec![],
        };

        assert!(has_potentially_destructive_forced_changes(&report));
    }
}
