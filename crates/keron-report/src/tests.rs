#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use keron_domain::{
    AbsolutePath, ApplyOperationResult, ApplyReport, CommandResource, LinkResource,
    PackageManagerName, PackageName, PackageResource, PackageState, PlanAction, PlanReport,
    PlannedOperation, Resource, TemplateResource,
};

use super::{
    ColorChoice, OutputFormat, RenderOptions, redact_sensitive, render_apply, render_plan,
};

#[test]
fn redact_basic_replacement() {
    let mut sensitive = BTreeSet::new();
    sensitive.insert("my-secret-token".to_string());
    let input = "token is my-secret-token here";
    assert_eq!(
        redact_sensitive(input, &sensitive),
        "token is [REDACTED] here"
    );
}

#[test]
fn redact_short_value_skipped() {
    let mut sensitive = BTreeSet::new();
    sensitive.insert("ab".to_string());
    sensitive.insert("x".to_string());
    let input = "ab and x remain";
    assert_eq!(redact_sensitive(input, &sensitive), input);
}

fn base_options() -> RenderOptions {
    RenderOptions {
        color: ColorChoice::Never,
        verbose: false,
        target: Some("examples/complex".to_string()),
    }
}

fn verbose_options() -> RenderOptions {
    RenderOptions {
        color: ColorChoice::Never,
        verbose: true,
        target: Some("examples/complex".to_string()),
    }
}

fn abs(path: &str) -> AbsolutePath {
    AbsolutePath::try_from(PathBuf::from(path)).expect("test path should be absolute")
}

fn link_op(id: usize, action: PlanAction, src: &str, dest: &str) -> PlannedOperation {
    PlannedOperation {
        id,
        manifest: PathBuf::from("/tmp/base.lua"),
        action,
        resource: Resource::Link(LinkResource {
            src: PathBuf::from(src),
            dest: abs(dest),
            force: false,
            mkdirs: true,
        }),
        summary: String::new(),
        would_change: matches!(action, PlanAction::LinkCreate | PlanAction::LinkReplace),
        conflict: matches!(action, PlanAction::LinkConflict),
        hint: if matches!(action, PlanAction::LinkConflict) {
            Some("set force=true or remove destination manually".to_string())
        } else {
            None
        },
        error: if matches!(action, PlanAction::LinkConflict) {
            Some("destination conflicts with requested symlink".to_string())
        } else {
            None
        },
        content_hash: None,
        dest_content_hash: None,
    }
}

fn template_op(id: usize, action: PlanAction, src: &str, dest: &str) -> PlannedOperation {
    PlannedOperation {
        id,
        manifest: PathBuf::from("/tmp/workstation.lua"),
        action,
        resource: Resource::Template(TemplateResource {
            src: PathBuf::from(src),
            dest: abs(dest),
            vars: BTreeMap::default(),
            force: false,
            mkdirs: true,
        }),
        summary: String::new(),
        would_change: matches!(
            action,
            PlanAction::TemplateCreate | PlanAction::TemplateUpdate
        ),
        conflict: false,
        hint: None,
        error: None,
        content_hash: None,
        dest_content_hash: None,
    }
}

fn package_op(
    id: usize,
    action: PlanAction,
    name: &str,
    provider: Option<&str>,
) -> PlannedOperation {
    PlannedOperation {
        id,
        manifest: PathBuf::from("/tmp/workstation.lua"),
        action,
        resource: Resource::Package(PackageResource {
            name: PackageName::try_from(name).expect("package name"),
            provider_hint: provider
                .map(PackageManagerName::try_from)
                .transpose()
                .expect("provider hint"),
            state: PackageState::Present,
        }),
        summary: String::new(),
        would_change: matches!(
            action,
            PlanAction::PackageInstall | PlanAction::PackageRemove
        ),
        conflict: false,
        hint: None,
        error: None,
        content_hash: None,
        dest_content_hash: None,
    }
}

fn command_op(id: usize, binary: &str) -> PlannedOperation {
    PlannedOperation {
        id,
        manifest: PathBuf::from("/tmp/base.lua"),
        action: PlanAction::CommandRun,
        resource: Resource::Command(CommandResource {
            binary: binary.to_string(),
            args: Vec::new(),
        }),
        summary: String::new(),
        would_change: true,
        conflict: false,
        hint: None,
        error: None,
        content_hash: None,
        dest_content_hash: None,
    }
}

fn make_report() -> PlanReport {
    PlanReport {
        discovered_manifests: vec![PathBuf::from("/tmp/base.lua")],
        execution_order: vec![PathBuf::from("/tmp/base.lua")],
        operations: vec![],
        warnings: vec![],
        errors: vec![],
    }
}

// Plan tests

#[test]
fn plan_shows_structured_detail() {
    let mut report = make_report();
    report.operations = vec![
        link_op(
            1,
            PlanAction::LinkCreate,
            "/tmp/src/zshrc",
            "/tmp/dest/.zshrc",
        ),
        template_op(
            2,
            PlanAction::TemplateUpdate,
            "/tmp/src/star.tmpl",
            "/tmp/dest/star.toml",
        ),
        package_op(3, PlanAction::PackageInstall, "ripgrep", Some("brew")),
        command_op(4, "echo configured"),
    ];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("+ create link"));
    assert!(text.contains("/tmp/src/zshrc"));
    assert!(text.contains("-> /tmp/dest/.zshrc"));
    assert!(text.contains("~ rerender template"));
    assert!(text.contains("/tmp/src/star.tmpl"));
    assert!(text.contains("-> /tmp/dest/star.toml"));
    assert!(text.contains("+ install package"));
    assert!(text.contains("ripgrep"));
    assert!(text.contains("via brew"));
    assert!(text.contains("+ run command"));
    assert!(text.contains("echo configured"));
}

#[test]
fn plan_noop_summary_in_normal_mode() {
    let mut report = make_report();
    report.operations = vec![
        link_op(1, PlanAction::LinkCreate, "/tmp/src", "/tmp/dest"),
        link_op(2, PlanAction::LinkNoop, "/tmp/src2", "/tmp/dest2"),
        package_op(3, PlanAction::PackageNoop, "git", Some("brew")),
    ];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    // Noops should be summarized, not listed individually
    assert!(text.contains("2 unchanged (1 link, 1 package)"));
    assert!(!text.contains("= link up to date"));
    assert!(!text.contains("= package up to date"));
}

#[test]
fn plan_verbose_shows_all_ops_and_metadata() {
    let mut report = make_report();
    report.operations = vec![
        link_op(1, PlanAction::LinkCreate, "/tmp/src", "/tmp/dest"),
        link_op(2, PlanAction::LinkNoop, "/tmp/src2", "/tmp/dest2"),
    ];

    let text = render_plan(&report, OutputFormat::Text, &verbose_options())
        .expect("render should succeed");

    // Verbose should show noops individually
    assert!(text.contains("= link up to date"));
    // Should show op IDs and manifest names
    assert!(text.contains("#1"));
    assert!(text.contains("#2"));
    assert!(text.contains("base.lua"));
}

#[test]
fn plan_tally_footer() {
    let mut report = make_report();
    report.operations = vec![
        link_op(1, PlanAction::LinkCreate, "/tmp/src", "/tmp/dest"),
        template_op(2, PlanAction::TemplateUpdate, "/tmp/s", "/tmp/d"),
        link_op(3, PlanAction::LinkNoop, "/tmp/src2", "/tmp/dest2"),
    ];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("Plan:"));
    assert!(text.contains("1 to add"));
    assert!(text.contains("1 to change"));
    assert!(text.contains("1 unchanged"));
}

#[test]
fn plan_conflict_shows_hint() {
    let mut report = make_report();
    let mut conflict = link_op(1, PlanAction::LinkConflict, "/tmp/src", "/tmp/dest/.bashrc");
    // Conflict with hint but no error
    conflict.error = None;
    conflict.hint = Some("set force=true or remove destination manually".to_string());

    report.operations = vec![conflict];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("! conflict"));
    assert!(text.contains("! conflict /tmp/src"));
    assert!(!text.contains("! conflict      /tmp/src"));
    assert!(text.contains("set force=true or remove destination manually"));
}

#[test]
fn plan_empty_operations() {
    let report = make_report();
    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("Nothing to do."));
}

#[test]
fn plan_empty_operations_with_errors_shows_errors() {
    let mut report = make_report();
    report.errors = vec!["dependency cycle detected".to_string()];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("Nothing to do."));
    assert!(text.contains("error:"));
    assert!(text.contains("dependency cycle detected"));
}

#[test]
fn plan_empty_operations_with_warnings_shows_warnings() {
    let mut report = make_report();
    report.warnings = vec!["provider is unavailable".to_string()];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("Nothing to do."));
    assert!(text.contains("warn:"));
    assert!(text.contains("provider is unavailable"));
}

#[test]
fn plan_warnings_and_errors() {
    let mut report = make_report();
    report.operations = vec![link_op(1, PlanAction::LinkCreate, "/tmp/s", "/tmp/d")];
    report.warnings = vec!["package provider 'pacman' is not supported".to_string()];
    report.errors = vec!["dependency cycle detected".to_string()];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("warn:"));
    assert!(text.contains("package provider 'pacman' is not supported"));
    assert!(text.contains("error:"));
    assert!(text.contains("dependency cycle detected"));
}

#[test]
fn plan_global_diagnostics_render_before_operation_lines() {
    let mut report = make_report();
    report.operations = vec![link_op(1, PlanAction::LinkCreate, "/tmp/s", "/tmp/d")];
    report.warnings = vec!["provider check warning".to_string()];
    report.errors = vec!["provider check error".to_string()];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    let warn_idx = text.find("warn: provider check warning").expect("warn");
    let error_idx = text.find("error: provider check error").expect("error");
    let op_idx = text.find("+ create link").expect("op line");
    assert!(
        warn_idx < op_idx,
        "warning should appear before operation line"
    );
    assert!(
        error_idx < op_idx,
        "error should appear before operation line"
    );
}

#[test]
fn plan_operation_errors_render_inline_in_non_verbose_mode() {
    let mut report = make_report();
    let conflict = link_op(1, PlanAction::LinkConflict, "/tmp/src", "/tmp/dest");
    report.operations = vec![conflict];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("! conflict"));
    assert!(text.contains("error: destination conflicts with requested symlink"));
}

#[test]
fn plan_hides_default_folder_list_in_non_verbose_warning_output() {
    let mut report = make_report();
    report.warnings = vec![
        "package 'foo' resolves to /tmp/foo which is outside default 'brew' install folders (default folders: /opt/homebrew/bin, /usr/local/bin)".to_string(),
    ];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("outside default 'brew' install folders"));
    assert!(!text.contains("default folders:"));
}

#[test]
fn plan_shows_default_folder_list_in_verbose_warning_output() {
    let mut report = make_report();
    report.warnings = vec![
        "package 'foo' resolves to /tmp/foo which is outside default 'brew' install folders (default folders: /opt/homebrew/bin, /usr/local/bin)".to_string(),
    ];

    let text = render_plan(&report, OutputFormat::Text, &verbose_options())
        .expect("render should succeed");

    assert!(text.contains("outside default 'brew' install folders"));
    assert!(text.contains("default folders: /opt/homebrew/bin, /usr/local/bin"));
}

// Apply tests

#[test]
fn apply_shows_structured_detail() {
    let plan = PlanReport {
        discovered_manifests: vec![PathBuf::from("/tmp/main.lua")],
        execution_order: vec![PathBuf::from("/tmp/main.lua")],
        operations: vec![
            link_op(1, PlanAction::LinkReplace, "/tmp/src", "/tmp/dest"),
            command_op(2, "echo hi"),
        ],
        warnings: vec![],
        errors: vec![],
    };

    let apply = ApplyReport {
        plan,
        results: vec![
            ApplyOperationResult {
                operation_id: 1,
                summary: "replaced".to_string(),
                success: true,
                changed: true,
                error: None,
            },
            ApplyOperationResult {
                operation_id: 2,
                summary: "failed".to_string(),
                success: false,
                changed: false,
                error: Some("command exited with non-zero status".to_string()),
            },
        ],
        errors: vec![],
    };

    let text =
        render_apply(&apply, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("~ replaced link"));
    assert!(text.contains("/tmp/src"));
    assert!(text.contains("-> /tmp/dest"));
    assert!(text.contains("! failed command"));
    assert!(text.contains("command exited with non-zero status"));
}

#[test]
fn apply_noop_summary() {
    let plan = PlanReport {
        discovered_manifests: vec![PathBuf::from("/tmp/main.lua")],
        execution_order: vec![PathBuf::from("/tmp/main.lua")],
        operations: vec![link_op(1, PlanAction::LinkCreate, "/tmp/src", "/tmp/dest")],
        warnings: vec![],
        errors: vec![],
    };

    let apply = ApplyReport {
        plan,
        results: vec![
            ApplyOperationResult {
                operation_id: 1,
                summary: "created".to_string(),
                success: true,
                changed: true,
                error: None,
            },
            ApplyOperationResult {
                operation_id: 2,
                summary: "noop".to_string(),
                success: true,
                changed: false,
                error: None,
            },
            ApplyOperationResult {
                operation_id: 3,
                summary: "noop".to_string(),
                success: true,
                changed: false,
                error: None,
            },
        ],
        errors: vec![],
    };

    let text =
        render_apply(&apply, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("2 unchanged"));
    assert!(text.contains("Applied:"));
    assert!(text.contains("1 added"));
}

#[test]
fn apply_empty_results_with_errors_shows_errors() {
    let plan = PlanReport {
        discovered_manifests: vec![PathBuf::from("/tmp/main.lua")],
        execution_order: vec![PathBuf::from("/tmp/main.lua")],
        operations: vec![],
        warnings: vec![],
        errors: vec![],
    };

    let apply = ApplyReport {
        plan,
        results: vec![],
        errors: vec!["operation 2 failed: command exited with non-zero status".to_string()],
    };

    let text =
        render_apply(&apply, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("Nothing to do."));
    assert!(text.contains("error:"));
    assert!(text.contains("operation 2 failed"));
}

#[test]
fn apply_tally_footer() {
    let plan = PlanReport {
        discovered_manifests: vec![PathBuf::from("/tmp/main.lua")],
        execution_order: vec![PathBuf::from("/tmp/main.lua")],
        operations: vec![
            link_op(1, PlanAction::LinkCreate, "/tmp/s", "/tmp/d"),
            command_op(2, "echo fail"),
        ],
        warnings: vec![],
        errors: vec![],
    };

    let apply = ApplyReport {
        plan,
        results: vec![
            ApplyOperationResult {
                operation_id: 1,
                summary: "ok".to_string(),
                success: true,
                changed: true,
                error: None,
            },
            ApplyOperationResult {
                operation_id: 2,
                summary: "fail".to_string(),
                success: false,
                changed: false,
                error: Some("boom".to_string()),
            },
        ],
        errors: vec![],
    };

    let text =
        render_apply(&apply, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("Applied:"));
    assert!(text.contains("1 added"));
    assert!(text.contains("1 failed"));
}

// Alignment test

#[test]
fn labels_align_without_color() {
    let mut report = make_report();
    report.operations = vec![
        link_op(1, PlanAction::LinkCreate, "/tmp/src", "/tmp/dest"),
        template_op(2, PlanAction::TemplateUpdate, "/tmp/s", "/tmp/d"),
        package_op(3, PlanAction::PackageInstall, "rg", Some("brew")),
    ];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    // All labels should be padded to 16 chars, so resource details start at the same column
    for line in text.lines() {
        if line.starts_with("  +") || line.starts_with("  ~") || line.starts_with("  =") {
            // The symbol is at index 2, then space, then 16-char padded label, then detail
            // Format: "  S LLLLLLLLLLLLLLLL detail"
            // So the detail starts around character 20
            assert!(
                line.len() > 20,
                "line should be long enough for aligned output: {line}"
            );
        }
    }
}

// JSON unchanged test

#[test]
fn json_output_unchanged() {
    let mut report = make_report();
    report.operations = vec![link_op(1, PlanAction::LinkCreate, "/tmp/s", "/tmp/d")];

    let json = render_plan(&report, OutputFormat::Json, &base_options())
        .expect("json render should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&json).expect("should be valid JSON");
    assert!(parsed.get("operations").is_some());
}

// Command truncation test

#[test]
fn long_command_truncated_in_normal_mode() {
    let mut report = make_report();
    let long_cmd = "echo ".to_string() + &"x".repeat(100);
    report.operations = vec![command_op(1, &long_cmd)];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(text.contains("..."));

    // In verbose mode, full command is shown
    let verbose_text = render_plan(&report, OutputFormat::Text, &verbose_options())
        .expect("render should succeed");

    assert!(verbose_text.contains(&long_cmd));
}

#[test]
fn verbose_shows_checksums_when_present() {
    let mut report = make_report();
    let mut op = link_op(1, PlanAction::LinkCreate, "/tmp/src", "/tmp/dest");
    op.content_hash =
        Some("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".to_string());
    op.dest_content_hash =
        Some("1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef".to_string());
    report.operations = vec![op];

    let text = render_plan(&report, OutputFormat::Text, &verbose_options())
        .expect("render should succeed");

    assert!(
        text.contains("content:  sha256:abcdef123456"),
        "text was:\n{text}"
    );
    assert!(
        text.contains("dest:     sha256:1234567890ab"),
        "text was:\n{text}"
    );
}

#[test]
fn non_verbose_hides_checksums() {
    let mut report = make_report();
    let mut op = link_op(1, PlanAction::LinkCreate, "/tmp/src", "/tmp/dest");
    op.content_hash =
        Some("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".to_string());
    report.operations = vec![op];

    let text =
        render_plan(&report, OutputFormat::Text, &base_options()).expect("render should succeed");

    assert!(!text.contains("sha256:"), "text was:\n{text}");
}

#[test]
fn json_output_includes_checksum_fields() {
    let mut report = make_report();
    let mut op = link_op(1, PlanAction::LinkCreate, "/tmp/s", "/tmp/d");
    op.content_hash = Some("abc123".to_string());
    op.dest_content_hash = None;
    report.operations = vec![op];

    let json = render_plan(&report, OutputFormat::Json, &base_options())
        .expect("json render should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&json).expect("should be valid JSON");
    let ops = parsed.get("operations").expect("has operations");
    let first = &ops[0];
    assert_eq!(
        first.get("content_hash").and_then(|v| v.as_str()),
        Some("abc123")
    );
    assert!(first.get("dest_content_hash").expect("has field").is_null());
}
