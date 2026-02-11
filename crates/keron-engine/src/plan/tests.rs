#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use crate::error::ProviderError;
use crate::providers::{PackageProvider, ProviderRegistry};
use keron_domain::{
    AbsolutePath, CommandResource, LinkResource, ManifestSpec, PackageManagerName, PackageName,
    PackageResource, PackageState, PlanAction, Resource, TemplateResource,
};

use super::{build_plan, plan_link_operation, plan_template_operation, sha256_bytes};

struct FakeProvider {
    name: &'static str,
    available: bool,
}

impl PackageProvider for FakeProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn detect(&self) -> bool {
        self.available
    }

    fn is_installed(&self, _package: &str) -> Result<bool, ProviderError> {
        Ok(false)
    }

    fn installed_packages(&self) -> Result<HashSet<String>, ProviderError> {
        Ok(HashSet::new())
    }

    fn install(&self, _package: &str) -> Result<(), ProviderError> {
        Ok(())
    }

    fn remove(&self, _package: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

fn abs(path: PathBuf) -> AbsolutePath {
    AbsolutePath::try_from(path).expect("test path should be absolute")
}

fn sample_package_manifest_with(
    name: &str,
    hint: Option<&str>,
    state: PackageState,
) -> ManifestSpec {
    let mut spec = ManifestSpec::new(PathBuf::from("/tmp/main.lua"));
    spec.resources.push(Resource::Package(PackageResource {
        name: PackageName::try_from(name).expect("package name"),
        provider_hint: hint
            .map(PackageManagerName::try_from)
            .transpose()
            .expect("provider hint"),
        state,
    }));
    spec
}

fn sample_package_manifest(hint: Option<&str>) -> ManifestSpec {
    sample_package_manifest_with("git", hint, PackageState::Present)
}

fn create_temp_executable() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    #[cfg(windows)]
    let path = dir.path().join("keron-path-probe.cmd");
    #[cfg(not(windows))]
    let path = dir.path().join("keron-path-probe");

    #[cfg(windows)]
    fs::write(&path, "@echo off\r\necho keron\r\n").expect("write temp executable");
    #[cfg(not(windows))]
    fs::write(&path, "#!/bin/sh\necho keron\n").expect("write temp executable");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut perms = fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod");
    }

    (dir, path.to_string_lossy().into_owned())
}

#[test]
fn warns_for_unsupported_provider_hint() {
    let spec = sample_package_manifest(Some("pacman"));
    let registry = ProviderRegistry::from_providers(vec![Box::new(FakeProvider {
        name: "brew",
        available: true,
    })]);

    let (report, _sensitive) = build_plan(
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        std::slice::from_ref(&spec),
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        &registry,
    )
    .expect("plan");

    assert!(
        report.warnings.is_empty(),
        "warnings: {:?}",
        report.warnings
    );
    let hint = report.operations[0]
        .hint
        .as_deref()
        .expect("expected operation hint");
    assert!(hint.contains("not supported by keron"), "hint: {hint}");
}

#[test]
fn warns_for_unavailable_hinted_provider() {
    let spec = sample_package_manifest(Some("brew"));
    let registry = ProviderRegistry::from_providers(vec![Box::new(FakeProvider {
        name: "brew",
        available: false,
    })]);

    let (report, _sensitive) = build_plan(
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        std::slice::from_ref(&spec),
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        &registry,
    )
    .expect("plan");

    assert!(
        report.warnings.is_empty(),
        "warnings: {:?}",
        report.warnings
    );
    let hint = report.operations[0]
        .hint
        .as_deref()
        .expect("expected operation hint");
    assert!(
        hint.contains("not installed or not available"),
        "hint: {hint}"
    );
}

#[test]
fn warns_when_no_provider_available_for_unhinted_package() {
    let spec = sample_package_manifest(None);
    let registry = ProviderRegistry::from_providers(vec![
        Box::new(FakeProvider {
            name: "brew",
            available: false,
        }),
        Box::new(FakeProvider {
            name: "apt",
            available: false,
        }),
    ]);

    let (report, _sensitive) = build_plan(
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        std::slice::from_ref(&spec),
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        &registry,
    )
    .expect("plan");

    assert!(
        report.warnings.is_empty(),
        "warnings: {:?}",
        report.warnings
    );
    let hint = report.operations[0]
        .hint
        .as_deref()
        .expect("expected operation hint");
    assert!(
        hint.contains("no package manager is available"),
        "hint: {hint}"
    );
}

#[test]
fn warns_when_package_binary_is_already_on_path() {
    let (_tmp_dir, binary_path) = create_temp_executable();
    let spec = sample_package_manifest_with(&binary_path, Some("brew"), PackageState::Present);
    let registry = ProviderRegistry::from_providers(vec![Box::new(FakeProvider {
        name: "brew",
        available: true,
    })]);

    let (report, _sensitive) = build_plan(
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        std::slice::from_ref(&spec),
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        &registry,
    )
    .expect("plan");

    assert!(
        report.warnings.is_empty(),
        "warnings: {:?}",
        report.warnings
    );
    let hint = report.operations[0]
        .hint
        .as_deref()
        .expect("expected operation hint");
    assert!(
        hint.contains("outside default 'brew' install folders")
            && hint.contains(&format!("'{binary_path}'")),
        "hint: {hint}"
    );
    assert_eq!(report.operations.len(), 1);
    assert_eq!(report.operations[0].action, PlanAction::PackageInstall);
    assert!(!report.operations[0].conflict);
}

#[test]
fn does_not_warn_about_path_for_absent_packages() {
    let (_tmp_dir, binary_path) = create_temp_executable();
    let spec = sample_package_manifest_with(&binary_path, Some("brew"), PackageState::Absent);
    let registry = ProviderRegistry::from_providers(vec![Box::new(FakeProvider {
        name: "brew",
        available: true,
    })]);

    let (report, _sensitive) = build_plan(
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        std::slice::from_ref(&spec),
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        &registry,
    )
    .expect("plan");

    assert!(
        report.warnings.is_empty(),
        "warnings: {:?}",
        report.warnings
    );
    let hint = report.operations[0].hint.as_deref().unwrap_or_default();
    assert!(!hint.contains("outside default"), "hint: {hint}");
}

#[cfg(unix)]
#[test]
fn does_not_warn_when_path_is_under_default_provider_folder() {
    let spec = sample_package_manifest_with("sh", Some("apt"), PackageState::Present);
    let registry = ProviderRegistry::from_providers(vec![Box::new(FakeProvider {
        name: "apt",
        available: true,
    })]);

    let (report, _sensitive) = build_plan(
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        std::slice::from_ref(&spec),
        std::slice::from_ref(&spec.id.path.to_path_buf()),
        &registry,
    )
    .expect("plan");

    assert!(
        report.warnings.is_empty(),
        "warnings: {:?}",
        report.warnings
    );
    let hint = report.operations[0].hint.as_deref().unwrap_or_default();
    assert!(
        !hint.contains("outside default 'apt' install folders"),
        "hint: {hint}"
    );
}

fn sample_parallel_planning_spec(path: &str, package_name: &str) -> ManifestSpec {
    let mut spec = ManifestSpec::new(PathBuf::from(path));
    spec.resources.push(Resource::Command(CommandResource {
        binary: "keron-nonexistent-precheck-binary-a".to_string(),
        args: vec!["--dry-run".to_string()],
    }));
    spec.resources.push(Resource::Package(PackageResource {
        name: PackageName::try_from(package_name).expect("package name"),
        provider_hint: Some(PackageManagerName::try_from("brew").expect("provider hint")),
        state: PackageState::Present,
    }));
    spec.resources.push(Resource::Command(CommandResource {
        binary: "keron-nonexistent-precheck-binary-b".to_string(),
        args: vec!["--version".to_string()],
    }));
    spec
}

#[test]
fn build_plan_is_deterministic_across_repeated_runs() {
    let first = sample_parallel_planning_spec("/tmp/parallel-a.lua", "git");
    let second = sample_parallel_planning_spec("/tmp/parallel-b.lua", "ripgrep");
    let manifests = vec![first.id.path.to_path_buf(), second.id.path.to_path_buf()];
    let specs = vec![first, second];
    let registry = ProviderRegistry::from_providers(vec![Box::new(FakeProvider {
        name: "brew",
        available: true,
    })]);

    let (baseline_report, baseline_sensitive) =
        build_plan(&manifests, &specs, &manifests, &registry).expect("plan");

    for _ in 0..12 {
        let (report, sensitive) =
            build_plan(&manifests, &specs, &manifests, &registry).expect("deterministic plan");
        assert_eq!(report, baseline_report);
        assert_eq!(sensitive, baseline_sensitive);
    }
}

#[test]
fn build_plan_parallelization_preserves_operation_id_order() {
    let first = sample_parallel_planning_spec("/tmp/parallel-id-a.lua", "git");
    let second = sample_parallel_planning_spec("/tmp/parallel-id-b.lua", "ripgrep");
    let manifests = vec![first.id.path.to_path_buf(), second.id.path.to_path_buf()];
    let specs = vec![first, second];
    let registry = ProviderRegistry::from_providers(vec![Box::new(FakeProvider {
        name: "brew",
        available: true,
    })]);

    let (report, _sensitive) = build_plan(&manifests, &specs, &manifests, &registry).expect("plan");

    let operation_ids = report.operations.iter().map(|op| op.id).collect::<Vec<_>>();
    assert_eq!(operation_ids, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        report.operations[0].manifest,
        PathBuf::from("/tmp/parallel-id-a.lua")
    );
    assert_eq!(
        report.operations[3].manifest,
        PathBuf::from("/tmp/parallel-id-b.lua")
    );
}

#[cfg(unix)]
#[test]
fn link_noop_includes_content_hash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("source.txt");
    let dest = dir.path().join("link.txt");
    std::fs::write(&src, "hello").expect("write src");
    std::os::unix::fs::symlink(&src, &dest).expect("symlink");

    let link = LinkResource {
        src,
        dest: abs(dest),
        force: false,
        mkdirs: false,
    };
    let resource = Resource::Link(link.clone());
    let op = plan_link_operation(1, PathBuf::from("test.lua"), resource, &link);

    assert_eq!(op.action, PlanAction::LinkNoop);
    assert_eq!(op.content_hash, Some(sha256_bytes(b"hello")));
    assert!(op.dest_content_hash.is_none());
}

#[test]
fn link_conflict_with_matching_content_shows_enhanced_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("source.txt");
    let dest = dir.path().join("copy.txt");
    std::fs::write(&src, "same content").expect("write src");
    std::fs::write(&dest, "same content").expect("write dest");

    let link = LinkResource {
        src,
        dest: abs(dest),
        force: false,
        mkdirs: false,
    };
    let resource = Resource::Link(link.clone());
    let op = plan_link_operation(1, PathBuf::from("test.lua"), resource, &link);

    assert_eq!(op.action, PlanAction::LinkConflict);
    assert!(op.content_hash.is_some());
    assert_eq!(op.content_hash, op.dest_content_hash);
    let hint = op.hint.expect("should have hint");
    assert!(
        hint.contains("content matches source but is not a symlink"),
        "hint was: {hint}"
    );
}

#[test]
fn link_conflict_with_different_content_shows_standard_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("source.txt");
    let dest = dir.path().join("different.txt");
    std::fs::write(&src, "content A").expect("write src");
    std::fs::write(&dest, "content B").expect("write dest");

    let link = LinkResource {
        src,
        dest: abs(dest),
        force: false,
        mkdirs: false,
    };
    let resource = Resource::Link(link.clone());
    let op = plan_link_operation(1, PathBuf::from("test.lua"), resource, &link);

    assert_eq!(op.action, PlanAction::LinkConflict);
    assert!(op.content_hash.is_some());
    assert!(op.dest_content_hash.is_some());
    assert_ne!(op.content_hash, op.dest_content_hash);
    let hint = op.hint.expect("should have hint");
    assert!(
        hint.contains("set force=true or remove destination manually"),
        "hint was: {hint}"
    );
}

#[test]
fn link_create_includes_source_hash_no_dest_hash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("source.txt");
    let dest = dir.path().join("nonexistent.txt");
    std::fs::write(&src, "data").expect("write src");

    let link = LinkResource {
        src,
        dest: abs(dest),
        force: false,
        mkdirs: false,
    };
    let resource = Resource::Link(link.clone());
    let op = plan_link_operation(1, PathBuf::from("test.lua"), resource, &link);

    assert_eq!(op.action, PlanAction::LinkCreate);
    assert_eq!(op.content_hash, Some(sha256_bytes(b"data")));
    assert!(op.dest_content_hash.is_none());
}

#[test]
fn link_missing_source_has_no_hashes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("missing.txt");
    let dest = dir.path().join("dest.txt");

    let link = LinkResource {
        src,
        dest: abs(dest),
        force: false,
        mkdirs: false,
    };
    let resource = Resource::Link(link.clone());
    let op = plan_link_operation(1, PathBuf::from("test.lua"), resource, &link);

    assert_eq!(op.action, PlanAction::LinkConflict);
    assert!(op.content_hash.is_none());
    assert!(op.dest_content_hash.is_none());
}

#[cfg(unix)]
#[test]
fn link_dangling_destination_requires_force() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("source.txt");
    let dest = dir.path().join("link.txt");
    let dangling_target = dir.path().join("missing-target.txt");
    std::fs::write(&src, "hello").expect("write src");
    std::os::unix::fs::symlink(&dangling_target, &dest).expect("symlink");

    let link = LinkResource {
        src,
        dest: abs(dest),
        force: false,
        mkdirs: false,
    };
    let resource = Resource::Link(link.clone());
    let op = plan_link_operation(1, PathBuf::from("test.lua"), resource, &link);

    assert_eq!(op.action, PlanAction::LinkConflict);
    assert!(!op.would_change);
    assert!(op.error.is_some());
}

#[cfg(unix)]
#[test]
fn link_dangling_destination_force_plans_replace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("source.txt");
    let dest = dir.path().join("link.txt");
    let dangling_target = dir.path().join("missing-target.txt");
    std::fs::write(&src, "hello").expect("write src");
    std::os::unix::fs::symlink(&dangling_target, &dest).expect("symlink");

    let link = LinkResource {
        src,
        dest: abs(dest),
        force: true,
        mkdirs: false,
    };
    let resource = Resource::Link(link.clone());
    let op = plan_link_operation(1, PathBuf::from("test.lua"), resource, &link);

    assert_eq!(op.action, PlanAction::LinkReplace);
    assert!(op.would_change);
    assert!(op.error.is_none());
}

#[test]
fn template_create_includes_rendered_hash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("tmpl.txt");
    let dest = dir.path().join("output.txt");
    std::fs::write(&src, "Hello {{ name }}!").expect("write src");

    let mut vars = BTreeMap::new();
    vars.insert("name".to_string(), "world".to_string());

    let template = TemplateResource {
        src,
        dest: abs(dest),
        vars,
        force: false,
        mkdirs: false,
    };
    let resource = Resource::Template(template.clone());
    let (op, _sensitive) =
        plan_template_operation(1, PathBuf::from("test.lua"), resource, &template);

    assert_eq!(op.action, PlanAction::TemplateCreate);
    assert_eq!(op.content_hash, Some(sha256_bytes(b"Hello world!")));
    assert!(op.dest_content_hash.is_none());
}

#[test]
fn template_noop_includes_both_hashes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("tmpl.txt");
    let dest = dir.path().join("output.txt");
    std::fs::write(&src, "static content").expect("write src");
    std::fs::write(&dest, "static content").expect("write dest");

    let template = TemplateResource {
        src,
        dest: abs(dest),
        vars: BTreeMap::new(),
        force: false,
        mkdirs: false,
    };
    let resource = Resource::Template(template.clone());
    let (op, _sensitive) =
        plan_template_operation(1, PathBuf::from("test.lua"), resource, &template);

    assert_eq!(op.action, PlanAction::TemplateNoop);
    let expected = sha256_bytes(b"static content");
    assert_eq!(op.content_hash, Some(expected.clone()));
    assert_eq!(op.dest_content_hash, Some(expected));
}

#[cfg(unix)]
#[test]
fn template_dangling_destination_with_force_plans_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("tmpl.txt");
    let dest = dir.path().join("output.txt");
    let dangling_target = dir.path().join("missing-target.txt");
    std::fs::write(&src, "hello").expect("write src");
    std::os::unix::fs::symlink(&dangling_target, &dest).expect("symlink");

    let template = TemplateResource {
        src,
        dest: abs(dest),
        vars: BTreeMap::new(),
        force: true,
        mkdirs: false,
    };
    let resource = Resource::Template(template.clone());
    let (op, _sensitive) =
        plan_template_operation(1, PathBuf::from("test.lua"), resource, &template);

    assert_eq!(op.action, PlanAction::TemplateUpdate);
    assert!(op.would_change);
    assert!(!op.conflict);
    assert!(op.error.is_none());
}
