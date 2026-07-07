use super::*;
use crate::plan::{PackageManager, ResourceKind, ShellKind};
use std::path::PathBuf;

fn render(plan: &Plan) -> String {
    render_with(plan, false, false)
}

fn render_verbose(plan: &Plan) -> String {
    render_with(plan, false, true)
}

fn render_with(plan: &Plan, color: bool, verbose: bool) -> String {
    let mut buf = Vec::new();
    render_plan(&mut buf, plan, RenderOptions { color, verbose }).unwrap();
    String::from_utf8(buf).unwrap()
}

#[test]
fn truly_empty_plan_prints_bare_no_changes() {
    let out = render(&Plan::default());
    assert!(out.contains("No changes."), "got: {out}");
    assert!(
        !out.contains("up to date"),
        "no resources means no roster line: {out}"
    );
}

#[test]
fn all_noop_plan_lists_each_resource_and_counts_them() {
    let plan = Plan {
        changes: vec![
            ResourceChange {
                address: "brew:git".into(),
                kind: ResourceKind::Package,
                action: Action::NoOp,
                before: Some(ResourceState::Package {
                    manager: PackageManager::Brew,
                    name: "git".into(),
                    tap: None,
                }),
                after: Some(ResourceState::Package {
                    manager: PackageManager::Brew,
                    name: "git".into(),
                    tap: None,
                }),
                requires_elevation: false,
                requires_force: false,
            },
            ResourceChange {
                address: "brew:fd".into(),
                kind: ResourceKind::Package,
                action: Action::NoOp,
                before: Some(ResourceState::Package {
                    manager: PackageManager::Brew,
                    name: "fd".into(),
                    tap: None,
                }),
                after: Some(ResourceState::Package {
                    manager: PackageManager::Brew,
                    name: "fd".into(),
                    tap: None,
                }),
                requires_elevation: false,
                requires_force: false,
            },
        ],
    };
    let out = render(&plan);
    assert!(
        out.contains("package.\"brew:git\" is up to date"),
        "missing git header: {out}"
    );
    assert!(
        out.contains("package.\"brew:fd\" is up to date"),
        "missing fd header: {out}"
    );
    assert!(
        out.contains("No changes. 2 resources are up to date."),
        "missing footer: {out}"
    );
    assert!(
        !out.contains("resource \"package\""),
        "NoOps should not render a body block: {out}"
    );
}

#[test]
fn single_noop_uses_singular_resource_noun() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "brew:git".into(),
            kind: ResourceKind::Package,
            action: Action::NoOp,
            before: None,
            after: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "git".into(),
                tap: None,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("No changes. 1 resource is up to date."),
        "got: {out}"
    );
}

#[test]
fn mixed_plan_renders_noops_inline_and_appends_unchanged_count() {
    let mut plan = Plan::sample();
    plan.changes.push(ResourceChange {
        address: "brew:git".into(),
        kind: ResourceKind::Package,
        action: Action::NoOp,
        before: Some(ResourceState::Package {
            manager: PackageManager::Brew,
            name: "git".into(),
            tap: None,
        }),
        after: Some(ResourceState::Package {
            manager: PackageManager::Brew,
            name: "git".into(),
            tap: None,
        }),
        requires_elevation: false,
        requires_force: false,
    });
    let out = render(&plan);
    assert!(
        out.contains("package.\"brew:git\" is up to date"),
        "missing noop header: {out}"
    );
    assert!(
        out.contains("Plan: 1 to add, 1 to change, 0 to run, 1 unchanged."),
        "missing footer with unchanged count: {out}"
    );
}

#[test]
fn sample_renders_each_action() {
    let out = render(&Plan::sample());
    assert!(out.contains("template.\"~/.zshrc\" will be created"));
    assert!(out.contains("symlink.\"~/.config/nvim\" will be updated in-place"));
    assert!(out.contains("Plan: 1 to add, 1 to change, 0 to run."));
    assert!(out.contains("+ resource \"template\""));
    assert!(out.contains("~ resource \"symlink\""));
    assert!(out.contains("~ source = \"/old/target\" -> \"/new/target\""));
}

#[test]
fn summary_renders_positive_elevated_and_force_counts() {
    let mut plan = Plan::sample();
    plan.changes[0].requires_elevation = true;
    plan.changes[1].requires_force = true;
    let out = render(&plan);
    assert!(
        out.contains("Plan: 1 to add, 1 to change, 0 to run (1 elevated) (1 force)."),
        "got: {out}"
    );
}

#[test]
fn no_color_means_no_escape_codes() {
    let out = render(&Plan::sample());
    assert!(!out.contains('\x1b'), "ANSI escape leaked: {out}");
}

#[test]
fn color_path_includes_escape_codes() {
    let out = render_with(&Plan::sample(), true, false);
    assert!(out.contains('\x1b'));
}

#[test]
fn create_renders_target_and_content_summary_by_default() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/etc/x".into(),
            kind: ResourceKind::Template,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Template {
                path: PathBuf::from("/etc/x"),
                content: "hi".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("+ target  = \"/etc/x\""),
        "missing target line: {out}"
    );
    // Default mode hides the body; a count-only summary stays.
    assert!(
        out.contains("+ content: 1 line"),
        "missing content summary: {out}",
    );
    // The opt-in flag appears exactly once, as a footer hint —
    // not repeated on every body summary line.
    assert_eq!(
        out.matches("--verbose-will-reveal-sensitive-content")
            .count(),
        1,
        "expected footer hint to appear once: {out}",
    );
    assert!(
        out.contains("hidden by default"),
        "missing footer text: {out}",
    );
    // The literal content must not leak.
    assert!(!out.contains("\"hi\""), "raw content leaked: {out}");
}

#[test]
fn verbose_mode_omits_footer_hint() {
    // No point pointing at the flag the user already typed.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/etc/x".into(),
            kind: ResourceKind::Template,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Template {
                path: PathBuf::from("/etc/x"),
                content: "hi".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render_verbose(&plan);
    assert!(
        !out.contains("--verbose-will-reveal-sensitive-content"),
        "footer hint should be suppressed in verbose mode: {out}",
    );
    assert!(
        !out.contains("hidden by default"),
        "footer text should be suppressed in verbose mode: {out}",
    );
}

#[test]
fn footer_hint_suppressed_when_no_body_blocks() {
    // A plan that only changes package / symlink resources has no
    // bodies to reveal, so the footer hint serves no purpose.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "brew:ripgrep".into(),
            kind: ResourceKind::Package,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "ripgrep".into(),
                tap: None,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        !out.contains("--verbose-will-reveal-sensitive-content"),
        "footer hint should be omitted for body-less plans: {out}",
    );
}

#[test]
fn body_summary_lines_do_not_carry_inline_hint() {
    // The hint moved to a single footer; per-line repetition was
    // noisy when several bodies appeared in one plan. Pin it: a
    // summary line ends right after the line counts.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "old\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "new\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    // The body line ends with the counts — no `(use ...)` suffix.
    assert!(
        out.contains("~ content: 1 line removed, 1 line added\n"),
        "summary should end at the line counts: {out:?}",
    );
}

#[test]
fn create_renders_full_content_in_verbose_mode() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/etc/x".into(),
            kind: ResourceKind::Template,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Template {
                path: PathBuf::from("/etc/x"),
                content: "hi".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render_verbose(&plan);
    assert!(out.contains("+ content:"), "missing content header: {out}");
    assert!(out.contains("+hi"), "missing added line: {out}");
    assert!(
        out.contains("Verbose mode"),
        "missing verbose banner: {out}",
    );
}

#[test]
fn create_symlink_renders_source_and_target_lines() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/a".into(),
            kind: ResourceKind::Symlink,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Symlink {
                from: PathBuf::from("/a"),
                to: PathBuf::from("/b"),
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("+ source = \"/b\""),
        "missing source line: {out}"
    );
    assert!(
        out.contains("+ target = \"/a\""),
        "missing target line: {out}"
    );
}

#[test]
fn update_file_renders_only_changed_fields() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "old".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "new".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    // Default mode: one summary line with line counts, no
    // verbatim content.
    assert!(
        out.contains("~ content: 1 line removed, 1 line added"),
        "missing content summary: {out}",
    );
    assert!(!out.contains("\"old\""), "old content leaked: {out}");
    assert!(!out.contains("\"new\""), "new content leaked: {out}");
    assert!(
        !out.contains("~ target"),
        "target should be unchanged: {out}",
    );
}

#[test]
fn update_file_renders_full_diff_in_verbose_mode() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "old\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "new\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render_verbose(&plan);
    assert!(out.contains("~ content:"), "missing content header: {out}");
    assert!(out.contains("@@"), "missing hunk header: {out}");
    assert!(out.contains("-old"), "missing removed line: {out}");
    assert!(out.contains("+new"), "missing added line: {out}");
}

#[test]
fn verbose_diff_keeps_content_lines_that_resemble_diff_headers() {
    // A removed line whose content starts with `-- ` renders as
    // `--- ...` in the unified diff (and `++ ` as `+++ ...`). Such
    // lines are real user content — e.g. Lua comments — and used to be
    // silently filtered out as "synthetic headers", hiding changed
    // lines from the reviewed diff.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "-- old lua comment\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "++ new marker\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render_verbose(&plan);
    assert!(
        out.contains("--- old lua comment"),
        "removed `-- ` content line must stay visible: {out}"
    );
    assert!(
        out.contains("+++ new marker"),
        "added `++ ` content line must stay visible: {out}"
    );
}

#[test]
fn update_file_renders_only_path_when_content_unchanged() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/old".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/old"),
                content: "same".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/new"),
                content: "same".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("~ target  = \"/old\" -> \"/new\""),
        "got: {out}"
    );
    assert!(
        !out.contains("~ content"),
        "content should be unchanged: {out}"
    );
}

#[test]
fn update_symlink_renders_only_changed_field() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/s".into(),
            kind: ResourceKind::Symlink,
            action: Action::Update,
            before: Some(ResourceState::Symlink {
                from: PathBuf::from("/s"),
                to: PathBuf::from("/t1"),
            }),
            after: Some(ResourceState::Symlink {
                from: PathBuf::from("/s"),
                to: PathBuf::from("/t2"),
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(out.contains("~ source = \"/t1\" -> \"/t2\""), "got: {out}");
    assert!(
        !out.contains("~ target"),
        "target should be unchanged: {out}"
    );
}

#[test]
fn update_kind_change_renders_before_then_after() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "old".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Symlink {
                from: PathBuf::from("/x"),
                to: PathBuf::from("/y"),
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("- target  = \"/x\""),
        "missing before half: {out}"
    );
    assert!(
        out.contains("+ target = \"/x\""),
        "missing create half: {out}"
    );
}

#[test]
fn action_color_uses_distinct_codes() {
    assert_eq!(action_color(Action::Create), GREEN);
    assert_eq!(action_color(Action::Update), YELLOW);
    assert_eq!(action_color(Action::Run), YELLOW);
    assert_eq!(action_color(Action::NoOp), RESET);
}

#[test]
fn color_output_uses_action_specific_code_for_each_action() {
    let out = render_with(&Plan::sample(), true, false);
    assert!(out.contains(GREEN), "create should be green: {out}");
    assert!(out.contains(YELLOW), "update should be yellow: {out}");
}

#[test]
fn escape_inline_handles_each_special_char() {
    assert_eq!(escape_inline("a\\b"), "a\\\\b");
    assert_eq!(escape_inline("\"quoted\""), "\\\"quoted\\\"");
    assert_eq!(escape_inline("line1\nline2"), "line1\\nline2");
    assert_eq!(escape_inline("col1\tcol2"), "col1\\tcol2");
}

#[test]
fn escape_inline_passes_plain_text_unchanged() {
    assert_eq!(escape_inline("hello world"), "hello world");
}

#[test]
fn escape_inline_neutralizes_ansi_and_carriage_returns() {
    // A hostile `.keron` could otherwise embed `\r` or `\x1b[A`
    // to cursor-up and overwrite the rendered diff so the user
    // sees a benign-looking plan while the planner has queued
    // writes to a different target.
    assert_eq!(escape_inline("safe\rmalicious"), "safe\\rmalicious");
    assert_eq!(escape_inline("\x1b[2K"), "\\u{001b}[2K");
    assert_eq!(escape_inline("\0null"), "\\u{0000}null");
}

#[test]
fn escape_inline_neutralizes_bidi_overrides() {
    // U+202E (Right-to-Left Override) and similar bidi controls
    // are not ASCII-control but can rewrite the apparent order
    // of subsequent characters in a terminal.
    let bidi = "good\u{202e}lave";
    assert_eq!(escape_inline(bidi), "good\\u{202e}lave");
}

#[test]
fn diff_render_neutralizes_control_chars_in_address_and_path() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/etc/passwd\r/safe".into(),
            kind: ResourceKind::Symlink,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Symlink {
                from: PathBuf::from("/etc/passwd\r/safe"),
                to: PathBuf::from("/var/target\x1b[2K"),
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        !out.contains('\r'),
        "carriage return leaked through render: {out:?}"
    );
    assert!(!out.contains('\x1b'), "ESC leaked through render: {out:?}");
    assert!(out.contains("\\r"), "expected \\r escape: {out}");
    assert!(
        out.contains("\\u{001b}"),
        "expected \\u{{001b}} escape: {out}"
    );
}

#[test]
fn verbose_mode_preserves_real_tabs_and_newlines() {
    // In verbose mode the body is rendered as a real unified diff,
    // so `\t` and `\n` are intentionally kept as literal control
    // characters (otherwise the diff would be unreadable — the
    // whole point of switching from the inline `"a\tb\n"` form is
    // to make the structure visible). `\r`, ANSI, and bidi
    // controls inside a single line are still escaped (covered by
    // `verbose_mode_sanitizes_inline_control_chars` below).
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "a\tb\nc\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render_verbose(&plan);
    // Body is split into lines by `\n`, so the literal newline
    // separates two added lines; the tab stays inside its line.
    assert!(out.contains("a\tb"), "tab not preserved in body: {out:?}");
    assert!(
        !out.contains("\\t"),
        "tab should NOT be escaped in verbose mode: {out:?}",
    );
}

#[test]
fn default_mode_hides_special_chars_entirely() {
    // Default mode never prints body content, so even special
    // chars in the content are absent from the output. This is
    // the "safe-by-default" guarantee.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "a\tb\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(!out.contains('\t'), "tab leaked in default mode: {out:?}");
    assert!(!out.contains("\\t"), "escaped tab leaked: {out:?}");
}

#[test]
fn default_mode_hides_template_content_regardless_of_sensitive_flag() {
    // The resource-level `sensitive` flag drives executor file
    // mode (0o600 vs 0o644). For diff rendering it attaches a
    // `[sensitive]` hint to the default-mode body summary so the
    // operator sees which bodies will print secrets if they opt
    // in to verbose. The hint does NOT redact — content is
    // hidden by default for *every* template, sensitive or not.
    let plan = Plan {
        changes: vec![
            ResourceChange {
                address: "/x".into(),
                kind: ResourceKind::Template,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Template {
                    path: PathBuf::from("/x"),
                    content: "token=secret-value".into(),
                    sensitive: true,
                }),
                requires_elevation: false,
                requires_force: false,
            },
            ResourceChange {
                address: "/y".into(),
                kind: ResourceKind::Template,
                action: Action::Update,
                before: Some(ResourceState::Template {
                    path: PathBuf::from("/y"),
                    content: "old-secret".into(),
                    sensitive: true,
                }),
                after: Some(ResourceState::Template {
                    path: PathBuf::from("/y"),
                    content: "new-secret".into(),
                    sensitive: true,
                }),
                requires_elevation: false,
                requires_force: true,
            },
        ],
    };
    let out = render(&plan);
    // Hint appears on both the one-sided Create and the two-sided
    // Update summaries — they're the bodies that carry secrets.
    assert!(
        out.contains("+ content [sensitive]: 1 line"),
        "missing one-sided sensitive summary: {out}",
    );
    assert!(
        out.contains("~ content [sensitive]: 1 line removed, 1 line added"),
        "missing update sensitive summary: {out}",
    );
    assert!(
        out.contains("template.\"/y\" will be updated in-place  (force required)"),
        "missing header: {out}",
    );
    // The literal secret-bearing content must not appear.
    assert!(!out.contains("secret-value"), "create secret leaked: {out}");
    assert!(!out.contains("old-secret"), "update before leaked: {out}");
    assert!(!out.contains("new-secret"), "update after leaked: {out}");
}

#[test]
fn default_mode_omits_sensitive_marker_when_flag_is_off() {
    // Non-sensitive templates and scripts never see the
    // `[sensitive]` marker — pin that so a future change to the
    // hint-emission predicate doesn't accidentally tag everything.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "old\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "new\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("~ content: 1 line removed, 1 line added"),
        "expected plain (no-marker) summary: {out}",
    );
    assert!(!out.contains("[sensitive]"), "marker leaked: {out}");
}

#[test]
fn default_mode_shell_script_sensitive_attaches_marker_and_shows_body() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "with-secret".into(),
            kind: ResourceKind::Shell,
            action: Action::Run,
            before: None,
            after: Some(ResourceState::Shell {
                kind: ShellKind::Sh,
                name: "with-secret".into(),
                cwd: PathBuf::from("/repo"),
                script: "TOKEN=abc\necho ok\n".into(),
                sensitive: true,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("> script [sensitive]:"),
        "missing sensitive shell header: {out}",
    );
    assert!(out.contains("+TOKEN=abc"), "script content missing: {out}");
}

#[test]
fn update_sensitive_either_side_attaches_marker() {
    // Conservative rule: if either before or after carries the
    // sensitive flag, the marker shows. This handles the typical
    // public→secret transition (template was non-sensitive on
    // disk; the new render is sensitive) without forcing the
    // operator to mark both sides identically.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "old\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "new\n".into(),
                sensitive: true,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("[sensitive]"),
        "marker missing when only after-side sensitive: {out}",
    );
}

#[test]
fn verbose_mode_reveals_content_even_when_sensitive_flag_is_set() {
    // The flag name `--verbose-will-reveal-sensitive-content` is
    // the consent — the renderer does not redact in verbose mode,
    // even for templates the manifest tagged sensitive. The
    // banner in `render_plan` advertises this.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "old-public\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "new-secret\n".into(),
                sensitive: true,
            }),
            requires_elevation: false,
            requires_force: true,
        }],
    };
    let out = render_verbose(&plan);
    assert!(out.contains("Verbose mode"), "missing banner: {out}");
    assert!(out.contains("-old-public"), "old content missing: {out}");
    assert!(out.contains("+new-secret"), "new content missing: {out}");
}

#[test]
fn create_package_renders_manager_and_name_lines() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "brew:ripgrep".into(),
            kind: ResourceKind::Package,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "ripgrep".into(),
                tap: None,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("package.\"brew:ripgrep\" will be created"),
        "missing header: {out}",
    );
    assert!(
        out.contains("+ resource \"package\""),
        "missing kind: {out}"
    );
    assert!(
        out.contains("+ manager = \"brew\""),
        "missing manager line: {out}",
    );
    assert!(
        out.contains("+ name    = \"ripgrep\""),
        "missing name line: {out}",
    );
}

#[test]
fn update_package_renders_only_changed_fields() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "brew:git".into(),
            kind: ResourceKind::Package,
            action: Action::Update,
            before: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "git".into(),
                tap: None,
            }),
            after: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "git@2".into(),
                tap: None,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("~ name    = \"git\" -> \"git@2\""),
        "missing name diff: {out}",
    );
    assert!(
        !out.contains("~ manager"),
        "manager should be unchanged: {out}",
    );
}

#[test]
fn package_manager_change_renders_both_diff_lines() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "brew:ripgrep".into(),
            kind: ResourceKind::Package,
            action: Action::Update,
            before: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "ripgrep".into(),
                tap: None,
            }),
            after: Some(ResourceState::Package {
                manager: PackageManager::Cargo,
                name: "ripgrep".into(),
                tap: None,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("~ manager = \"brew\" -> \"cargo\""),
        "missing manager diff: {out}",
    );
    assert!(!out.contains("~ name"), "name unchanged: {out}");
}

#[test]
fn update_shell_renders_non_body_fields_inline_and_script_diff() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "refresh".into(),
            kind: ResourceKind::Shell,
            action: Action::Update,
            before: Some(ResourceState::Shell {
                kind: ShellKind::Sh,
                name: "refresh".into(),
                cwd: PathBuf::from("/repo"),
                script: "echo old\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Shell {
                kind: ShellKind::Bash,
                name: "reload".into(),
                cwd: PathBuf::from("/repo/subdir"),
                script: "echo new\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("~ kind   = \"sh\" -> \"bash\""),
        "missing kind diff: {out}"
    );
    assert!(
        out.contains("~ name   = \"refresh\" -> \"reload\""),
        "missing name diff: {out}"
    );
    assert!(
        out.contains("~ cwd    = \"/repo\" -> \"/repo/subdir\""),
        "missing cwd diff: {out}"
    );
    assert!(
        out.contains("~ script:"),
        "missing script diff header: {out}",
    );
    assert!(out.contains("-echo old"), "old script missing: {out}");
    assert!(out.contains("+echo new"), "new script missing: {out}");
}

#[test]
fn update_shell_in_verbose_mode_shows_unified_script_diff() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "refresh".into(),
            kind: ResourceKind::Shell,
            action: Action::Update,
            before: Some(ResourceState::Shell {
                kind: ShellKind::Sh,
                name: "refresh".into(),
                cwd: PathBuf::from("/repo"),
                script: "echo old\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Shell {
                kind: ShellKind::Sh,
                name: "refresh".into(),
                cwd: PathBuf::from("/repo"),
                script: "echo new\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render_verbose(&plan);
    assert!(out.contains("~ script:"), "missing script header: {out}");
    assert!(out.contains("-echo old"), "missing removed line: {out}");
    assert!(out.contains("+echo new"), "missing added line: {out}");
}

#[test]
fn run_shell_renders_non_body_fields_inline_and_script_body() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "refresh".into(),
            kind: ResourceKind::Shell,
            action: Action::Run,
            before: None,
            after: Some(ResourceState::Shell {
                kind: ShellKind::Sh,
                name: "refresh".into(),
                cwd: PathBuf::from("/repo"),
                script: "echo ok\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("shell.\"refresh\" will run"),
        "missing header: {out}",
    );
    assert!(out.contains("> resource \"shell\""), "missing kind: {out}");
    assert!(
        out.contains("> kind   = \"sh\""),
        "missing kind line: {out}"
    );
    assert!(
        out.contains("> name   = \"refresh\""),
        "missing name line: {out}"
    );
    assert!(
        out.contains("> cwd    = \"/repo\""),
        "missing cwd line: {out}"
    );
    assert!(
        out.contains("> script:"),
        "missing script diff header: {out}",
    );
    assert!(out.contains("+echo ok"), "script content missing: {out}");
    assert!(out.contains("Plan: 0 to add, 0 to change, 1 to run."));
}

#[test]
fn run_shell_in_verbose_mode_shows_one_sided_script_diff() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "refresh".into(),
            kind: ResourceKind::Shell,
            action: Action::Run,
            before: None,
            after: Some(ResourceState::Shell {
                kind: ShellKind::Sh,
                name: "refresh".into(),
                cwd: PathBuf::from("/repo"),
                script: "echo ok\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render_verbose(&plan);
    assert!(out.contains("> script:"), "missing script header: {out}");
    assert!(out.contains("+echo ok"), "missing added line: {out}");
}

// --- Counts, banner, and sanitization for the verbose/default split ---

#[test]
fn default_mode_line_counts_match_textdiff_iter_changes() {
    // Synthesize a 4-line→3-line update and verify the summary's
    // counts agree with the underlying `similar` view, so a
    // future refactor doesn't accidentally inflate / deflate the
    // shown numbers.
    let before = "a\nb\nc\nd\n";
    let after = "a\nB\nd\n";
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: before.into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: after.into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    // Two lines removed (`b`, `c`), one line added (`B`). Pin the
    // exact pluralization too — line / lines.
    assert!(
        out.contains("~ content: 2 lines removed, 1 line added"),
        "summary line counts off: {out}",
    );
}

#[test]
fn count_lines_pins_pluralization_edge_cases() {
    assert_eq!(count_lines(""), 0);
    assert_eq!(count_lines("a"), 1);
    assert_eq!(count_lines("a\n"), 1);
    assert_eq!(count_lines("a\nb"), 2);
    assert_eq!(count_lines("a\nb\n"), 2);
}

#[test]
fn verbose_banner_appears_only_when_body_blocks_present() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "hi".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    assert!(
        render_verbose(&plan).contains("Verbose mode"),
        "banner should appear when there's a body block to reveal",
    );

    // No body blocks: a package-only plan has nothing to reveal,
    // so the banner is suppressed even with verbose set.
    let packages_only = Plan {
        changes: vec![ResourceChange {
            address: "brew:ripgrep".into(),
            kind: ResourceKind::Package,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "ripgrep".into(),
                tap: None,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    assert!(
        !render_verbose(&packages_only).contains("Verbose mode"),
        "banner should NOT appear when there are no bodies to reveal",
    );
}

#[test]
fn verbose_color_paints_hunk_header_minus_and_plus_lines() {
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "old\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "new\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render_with(&plan, true, true);
    assert!(out.contains(CYAN), "hunk header should be cyan: {out:?}");
    assert!(out.contains(RED), "removed line should be red: {out:?}");
    assert!(out.contains(GREEN), "added line should be green: {out:?}");
}

#[test]
fn verbose_mode_sanitizes_inline_control_chars() {
    // Inside a single rendered line, control bytes must not pass
    // through (a hostile content could otherwise embed `\x1b[2J`
    // to clear the screen between hunks). Real `\n` between
    // lines is fine — that's the diff's natural line break.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "hello\x1b[2Jworld\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render_verbose(&plan);
    assert!(!out.contains('\x1b'), "ESC leaked: {out:?}");
    assert!(out.contains("\\u{001b}"), "expected ESC escape: {out:?}");
}

#[test]
fn plan_has_body_blocks_predicate_matches_renderer_output() {
    // The renderer uses this predicate to gate the footer hint
    // pointing at `--verbose-will-reveal-sensitive-content`; it
    // must agree with whether the renderer actually emits a body
    // summary. Pinning this match means a future change to one
    // path must also update the
    // other.
    let template_create = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "hi".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    assert!(plan_has_body_blocks(&template_create));

    let package_only = Plan {
        changes: vec![ResourceChange {
            address: "brew:ripgrep".into(),
            kind: ResourceKind::Package,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "ripgrep".into(),
                tap: None,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    assert!(!plan_has_body_blocks(&package_only));

    // Template update with equal content: no body block.
    let template_equal_update = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "same".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "same".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    assert!(!plan_has_body_blocks(&template_equal_update));

    // NoOp Template doesn't qualify either.
    let template_noop = Plan {
        changes: vec![ResourceChange {
            address: "/x".into(),
            kind: ResourceKind::Template,
            action: Action::NoOp,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "same".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/x"),
                content: "same".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    assert!(!plan_has_body_blocks(&template_noop));
}

#[test]
fn plan_has_body_blocks_recognises_ssh_key_create() {
    // Pins the SshKey arm of the per-state body lookup. Catches the
    // `delete match arm` mutation that would force every SshKey
    // resource into the `_ => None` fallback, suppressing the
    // verbose-banner gate and the body-body summary footer hint.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "ssh:id_keron".into(),
            kind: ResourceKind::SshKey,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::SshKey {
                private_path: PathBuf::from("/keys/id_keron"),
                public_path: PathBuf::from("/keys/id_keron.pub"),
                private_key: "-----BEGIN OPENSSH PRIVATE KEY-----\n…\n".into(),
                public_key: "ssh-ed25519 AAAA…".into(),
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    assert!(plan_has_body_blocks(&plan));
}

#[test]
fn plan_has_body_blocks_recognises_gpg_key_create() {
    // Pins the GpgKey arm of the per-state body lookup. Same
    // rationale as the SshKey companion test.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "gpg:ABCD1234".into(),
            kind: ResourceKind::GpgKey,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::GpgKey {
                fingerprint: "ABCD1234".into(),
                key: "-----BEGIN PGP PRIVATE KEY BLOCK-----\n…\n".into(),
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    assert!(plan_has_body_blocks(&plan));
}

#[test]
fn elevated_create_renders_under_elevated_header_and_not_unprivileged() {
    // Pins the `is_elevated` predicate and the renderer's
    // partitioning. A change marked `requires_elevation: true` with
    // a non-NoOp action MUST land in the "require elevated rights"
    // section AND NOT in the unprivileged "will perform" section.
    // Catches both `&& -> ||` and `delete !` mutations on the
    // `is_elevated` / `has_unprivileged_actions` predicates inside
    // render_plan.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/etc/x".into(),
            kind: ResourceKind::Template,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Template {
                path: PathBuf::from("/etc/x"),
                content: "hi".into(),
                sensitive: false,
            }),
            requires_elevation: true,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("The following changes require elevated rights:"),
        "elevated section header missing: {out}",
    );
    assert!(
        !out.contains("keron will perform the following actions:"),
        "elevated-only plan must NOT emit the unprivileged header: {out}",
    );
}

#[test]
fn elevated_noop_lists_under_unprivileged_section() {
    // The `!matches!(c.action, Action::NoOp)` carve-out inside
    // `is_elevated` exists so that an elevated path that ended up
    // NoOp doesn't disappear behind the elevated-only section.
    // Pin that behavior: an elevated NoOp resource is reported
    // alongside the rest of the unchanged roster, NOT under the
    // "require elevated rights" header.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/etc/x".into(),
            kind: ResourceKind::Template,
            action: Action::NoOp,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/etc/x"),
                content: "same".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/etc/x"),
                content: "same".into(),
                sensitive: false,
            }),
            requires_elevation: true,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("is up to date"),
        "elevated NoOp must still surface as up-to-date: {out}",
    );
    assert!(
        !out.contains("require elevated rights"),
        "elevated NoOp must NOT trigger the elevated section header: {out}",
    );
}

#[test]
fn render_body_verbose_drops_synthetic_unified_diff_headers() {
    // The unified-diff renderer adds `--- file` / `+++ file`
    // header lines when no `header(...)` is supplied to
    // `similar`. The diff body-renderer drops both lines because
    // the resource block already labels the field; pinning that
    // means an `|| -> &&` mutation would let the `--- ` and `+++ `
    // headers leak through as content.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "/etc/x".into(),
            kind: ResourceKind::Template,
            action: Action::Update,
            before: Some(ResourceState::Template {
                path: PathBuf::from("/etc/x"),
                content: "before\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Template {
                path: PathBuf::from("/etc/x"),
                content: "after\n".into(),
                sensitive: false,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render_verbose(&plan);
    assert!(
        !out.contains("--- "),
        "synthetic `--- file` header must be filtered: {out}",
    );
    assert!(
        !out.contains("+++ "),
        "synthetic `+++ file` header must be filtered: {out}",
    );
}

#[test]
fn tap_update_diff_renders_only_when_url_changes() {
    // Pin the `before_spec.url != after_spec.url` gate inside the
    // Tap arm of render_diff_lines. With a URL drift, the renderer
    // emits the `url = "<old>" -> "<new>"` line. The `!= with ==`
    // mutation would invert the gate and refuse to render when the
    // URL actually changed (renders the line when URLs are equal,
    // which would never produce useful output).
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "tap.icepuma/keron".into(),
            kind: ResourceKind::Tap,
            action: Action::Update,
            before: Some(ResourceState::Tap(crate::plan::TapSpec {
                user_tap: "icepuma/keron".into(),
                url: Some("https://github.com/icepuma/keron".into()),
            })),
            after: Some(ResourceState::Tap(crate::plan::TapSpec {
                user_tap: "icepuma/keron".into(),
                url: Some("https://github.com/forked/keron".into()),
            })),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("github.com/icepuma/keron") && out.contains("github.com/forked/keron"),
        "tap URL drift must render both URLs: {out}",
    );
}

#[test]
fn shell_update_diff_marks_sensitive_when_either_side_is_sensitive() {
    // Pin the `*before_sensitive || *after_sensitive` arm inside
    // render_diff_lines's Shell branch. A `|| with &&` mutation
    // would only mark the diff sensitive when BOTH sides were
    // sensitive — a one-sided sensitivity change (the common case
    // when a secret is newly introduced) would render plain.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "shell.refresh".into(),
            kind: ResourceKind::Shell,
            action: Action::Update,
            before: Some(ResourceState::Shell {
                kind: ShellKind::Sh,
                name: "refresh".into(),
                cwd: PathBuf::from("/tmp"),
                script: "echo old\n".into(),
                sensitive: false,
            }),
            after: Some(ResourceState::Shell {
                kind: ShellKind::Sh,
                name: "refresh".into(),
                cwd: PathBuf::from("/tmp"),
                script: "echo TOKEN=secret\n".into(),
                sensitive: true,
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("[sensitive]"),
        "one-sided sensitivity must still propagate to the diff: {out}",
    );
}

#[test]
fn package_diff_renders_tap_drift_when_tap_changes() {
    // Pin the `before.tap != after.tap` gate inside
    // render_package_diff. A `!= with ==` mutation would emit the
    // tap diff only when the taps were already equal (useless
    // output), and would silently drop the diff when the tap
    // actually changes — masking a relevant rename from the user.
    let plan = Plan {
        changes: vec![ResourceChange {
            address: "brew:keron".into(),
            kind: ResourceKind::Package,
            action: Action::Update,
            before: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "keron".into(),
                tap: Some(crate::plan::TapSpec {
                    user_tap: "icepuma/keron".into(),
                    url: None,
                }),
            }),
            after: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "keron".into(),
                tap: Some(crate::plan::TapSpec {
                    user_tap: "forked/keron".into(),
                    url: None,
                }),
            }),
            requires_elevation: false,
            requires_force: false,
        }],
    };
    let out = render(&plan);
    assert!(
        out.contains("tap"),
        "tap drift must render a tap line: {out}",
    );
    assert!(
        out.contains("icepuma/keron") && out.contains("forked/keron"),
        "tap drift must include both old and new tap names: {out}",
    );
}
