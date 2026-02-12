use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use keron_domain::{
    ApplyOperationResult, ApplyReport, CommandResource, LinkResource, PackageResource,
    PackageState, PlanReport, PlannedOperation, Resource, TemplateResource,
};

use crate::error::ApplyError;
use crate::fs_util::{path_exists_including_dangling_symlink, symlink_points_to};
use crate::providers::{ProviderRegistry, ProviderSnapshot, apply_package, package_state};
use crate::template::render_template_string;
#[cfg(unix)]
use nix::fcntl::{AT_FDCWD, AtFlags};
#[cfg(unix)]
use nix::unistd::{Gid, Uid, chown, fchownat};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

type ApplyResult<T> = std::result::Result<T, ApplyError>;
const INTERNAL_APPLY_OP_SUBCOMMAND: &str = "__apply-op";
const INTERNAL_APPLY_OP_FLAG: &str = "--op-file";
const KERON_ELEVATED_CHILD_ENV: &str = "KERON_ELEVATED_CHILD";
const KERON_INVOKING_UID_ENV: &str = "KERON_INVOKING_UID";
const KERON_INVOKING_GID_ENV: &str = "KERON_INVOKING_GID";

#[cfg(unix)]
const UNIX_ELEVATION_LAUNCHERS: [&str; 3] = ["run0", "doas", "sudo"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnershipTarget {
    uid: u32,
    gid: u32,
}

/// Apply a serialized planned operation from a JSON payload file.
///
/// # Errors
///
/// Returns an error if the payload cannot be read or decoded, or if executing
/// the operation fails.
pub fn apply_operation_from_file(path: &Path, providers: &ProviderRegistry) -> ApplyResult<bool> {
    let payload = fs::read(path).map_err(|source| ApplyError::Io {
        context: format!(
            "failed to read elevated operation payload: {}",
            path.display()
        ),
        source,
    })?;
    let operation: PlannedOperation =
        serde_json::from_slice(&payload).map_err(|source| ApplyError::OperationPayloadDecode {
            path: path.to_path_buf(),
            source,
        })?;
    apply_operation_local(&operation, providers)
}

/// Apply a single planned operation without invoking elevation wrappers.
///
/// # Errors
///
/// Returns an error if the operation is blocked by planner diagnostics or if
/// execution of the resource operation fails.
pub fn apply_operation_local(
    operation: &PlannedOperation,
    providers: &ProviderRegistry,
) -> ApplyResult<bool> {
    if let Some(error) = &operation.error {
        return Err(ApplyError::Invariant {
            message: format!("operation {} blocked: {error}", operation.id),
        });
    }
    let provider_snapshot = providers.snapshot();
    apply_operation_with_snapshot(operation, providers, &provider_snapshot, false)
        .map(|(changed, _sensitive)| changed)
}

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
    let provider_snapshot = providers.snapshot();

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

        let applied = apply_operation_with_snapshot(operation, providers, &provider_snapshot, true);

        match applied {
            Ok((changed, operation_sensitive)) => {
                sensitive_values.extend(operation_sensitive);
                results.push(ApplyOperationResult {
                    operation_id: operation.id,
                    summary: operation.summary.clone(),
                    success: true,
                    changed,
                    error: None,
                });
            }
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

fn apply_operation_with_snapshot(
    operation: &PlannedOperation,
    providers: &ProviderRegistry,
    snapshot: &ProviderSnapshot,
    allow_elevation: bool,
) -> ApplyResult<(bool, BTreeSet<String>)> {
    if allow_elevation && should_use_elevated_runner(operation) {
        let changed = apply_elevated_operation(operation)?;
        return Ok((changed, BTreeSet::new()));
    }

    match &operation.resource {
        Resource::Link(link) => apply_link(link).map(|changed| (changed, BTreeSet::new())),
        Resource::Template(template) => apply_template(template),
        Resource::Package(package) => apply_package_resource(package, providers, snapshot)
            .map(|changed| (changed, BTreeSet::new())),
        Resource::Command(command) => {
            apply_command_resource(command).map(|changed| (changed, BTreeSet::new()))
        }
    }
}

fn should_use_elevated_runner(operation: &PlannedOperation) -> bool {
    operation.would_change
        && resource_requests_elevation(&operation.resource)
        && !running_as_elevated_child()
        && !already_elevated_on_host()
}

const fn resource_requests_elevation(resource: &Resource) -> bool {
    match resource {
        Resource::Link(link) => link.elevate,
        Resource::Template(template) => template.elevate,
        Resource::Package(package) => package.elevate,
        Resource::Command(command) => command.elevate,
    }
}

fn running_as_elevated_child() -> bool {
    env::var_os(KERON_ELEVATED_CHILD_ENV).is_some()
}

#[cfg(unix)]
fn already_elevated_on_host() -> bool {
    nix::unistd::Uid::effective().is_root()
}

#[cfg(not(unix))]
const fn already_elevated_on_host() -> bool {
    false
}

fn apply_elevated_operation(operation: &PlannedOperation) -> ApplyResult<bool> {
    let payload_path = write_operation_payload(operation)?;
    let executable = std::env::current_exe().map_err(|source| ApplyError::Io {
        context: "failed to determine current executable for elevated operation".to_string(),
        source,
    })?;

    #[cfg(unix)]
    let mut command = {
        let Some(launcher) = select_unix_elevation_launcher() else {
            let _ = fs::remove_file(&payload_path);
            return Err(ApplyError::ElevatedRunnerMissing {
                detail: "no supported elevation launcher found on PATH (tried: run0, doas, sudo)"
                    .to_string(),
            });
        };
        let mut command = Command::new(launcher);
        command.arg(&executable);
        command.arg(INTERNAL_APPLY_OP_SUBCOMMAND);
        command.arg(INTERNAL_APPLY_OP_FLAG);
        command.arg(&payload_path);
        command.env(KERON_ELEVATED_CHILD_ENV, "1");
        let target = desired_ownership_target();
        command.env(KERON_INVOKING_UID_ENV, target.uid.to_string());
        command.env(KERON_INVOKING_GID_ENV, target.gid.to_string());
        command
    };

    #[cfg(windows)]
    let mut command = {
        let exe = powershell_quote(&executable.display().to_string());
        let payload = powershell_quote(&payload_path.display().to_string());
        let script = format!(
            "$p = Start-Process -FilePath '{exe}' -ArgumentList @('{subcommand}', '{flag}', '{payload}') -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
            subcommand = INTERNAL_APPLY_OP_SUBCOMMAND,
            flag = INTERNAL_APPLY_OP_FLAG
        );
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        command
    };

    #[cfg(not(any(unix, windows)))]
    let mut command = {
        let _ = fs::remove_file(&payload_path);
        return Err(ApplyError::ElevatedRunnerMissing {
            detail: "this host OS does not support elevated operation launching".to_string(),
        });
    };

    let output = command.output().map_err(|source| ApplyError::Io {
        context: "failed to launch elevated operation".to_string(),
        source,
    })?;

    let _ = fs::remove_file(&payload_path);

    if output.status.success() {
        Ok(operation.would_change)
    } else {
        Err(ApplyError::ElevatedOperationFailed {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

#[cfg(unix)]
fn select_unix_elevation_launcher() -> Option<&'static str> {
    UNIX_ELEVATION_LAUNCHERS
        .into_iter()
        .find(|launcher| which::which(launcher).is_ok())
}

#[cfg(windows)]
fn powershell_quote(input: &str) -> String {
    input.replace('\'', "''")
}

fn write_operation_payload(operation: &PlannedOperation) -> ApplyResult<PathBuf> {
    let payload = serde_json::to_vec(operation)
        .map_err(|source| ApplyError::OperationPayloadEncode { source })?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |duration| duration.as_nanos());
    let filename = format!(
        "keron-op-{}-{}-{}.json",
        operation.id,
        std::process::id(),
        timestamp
    );
    let path = std::env::temp_dir().join(filename);
    fs::write(&path, payload).map_err(|source| ApplyError::Io {
        context: format!(
            "failed to write elevated operation payload: {}",
            path.display()
        ),
        source,
    })?;
    Ok(path)
}

#[cfg(unix)]
fn desired_ownership_target() -> OwnershipTarget {
    let parsed_uid = env::var(KERON_INVOKING_UID_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let parsed_group = env::var(KERON_INVOKING_GID_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok());

    match (parsed_uid, parsed_group) {
        (Some(uid), Some(gid)) => OwnershipTarget { uid, gid },
        _ => OwnershipTarget {
            uid: Uid::current().as_raw(),
            gid: Gid::current().as_raw(),
        },
    }
}

#[cfg(not(unix))]
const fn ownership_target_for_elevation(_elevate: bool) -> Option<OwnershipTarget> {
    None
}

#[cfg(unix)]
fn ownership_target_for_elevation(elevate: bool) -> Option<OwnershipTarget> {
    if elevate {
        Some(desired_ownership_target())
    } else {
        None
    }
}

#[cfg(unix)]
fn apply_ownership_if_needed(
    path: &Path,
    ownership: OwnershipTarget,
    nofollow_symlink: bool,
) -> ApplyResult<bool> {
    let metadata = fs::symlink_metadata(path).map_err(|source| ApplyError::Io {
        context: format!("failed to inspect ownership for {}", path.display()),
        source,
    })?;
    if metadata.uid() == ownership.uid && metadata.gid() == ownership.gid {
        return Ok(false);
    }

    let owner = Some(Uid::from_raw(ownership.uid));
    let group = Some(Gid::from_raw(ownership.gid));
    if nofollow_symlink {
        fchownat(AT_FDCWD, path, owner, group, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|error| {
            ApplyError::Invariant {
                message: format!(
                    "failed to set ownership on {} to {}:{}: {error}",
                    path.display(),
                    ownership.uid,
                    ownership.gid
                ),
            }
        })?;
    } else {
        chown(path, owner, group).map_err(|error| ApplyError::Invariant {
            message: format!(
                "failed to set ownership on {} to {}:{}: {error}",
                path.display(),
                ownership.uid,
                ownership.gid
            ),
        })?;
    }

    Ok(true)
}

#[cfg(not(unix))]
fn apply_ownership_if_needed(
    _path: &Path,
    _ownership: OwnershipTarget,
    _nofollow_symlink: bool,
) -> ApplyResult<bool> {
    Ok(false)
}

fn apply_package_resource(
    package: &PackageResource,
    providers: &ProviderRegistry,
    snapshot: &ProviderSnapshot,
) -> ApplyResult<bool> {
    if let Ok((_provider, installed)) = package_state(
        providers,
        snapshot,
        &package.name,
        package.provider_hint.as_deref(),
    ) {
        match (package.state, installed) {
            (PackageState::Present, false) => {
                let _ = apply_package(
                    providers,
                    snapshot,
                    &package.name,
                    package.provider_hint.as_deref(),
                    true,
                )?;
                Ok(true)
            }
            (PackageState::Absent, true) => {
                let _ = apply_package(
                    providers,
                    snapshot,
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
            snapshot,
            &package.name,
            package.provider_hint.as_deref(),
            install,
        )?;
        Ok(true)
    }
}

fn apply_command_resource(command: &CommandResource) -> ApplyResult<bool> {
    let binary_path =
        which::which(&command.binary).map_err(|_| ApplyError::CommandBinaryNotFound {
            binary: command.binary.clone(),
        })?;

    let status = Command::new(binary_path)
        .args(&command.args)
        .status()
        .map_err(|source| ApplyError::CommandSpawn {
            binary: command.binary.clone(),
            source,
        })?;

    if status.success() {
        Ok(true)
    } else {
        Err(ApplyError::CommandFailed { status })
    }
}

fn apply_link(link: &LinkResource) -> ApplyResult<bool> {
    let ownership_target = ownership_target_for_elevation(link.elevate);
    let source_exists =
        path_exists_including_dangling_symlink(&link.src).map_err(|source| ApplyError::Io {
            context: format!("failed to inspect source {}", link.src.display()),
            source,
        })?;
    if !source_exists {
        return Err(ApplyError::Invariant {
            message: format!("source does not exist: {}", link.src.display()),
        });
    }

    let destination_exists =
        path_exists_including_dangling_symlink(link.dest.as_path()).map_err(|source| {
            ApplyError::Io {
                context: format!("failed to inspect destination {}", link.dest.display()),
                source,
            }
        })?;
    if destination_exists {
        if symlink_points_to(link.dest.as_path(), &link.src).map_err(|source| ApplyError::Io {
            context: format!("failed to inspect symlink {}", link.dest.display()),
            source,
        })? {
            if let Some(target) = ownership_target {
                let ownership_changed =
                    apply_ownership_if_needed(link.dest.as_path(), target, true)?;
                return Ok(ownership_changed);
            }
            return Ok(false);
        }

        if !link.force {
            return Err(ApplyError::Invariant {
                message: format!(
                    "destination exists and differs (set force=true to replace): {}",
                    link.dest.display()
                ),
            });
        }

        remove_existing_path(link.dest.as_path())?;
    }

    if link.mkdirs
        && let Some(parent) = link.dest.parent()
    {
        fs::create_dir_all(parent).map_err(|source| ApplyError::Io {
            context: format!(
                "failed to create destination directory: {}",
                parent.display()
            ),
            source,
        })?;
    }

    create_symlink(&link.src, link.dest.as_path())?;
    if let Some(target) = ownership_target {
        let _ = apply_ownership_if_needed(link.dest.as_path(), target, true)?;
    }
    Ok(true)
}

fn apply_template(template: &TemplateResource) -> ApplyResult<(bool, BTreeSet<String>)> {
    let ownership_target = ownership_target_for_elevation(template.elevate);
    let source_exists =
        path_exists_including_dangling_symlink(&template.src).map_err(|source| ApplyError::Io {
            context: format!(
                "failed to inspect template source {}",
                template.src.display()
            ),
            source,
        })?;
    if !source_exists {
        return Err(ApplyError::Invariant {
            message: format!("template source does not exist: {}", template.src.display()),
        });
    }

    let source = fs::read_to_string(&template.src).map_err(|source| ApplyError::Io {
        context: format!("failed to read template source {}", template.src.display()),
        source,
    })?;
    let (rendered, template_sensitive) =
        render_template_string(&source, &template.vars).map_err(|source| {
            ApplyError::TemplateRender {
                path: template.src.clone(),
                source,
            }
        })?;

    let destination_exists = path_exists_including_dangling_symlink(template.dest.as_path())
        .map_err(|source| ApplyError::Io {
            context: format!(
                "failed to inspect template destination {}",
                template.dest.display()
            ),
            source,
        })?;
    if destination_exists {
        match fs::read_to_string(template.dest.as_path()) {
            Ok(current) if current == rendered => {
                if let Some(target) = ownership_target {
                    let ownership_changed =
                        apply_ownership_if_needed(template.dest.as_path(), target, false)?;
                    return Ok((ownership_changed, template_sensitive));
                }
                return Ok((false, template_sensitive));
            }
            Ok(_) => {}
            Err(error) => {
                if !template.force {
                    return Err(ApplyError::Invariant {
                        message: format!(
                            "destination exists and cannot be read (set force=true to replace): {} ({error})",
                            template.dest.display()
                        ),
                    });
                }
            }
        }

        if !template.force {
            return Err(ApplyError::Invariant {
                message: format!(
                    "destination exists and differs (set force=true to replace): {}",
                    template.dest.display()
                ),
            });
        }

        remove_existing_path(template.dest.as_path())?;
    }

    if template.mkdirs
        && let Some(parent) = template.dest.parent()
    {
        fs::create_dir_all(parent).map_err(|source| ApplyError::Io {
            context: format!(
                "failed to create destination directory: {}",
                parent.display()
            ),
            source,
        })?;
    }

    fs::write(template.dest.as_path(), rendered).map_err(|source| ApplyError::Io {
        context: format!(
            "failed to write rendered template destination: {}",
            template.dest.display()
        ),
        source,
    })?;

    if let Some(target) = ownership_target {
        let _ = apply_ownership_if_needed(template.dest.as_path(), target, false)?;
    }

    Ok((true, template_sensitive))
}

fn remove_existing_path(path: &Path) -> ApplyResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(|source| ApplyError::Io {
        context: format!("failed to read metadata for {}", path.display()),
        source,
    })?;

    if metadata.file_type().is_symlink() || metadata.file_type().is_file() {
        fs::remove_file(path).map_err(|source| ApplyError::Io {
            context: format!("failed to remove file {}", path.display()),
            source,
        })
    } else if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(|source| ApplyError::Io {
            context: format!("failed to remove directory {}", path.display()),
            source,
        })
    } else {
        Err(ApplyError::Invariant {
            message: format!("unsupported existing path type: {}", path.display()),
        })
    }
}

fn create_symlink(src: &Path, dest: &Path) -> ApplyResult<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dest).map_err(|source| ApplyError::Io {
            context: format!(
                "failed to create symlink {} -> {}",
                dest.display(),
                src.display()
            ),
            source,
        })?;
    }

    #[cfg(windows)]
    {
        let metadata = fs::metadata(src).map_err(|source| ApplyError::Io {
            context: format!("failed to inspect source {}", src.display()),
            source,
        })?;

        let result = if metadata.is_dir() {
            std::os::windows::fs::symlink_dir(src, dest)
        } else {
            std::os::windows::fs::symlink_file(src, dest)
        };

        result.map_err(|source| ApplyError::Io {
            context: format!(
                "failed to create symlink {} -> {}. Hint: enable Developer Mode or run an elevated shell",
                dest.display(),
                src.display()
            ),
            source,
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
    use keron_domain::{
        AbsolutePath, PlanAction, PlanReport, PlannedOperation, Resource, TemplateResource,
    };

    use super::{ApplyOptions, apply_plan};

    fn abs(path: PathBuf) -> AbsolutePath {
        AbsolutePath::try_from(path).expect("test path should be absolute")
    }

    fn template_op(id: usize, src: PathBuf, dest: PathBuf) -> PlannedOperation {
        PlannedOperation {
            id,
            manifest: PathBuf::from("main.lua"),
            action: PlanAction::TemplateCreate,
            resource: Resource::Template(TemplateResource {
                src,
                dest: abs(dest),
                vars: BTreeMap::new(),
                force: false,
                mkdirs: true,
                elevate: false,
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
                    dest: abs(dest.clone()),
                    force: true,
                    mkdirs: false,
                    elevate: false,
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
