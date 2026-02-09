#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::PathBuf;

use crate::providers::{PackageProvider, ProviderRegistry};
use anyhow::Result;
use keron_domain::{
    LinkResource, ManifestSpec, PackageResource, PackageState, PlanAction, Resource,
    TemplateResource,
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

    fn is_installed(&self, _package: &str) -> Result<bool> {
        Ok(false)
    }

    fn installed_packages(&self) -> Result<HashSet<String>> {
        Ok(HashSet::new())
    }

    fn install(&self, _package: &str) -> Result<()> {
        Ok(())
    }

    fn remove(&self, _package: &str) -> Result<()> {
        Ok(())
    }
}

fn sample_package_manifest(hint: Option<&str>) -> ManifestSpec {
    let mut spec = ManifestSpec::new(PathBuf::from("/tmp/main.lua"));
    spec.resources.push(Resource::Package(PackageResource {
        name: "git".to_string(),
        provider_hint: hint.map(str::to_string),
        state: PackageState::Present,
    }));
    spec
}

#[test]
fn warns_for_unsupported_provider_hint() {
    let spec = sample_package_manifest(Some("pacman"));
    let registry = ProviderRegistry::from_providers(vec![Box::new(FakeProvider {
        name: "brew",
        available: true,
    })]);

    let (report, _sensitive) = build_plan(
        std::slice::from_ref(&spec.id.path),
        std::slice::from_ref(&spec),
        std::slice::from_ref(&spec.id.path),
        &registry,
    )
    .expect("plan");

    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("not supported by keron")),
        "warnings: {:?}",
        report.warnings
    );
}

#[test]
fn warns_for_unavailable_hinted_provider() {
    let spec = sample_package_manifest(Some("brew"));
    let registry = ProviderRegistry::from_providers(vec![Box::new(FakeProvider {
        name: "brew",
        available: false,
    })]);

    let (report, _sensitive) = build_plan(
        std::slice::from_ref(&spec.id.path),
        std::slice::from_ref(&spec),
        std::slice::from_ref(&spec.id.path),
        &registry,
    )
    .expect("plan");

    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("not installed or not available")),
        "warnings: {:?}",
        report.warnings
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
        std::slice::from_ref(&spec.id.path),
        std::slice::from_ref(&spec),
        std::slice::from_ref(&spec.id.path),
        &registry,
    )
    .expect("plan");

    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("no package manager is available")),
        "warnings: {:?}",
        report.warnings
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
        dest,
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
        dest,
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
        dest,
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
        dest,
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
        dest,
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
        dest,
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
        dest,
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
        dest,
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
        dest,
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
        dest,
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
