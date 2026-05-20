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
    // NoOps always render in the unprivileged group regardless of
    // their `requires_elevation` flag, matching the convention in
    // `Plan::partition_by_elevation` (no-op changes never reach the
    // elevated child process).
    let is_elevated =
        |c: &ResourceChange| c.requires_elevation && !matches!(c.action, Action::NoOp);
    let is_noop = |c: &ResourceChange| matches!(c.action, Action::NoOp);
    let has_unprivileged = plan.changes.iter().any(|c| !is_elevated(c));
    let has_unprivileged_actions = plan.changes.iter().any(|c| !is_elevated(c) && !is_noop(c));
    let has_elevated = plan.changes.iter().any(is_elevated);

    if has_unprivileged {
        // Suppress the "will perform" header when the unprivileged
        // group is all NoOps — nothing is actually being performed,
        // just listed.
        if has_unprivileged_actions {
            writeln!(out, "keron will perform the following actions:")?;
            writeln!(out)?;
        }
        for change in &plan.changes {
            if is_elevated(change) {
                continue;
            }
            render_change(out, change, opts)?;
        }
    }

    if has_elevated {
        writeln!(out, "The following changes require elevated rights:")?;
        writeln!(out)?;
        for change in &plan.changes {
            if !is_elevated(change) {
                continue;
            }
            render_change(out, change, opts)?;
        }
    }

    let s = plan.summary();
    if plan.is_empty() {
        if s.unchanged > 0 {
            let (noun, verb) = if s.unchanged == 1 {
                ("resource", "is")
            } else {
                ("resources", "are")
            };
            writeln!(out, "No changes. {} {noun} {verb} up to date.", s.unchanged)?;
        } else {
            writeln!(out, "No changes.")?;
        }
        return Ok(());
    }
    write!(
        out,
        "Plan: {} to add, {} to change, {} to run",
        s.add, s.change, s.run
    )?;
    if s.unchanged > 0 {
        write!(out, ", {} unchanged", s.unchanged)?;
    }
    if s.elevated > 0 {
        write!(out, " ({} elevated)", s.elevated)?;
    }
    if s.force > 0 {
        write!(out, " ({} force)", s.force)?;
    }
    writeln!(out, ".")?;
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
        Action::Run => "will run",
        Action::NoOp => "is up to date",
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

    // NoOps render as a single header line: the action verb already
    // conveys "no change" and a full empty `{}` block per resource
    // makes a long unchanged roster (e.g. 50 already-installed brews)
    // unreadable.
    if matches!(change.action, Action::NoOp) {
        writeln!(out)?;
        return Ok(());
    }

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
        Action::Run => {
            if let Some(state) = &change.after {
                render_state_lines(out, state, ">", YELLOW, opts)?;
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
            writeln!(out, "      {s} target  = \"{}\"", show_path(path))?;
            if *sensitive {
                writeln!(out, "      {s} content = <sensitive>")?;
            } else {
                writeln!(out, "      {s} content = \"{}\"", escape_inline(content))?;
            }
        }
        ResourceState::Symlink { from, to } => {
            writeln!(out, "      {s} source = \"{}\"", show_path(to))?;
            writeln!(out, "      {s} target = \"{}\"", show_path(from))?;
        }
        ResourceState::Package { manager, name } => {
            writeln!(out, "      {s} manager = \"{}\"", manager.label())?;
            writeln!(out, "      {s} name    = \"{}\"", escape_inline(name))?;
        }
        ResourceState::Shell {
            kind,
            name,
            cwd,
            script,
            sensitive,
        } => {
            writeln!(out, "      {s} kind   = \"{}\"", kind.label())?;
            writeln!(out, "      {s} name   = \"{}\"", escape_inline(name))?;
            writeln!(out, "      {s} cwd    = \"{}\"", show_path(cwd))?;
            if *sensitive {
                writeln!(out, "      {s} script = <sensitive>")?;
            } else {
                writeln!(out, "      {s} script = \"{}\"", escape_inline(script))?;
            }
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
                path: before_path,
                content: before_content,
                sensitive: before_sensitive,
            },
            ResourceState::Template {
                path: after_path,
                content: after_content,
                sensitive: after_sensitive,
            },
        ) => {
            let before = TemplateRenderState {
                path: before_path,
                content: before_content,
                sensitive: *before_sensitive,
            };
            let after = TemplateRenderState {
                path: after_path,
                content: after_content,
                sensitive: *after_sensitive,
            };
            render_template_diff(out, before, after, &s)?;
        }
        (
            ResourceState::Symlink {
                from: before_from,
                to: before_to,
            },
            ResourceState::Symlink {
                from: after_from,
                to: after_to,
            },
        ) => {
            let before = SymlinkRenderState {
                from: before_from,
                to: before_to,
            };
            let after = SymlinkRenderState {
                from: after_from,
                to: after_to,
            };
            render_symlink_diff(out, before, after, &s)?;
        }
        (
            ResourceState::Package {
                manager: before_manager,
                name: before_name,
            },
            ResourceState::Package {
                manager: after_manager,
                name: after_name,
            },
        ) => {
            let before = PackageRenderState {
                manager: *before_manager,
                name: before_name,
            };
            let after = PackageRenderState {
                manager: *after_manager,
                name: after_name,
            };
            render_package_diff(out, before, after, &s)?;
        }
        (ResourceState::Shell { .. }, ResourceState::Shell { .. }) => {
            render_shell_resource_diff(out, before, after, &s)?;
        }
        _ => {
            render_state_lines(out, before, "-", RED, opts)?;
            render_state_lines(out, after, "+", GREEN, opts)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct TemplateRenderState<'a> {
    path: &'a std::path::Path,
    content: &'a str,
    sensitive: bool,
}

#[derive(Debug, Clone, Copy)]
struct SymlinkRenderState<'a> {
    from: &'a std::path::Path,
    to: &'a std::path::Path,
}

#[derive(Debug, Clone, Copy)]
struct PackageRenderState<'a> {
    manager: crate::plan::PackageManager,
    name: &'a str,
}

#[derive(Debug, Clone, Copy)]
struct ShellRenderState<'a> {
    kind: crate::plan::ShellKind,
    name: &'a str,
    cwd: &'a std::path::Path,
    script: &'a str,
    sensitive: bool,
}

fn render_template_diff<W: Write>(
    out: &mut W,
    before: TemplateRenderState<'_>,
    after: TemplateRenderState<'_>,
    s: &str,
) -> io::Result<()> {
    if before.path != after.path {
        writeln!(
            out,
            "      {s} target  = \"{}\" -> \"{}\"",
            show_path(before.path),
            show_path(after.path)
        )?;
    }
    if before.sensitive || after.sensitive {
        if before.content != after.content {
            writeln!(out, "      {s} content = <sensitive> -> <sensitive>")?;
        }
    } else if before.content != after.content {
        writeln!(
            out,
            "      {s} content = \"{}\" -> \"{}\"",
            escape_inline(before.content),
            escape_inline(after.content)
        )?;
    }
    Ok(())
}

fn render_symlink_diff<W: Write>(
    out: &mut W,
    before: SymlinkRenderState<'_>,
    after: SymlinkRenderState<'_>,
    s: &str,
) -> io::Result<()> {
    if before.from != after.from {
        writeln!(
            out,
            "      {s} target = \"{}\" -> \"{}\"",
            show_path(before.from),
            show_path(after.from)
        )?;
    }
    if before.to != after.to {
        writeln!(
            out,
            "      {s} source = \"{}\" -> \"{}\"",
            show_path(before.to),
            show_path(after.to)
        )?;
    }
    Ok(())
}

fn render_package_diff<W: Write>(
    out: &mut W,
    before: PackageRenderState<'_>,
    after: PackageRenderState<'_>,
    s: &str,
) -> io::Result<()> {
    if before.manager != after.manager {
        writeln!(
            out,
            "      {s} manager = \"{}\" -> \"{}\"",
            before.manager.label(),
            after.manager.label()
        )?;
    }
    if before.name != after.name {
        writeln!(
            out,
            "      {s} name    = \"{}\" -> \"{}\"",
            escape_inline(before.name),
            escape_inline(after.name)
        )?;
    }
    Ok(())
}

fn render_shell_resource_diff<W: Write>(
    out: &mut W,
    before: &ResourceState,
    after: &ResourceState,
    s: &str,
) -> io::Result<()> {
    let ResourceState::Shell {
        kind: before_kind,
        name: before_name,
        cwd: before_cwd,
        script: before_script,
        sensitive: before_sensitive,
    } = before
    else {
        unreachable!("caller already matched Shell before-state");
    };
    let ResourceState::Shell {
        kind: after_kind,
        name: after_name,
        cwd: after_cwd,
        script: after_script,
        sensitive: after_sensitive,
    } = after
    else {
        unreachable!("caller already matched Shell after-state");
    };
    let before = ShellRenderState {
        kind: *before_kind,
        name: before_name,
        cwd: before_cwd,
        script: before_script,
        sensitive: *before_sensitive,
    };
    let after = ShellRenderState {
        kind: *after_kind,
        name: after_name,
        cwd: after_cwd,
        script: after_script,
        sensitive: *after_sensitive,
    };
    render_shell_diff(out, before, after, s)
}

fn render_shell_diff<W: Write>(
    out: &mut W,
    before: ShellRenderState<'_>,
    after: ShellRenderState<'_>,
    s: &str,
) -> io::Result<()> {
    if before.kind != after.kind {
        writeln!(
            out,
            "      {s} kind   = \"{}\" -> \"{}\"",
            before.kind.label(),
            after.kind.label()
        )?;
    }
    if before.name != after.name {
        writeln!(
            out,
            "      {s} name   = \"{}\" -> \"{}\"",
            escape_inline(before.name),
            escape_inline(after.name)
        )?;
    }
    if before.cwd != after.cwd {
        writeln!(
            out,
            "      {s} cwd    = \"{}\" -> \"{}\"",
            show_path(before.cwd),
            show_path(after.cwd)
        )?;
    }
    if before.sensitive || after.sensitive {
        if before.script != after.script {
            writeln!(out, "      {s} script = <sensitive> -> <sensitive>")?;
        }
    } else if before.script != after.script {
        writeln!(
            out,
            "      {s} script = \"{}\" -> \"{}\"",
            escape_inline(before.script),
            escape_inline(after.script)
        )?;
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
        Action::Run => ">",
        Action::NoOp => " ",
    }
}

const fn action_color(action: Action) -> &'static str {
    match action {
        Action::Create => GREEN,
        Action::Update | Action::Run => YELLOW,
        Action::NoOp => RESET,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PackageManager, ResourceKind, ShellKind};
    use std::path::PathBuf;

    fn render(plan: &Plan) -> String {
        let mut buf = Vec::new();
        render_plan(&mut buf, plan, RenderOptions { color: false }).unwrap();
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
                    }),
                    after: Some(ResourceState::Package {
                        manager: PackageManager::Brew,
                        name: "git".into(),
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
                    }),
                    after: Some(ResourceState::Package {
                        manager: PackageManager::Brew,
                        name: "fd".into(),
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
            }),
            after: Some(ResourceState::Package {
                manager: PackageManager::Brew,
                name: "git".into(),
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
        let mut buf = Vec::new();
        render_plan(&mut buf, &Plan::sample(), RenderOptions { color: true }).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains('\x1b'));
    }

    #[test]
    fn create_renders_target_and_content_lines() {
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
        assert!(
            out.contains("+ content = \"hi\""),
            "missing content line: {out}"
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
        assert!(out.contains("~ content = \"old\" -> \"new\""), "got: {out}");
        assert!(
            !out.contains("~ target"),
            "target should be unchanged: {out}"
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

    #[test]
    fn update_shell_renders_all_changed_fields() {
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
            out.contains("~ script = \"echo old\\n\" -> \"echo new\\n\""),
            "missing script diff: {out}"
        );
    }

    #[test]
    fn update_shell_redacts_when_only_after_script_is_sensitive() {
        let plan = Plan {
            changes: vec![ResourceChange {
                address: "refresh".into(),
                kind: ResourceKind::Shell,
                action: Action::Update,
                before: Some(ResourceState::Shell {
                    kind: ShellKind::Sh,
                    name: "refresh".into(),
                    cwd: PathBuf::from("/repo"),
                    script: "echo public\n".into(),
                    sensitive: false,
                }),
                after: Some(ResourceState::Shell {
                    kind: ShellKind::Sh,
                    name: "refresh".into(),
                    cwd: PathBuf::from("/repo"),
                    script: "TOKEN=secret\n".into(),
                    sensitive: true,
                }),
                requires_elevation: false,
                requires_force: false,
            }],
        };
        let out = render(&plan);
        assert!(
            out.contains("~ script = <sensitive> -> <sensitive>"),
            "got: {out}"
        );
        assert!(!out.contains("echo public"), "old script leaked: {out}");
        assert!(!out.contains("TOKEN=secret"), "new script leaked: {out}");
    }

    #[test]
    fn run_shell_renders_shell_fields() {
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
            out.contains("> script = \"echo ok\\n\""),
            "missing script: {out}"
        );
        assert!(out.contains("Plan: 0 to add, 0 to change, 1 to run."));
    }

    #[test]
    fn sensitive_shell_script_is_redacted() {
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
                    script: "TOKEN=secret".into(),
                    sensitive: true,
                }),
                requires_elevation: false,
                requires_force: false,
            }],
        };
        let out = render(&plan);
        assert!(out.contains("> script = <sensitive>"), "got: {out}");
        assert!(!out.contains("TOKEN=secret"), "secret leaked: {out}");
    }
}
