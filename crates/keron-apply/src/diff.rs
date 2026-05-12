//! Render a `Plan` as an OpenTofu-style diff. Symbols and colors
//! follow the well-worn convention: `+` create (green), `~` update
//! (yellow), `#` header (dim).
//!
//! ANSI escape codes are emitted inline rather than pulled in via a
//! crate to keep the dep surface small. Color is opt-in per call —
//! the caller decides based on `IsTerminal`.

use std::io::{self, Write};

use crate::plan::{Action, Plan, ResourceChange, ResourceState};
use crate::terminal_safe::{escape_inline, show_path, show_str};

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub color: bool,
}

pub fn render_plan<W: Write>(out: &mut W, plan: &Plan, opts: RenderOptions) -> io::Result<()> {
    if plan.is_empty() {
        writeln!(
            out,
            "No changes. Your infrastructure matches the configuration."
        )?;
        return Ok(());
    }

    let has_unprivileged = plan
        .changes
        .iter()
        .any(|c| !matches!(c.action, Action::NoOp) && !c.requires_elevation);
    let has_elevated = plan
        .changes
        .iter()
        .any(|c| !matches!(c.action, Action::NoOp) && c.requires_elevation);

    if has_unprivileged {
        writeln!(out, "keron will perform the following actions:")?;
        writeln!(out)?;
        for change in &plan.changes {
            if matches!(change.action, Action::NoOp) || change.requires_elevation {
                continue;
            }
            render_change(out, change, opts)?;
        }
    }

    if has_elevated {
        writeln!(out, "The following changes require elevated rights:")?;
        writeln!(out)?;
        for change in &plan.changes {
            if matches!(change.action, Action::NoOp) || !change.requires_elevation {
                continue;
            }
            render_change(out, change, opts)?;
        }
    }

    let s = plan.summary();
    match (s.elevated, s.force) {
        (0, 0) => writeln!(out, "Plan: {} to add, {} to change.", s.add, s.change)?,
        (elevated, 0) => writeln!(
            out,
            "Plan: {} to add, {} to change ({} elevated).",
            s.add, s.change, elevated,
        )?,
        (0, force) => writeln!(
            out,
            "Plan: {} to add, {} to change ({} force).",
            s.add, s.change, force,
        )?,
        (elevated, force) => writeln!(
            out,
            "Plan: {} to add, {} to change ({} elevated) ({} force).",
            s.add, s.change, elevated, force,
        )?,
    }
    Ok(())
}

fn render_change<W: Write>(
    out: &mut W,
    change: &ResourceChange,
    opts: RenderOptions,
) -> io::Result<()> {
    let verb = match change.action {
        Action::Create => "will be created",
        Action::Update => "will be updated in-place",
        Action::NoOp => return Ok(()),
    };
    let symbol = action_symbol(change.action);
    let color = action_color(change.action);
    let tag = match (change.requires_elevation, change.requires_force) {
        (true, true) => "  (elevated, force required)",
        (true, false) => "  (elevated)",
        (false, true) => "  (force required)",
        (false, false) => "",
    };

    writeln!(
        out,
        "  {hash} {kind}.\"{addr}\" {verb}{tag}",
        hash = paint(opts.color, DIM, "#"),
        kind = change.kind.label(),
        addr = show_str(&change.address),
    )?;
    writeln!(
        out,
        "  {sym} resource \"{kind}\" \"{addr}\" {{",
        sym = paint(opts.color, color, symbol),
        kind = change.kind.label(),
        addr = show_str(&change.address),
    )?;

    render_body(out, change, opts)?;

    writeln!(out, "    }}")?;
    writeln!(out)?;
    Ok(())
}

fn render_body<W: Write>(
    out: &mut W,
    change: &ResourceChange,
    opts: RenderOptions,
) -> io::Result<()> {
    match change.action {
        Action::Create => {
            if let Some(state) = &change.after {
                render_state_lines(out, state, "+", GREEN, opts)?;
            }
        }
        Action::Update => {
            if let (Some(before), Some(after)) = (&change.before, &change.after) {
                render_diff_lines(out, before, after, opts)?;
            }
        }
        Action::NoOp => {}
    }
    Ok(())
}

fn render_state_lines<W: Write>(
    out: &mut W,
    state: &ResourceState,
    sign: &str,
    color: &str,
    opts: RenderOptions,
) -> io::Result<()> {
    let s = paint(opts.color, color, sign);
    match state {
        ResourceState::Template {
            path,
            content,
            sensitive,
        } => {
            writeln!(out, "      {s} path    = \"{}\"", show_path(path))?;
            if *sensitive {
                writeln!(out, "      {s} content = <sensitive>")?;
            } else {
                writeln!(out, "      {s} content = \"{}\"", escape_inline(content))?;
            }
        }
        ResourceState::Symlink { from, to } => {
            writeln!(out, "      {s} from = \"{}\"", show_path(from))?;
            writeln!(out, "      {s} to   = \"{}\"", show_path(to))?;
        }
        ResourceState::Package { manager, name } => {
            writeln!(out, "      {s} manager = \"{}\"", manager.label())?;
            writeln!(out, "      {s} name    = \"{}\"", escape_inline(name))?;
        }
    }
    Ok(())
}

fn render_diff_lines<W: Write>(
    out: &mut W,
    before: &ResourceState,
    after: &ResourceState,
    opts: RenderOptions,
) -> io::Result<()> {
    let s = paint(opts.color, YELLOW, "~");
    match (before, after) {
        (
            ResourceState::Template {
                path: bp,
                content: bc,
                sensitive: bs,
            },
            ResourceState::Template {
                path: ap,
                content: ac,
                sensitive: as_,
            },
        ) => {
            if bp != ap {
                writeln!(
                    out,
                    "      {s} path    = \"{}\" -> \"{}\"",
                    show_path(bp),
                    show_path(ap)
                )?;
            }
            if *bs || *as_ {
                if bc != ac {
                    writeln!(out, "      {s} content = <sensitive> -> <sensitive>")?;
                }
            } else if bc != ac {
                writeln!(
                    out,
                    "      {s} content = \"{}\" -> \"{}\"",
                    escape_inline(bc),
                    escape_inline(ac)
                )?;
            }
        }
        (
            ResourceState::Symlink { from: bf, to: bt },
            ResourceState::Symlink { from: af, to: at },
        ) => {
            if bf != af {
                writeln!(
                    out,
                    "      {s} from = \"{}\" -> \"{}\"",
                    show_path(bf),
                    show_path(af)
                )?;
            }
            if bt != at {
                writeln!(
                    out,
                    "      {s} to   = \"{}\" -> \"{}\"",
                    show_path(bt),
                    show_path(at)
                )?;
            }
        }
        (
            ResourceState::Package {
                manager: bm,
                name: bn,
            },
            ResourceState::Package {
                manager: am,
                name: an,
            },
        ) => {
            if bm != am {
                writeln!(
                    out,
                    "      {s} manager = \"{}\" -> \"{}\"",
                    bm.label(),
                    am.label()
                )?;
            }
            if bn != an {
                writeln!(
                    out,
                    "      {s} name    = \"{}\" -> \"{}\"",
                    escape_inline(bn),
                    escape_inline(an)
                )?;
            }
        }
        _ => {
            render_state_lines(out, before, "-", RED, opts)?;
            render_state_lines(out, after, "+", GREEN, opts)?;
        }
    }
    Ok(())
}

fn paint(color: bool, code: &str, s: &str) -> String {
    if color {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

const fn action_symbol(action: Action) -> &'static str {
    match action {
        Action::Create => "+",
        Action::Update => "~",
        Action::NoOp => " ",
    }
}

const fn action_color(action: Action) -> &'static str {
    match action {
        Action::Create => GREEN,
        Action::Update => YELLOW,
        Action::NoOp => RESET,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PackageManager, ResourceKind};
    use std::path::PathBuf;

    fn render(plan: &Plan) -> String {
        let mut buf = Vec::new();
        render_plan(&mut buf, plan, RenderOptions { color: false }).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn empty_plan_message() {
        let out = render(&Plan::default());
        assert!(out.contains("No changes"), "got: {out}");
    }

    #[test]
    fn sample_renders_each_action() {
        let out = render(&Plan::sample());
        assert!(out.contains("template.\"~/.zshrc\" will be created"));
        assert!(out.contains("symlink.\"~/.config/nvim\" will be updated in-place"));
        assert!(out.contains("Plan: 1 to add, 1 to change."));
        assert!(out.contains("+ resource \"template\""));
        assert!(out.contains("~ resource \"symlink\""));
        assert!(out.contains("~ to   = \"/old/target\" -> \"/new/target\""));
    }

    #[test]
    fn no_color_means_no_escape_codes() {
        let out = render(&Plan::sample());
        assert!(!out.contains('\x1b'), "ANSI escape leaked: {out}");
    }

    #[test]
    fn color_path_includes_escape_codes() {
        let mut buf = Vec::new();
        render_plan(&mut buf, &Plan::sample(), RenderOptions { color: true }).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains('\x1b'));
    }

    #[test]
    fn create_renders_path_and_content_lines() {
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
            out.contains("+ path    = \"/etc/x\""),
            "missing path line: {out}"
        );
        assert!(
            out.contains("+ content = \"hi\""),
            "missing content line: {out}"
        );
    }

    #[test]
    fn create_symlink_renders_from_and_to_lines() {
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
        assert!(out.contains("+ from = \"/a\""), "missing from line: {out}");
        assert!(out.contains("+ to   = \"/b\""), "missing to line: {out}");
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
        assert!(out.contains("~ content = \"old\" -> \"new\""), "got: {out}");
        assert!(!out.contains("~ path"), "path should be unchanged: {out}");
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
            out.contains("~ path    = \"/old\" -> \"/new\""),
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
        assert!(out.contains("~ to   = \"/t1\" -> \"/t2\""), "got: {out}");
        assert!(!out.contains("~ from"), "from should be unchanged: {out}");
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
            out.contains("- path    = \"/x\""),
            "missing before half: {out}"
        );
        assert!(
            out.contains("+ from = \"/x\""),
            "missing create half: {out}"
        );
    }

    #[test]
    fn action_color_uses_distinct_codes() {
        assert_eq!(action_color(Action::Create), GREEN);
        assert_eq!(action_color(Action::Update), YELLOW);
        assert_eq!(action_color(Action::NoOp), RESET);
    }

    #[test]
    fn color_output_uses_action_specific_code_for_each_action() {
        let mut buf = Vec::new();
        render_plan(&mut buf, &Plan::sample(), RenderOptions { color: true }).unwrap();
        let out = String::from_utf8(buf).unwrap();
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
    fn create_with_special_chars_in_content_escapes_them() {
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
        assert!(out.contains("\\t"), "tab not escaped: {out}");
        assert!(out.contains("\\n"), "newline not escaped: {out}");
    }

    #[test]
    fn sensitive_template_content_is_redacted() {
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
        assert!(out.contains("+ content = <sensitive>"), "got: {out}");
        assert!(
            out.contains("~ content = <sensitive> -> <sensitive>"),
            "got: {out}"
        );
        assert!(
            out.contains("template.\"/y\" will be updated in-place  (force required)"),
            "got: {out}"
        );
        assert!(!out.contains("secret"), "secret leaked in diff: {out}");
    }

    #[test]
    fn template_update_redacts_when_only_after_is_sensitive() {
        let plan = Plan {
            changes: vec![ResourceChange {
                address: "/x".into(),
                kind: ResourceKind::Template,
                action: Action::Update,
                before: Some(ResourceState::Template {
                    path: PathBuf::from("/x"),
                    content: "old-public".into(),
                    sensitive: false,
                }),
                after: Some(ResourceState::Template {
                    path: PathBuf::from("/x"),
                    content: "new-secret".into(),
                    sensitive: true,
                }),
                requires_elevation: false,
                requires_force: true,
            }],
        };
        let out = render(&plan);
        assert!(
            out.contains("~ content = <sensitive> -> <sensitive>"),
            "got: {out}"
        );
        assert!(!out.contains("old-public"), "old content leaked: {out}");
        assert!(!out.contains("new-secret"), "new content leaked: {out}");
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
                }),
                after: Some(ResourceState::Package {
                    manager: PackageManager::Brew,
                    name: "git@2".into(),
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
                }),
                after: Some(ResourceState::Package {
                    manager: PackageManager::Cargo,
                    name: "ripgrep".into(),
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
}
