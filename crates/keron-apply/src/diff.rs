//! Render a `Plan` as an OpenTofu-style diff. Symbols and colors
//! follow the well-worn convention: `+` create (green), `~` update
//! (yellow), `-` destroy (red), `#` header (dim).
//!
//! ANSI escape codes are emitted inline rather than pulled in via a
//! crate to keep the dep surface small. Color is opt-in per call —
//! the caller decides based on `IsTerminal`.

use std::io::{self, Write};

use crate::plan::{Action, Plan, ResourceChange, ResourceState};

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

    writeln!(out, "keron will perform the following actions:")?;
    writeln!(out)?;

    for change in &plan.changes {
        if matches!(change.action, Action::NoOp) {
            continue;
        }
        render_change(out, change, opts)?;
    }

    let s = plan.summary();
    writeln!(
        out,
        "Plan: {} to add, {} to change, {} to destroy.",
        s.add, s.change, s.destroy
    )?;
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
        Action::Destroy => "will be destroyed",
        Action::NoOp => return Ok(()),
    };
    let symbol = action_symbol(change.action);
    let color = action_color(change.action);

    writeln!(
        out,
        "  {hash} {kind}.\"{addr}\" {verb}",
        hash = paint(opts.color, DIM, "#"),
        kind = change.kind.label(),
        addr = change.address,
    )?;
    writeln!(
        out,
        "  {sym} resource \"{kind}\" \"{addr}\" {{",
        sym = paint(opts.color, color, symbol),
        kind = change.kind.label(),
        addr = change.address,
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
        Action::Destroy => {
            if let Some(state) = &change.before {
                render_state_lines(out, state, "-", RED, opts)?;
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
        ResourceState::Template { path, content } => {
            writeln!(out, "      {s} path    = \"{}\"", path.display())?;
            writeln!(out, "      {s} content = \"{}\"", escape_inline(content))?;
        }
        ResourceState::Directory { path } => {
            writeln!(out, "      {s} path = \"{}\"", path.display())?;
        }
        ResourceState::Symlink { from, to } => {
            writeln!(out, "      {s} from = \"{}\"", from.display())?;
            writeln!(out, "      {s} to   = \"{}\"", to.display())?;
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
            },
            ResourceState::Template {
                path: ap,
                content: ac,
            },
        ) => {
            if bp != ap {
                writeln!(
                    out,
                    "      {s} path    = \"{}\" -> \"{}\"",
                    bp.display(),
                    ap.display()
                )?;
            }
            if bc != ac {
                writeln!(
                    out,
                    "      {s} content = \"{}\" -> \"{}\"",
                    escape_inline(bc),
                    escape_inline(ac)
                )?;
            }
        }
        (ResourceState::Directory { path: bp }, ResourceState::Directory { path: ap }) => {
            if bp != ap {
                writeln!(
                    out,
                    "      {s} path = \"{}\" -> \"{}\"",
                    bp.display(),
                    ap.display()
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
                    bf.display(),
                    af.display()
                )?;
            }
            if bt != at {
                writeln!(
                    out,
                    "      {s} to   = \"{}\" -> \"{}\"",
                    bt.display(),
                    at.display()
                )?;
            }
        }
        _ => {
            // Heterogeneous before/after — render as full destroy + create.
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
        Action::Destroy => "-",
        Action::NoOp => " ",
    }
}

const fn action_color(action: Action) -> &'static str {
    match action {
        Action::Create => GREEN,
        Action::Update => YELLOW,
        Action::Destroy => RED,
        Action::NoOp => RESET,
    }
}

fn escape_inline(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::ResourceKind;
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
        assert!(out.contains("directory.\"/tmp/scratch\" will be destroyed"));
        assert!(out.contains("Plan: 1 to add, 1 to change, 1 to destroy."));
        assert!(out.contains("+ resource \"template\""));
        assert!(out.contains("~ resource \"symlink\""));
        assert!(out.contains("- resource \"directory\""));
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
        // Pin the body produced by `render_state_lines` for a Create
        // action — without each line, a `Ok(())` mutation would still
        // pass the action-header assertions in `sample_renders_each_action`.
        let plan = Plan {
            changes: vec![ResourceChange {
                address: "/etc/x".into(),
                kind: ResourceKind::Template,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Template {
                    path: PathBuf::from("/etc/x"),
                    content: "hi".into(),
                }),
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
    fn create_directory_renders_single_path_line() {
        let plan = Plan {
            changes: vec![ResourceChange {
                address: "/d".into(),
                kind: ResourceKind::Directory,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Directory {
                    path: PathBuf::from("/d"),
                }),
            }],
        };
        let out = render(&plan);
        assert!(out.contains("+ path = \"/d\""), "missing path line: {out}");
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
            }],
        };
        let out = render(&plan);
        assert!(out.contains("+ from = \"/a\""), "missing from line: {out}");
        assert!(out.contains("+ to   = \"/b\""), "missing to line: {out}");
    }

    #[test]
    fn destroy_renders_with_minus_sign() {
        let plan = Plan {
            changes: vec![ResourceChange {
                address: "/d".into(),
                kind: ResourceKind::Directory,
                action: Action::Destroy,
                before: Some(ResourceState::Directory {
                    path: PathBuf::from("/d"),
                }),
                after: None,
            }],
        };
        let out = render(&plan);
        assert!(
            out.contains("- path = \"/d\""),
            "missing destroy body: {out}"
        );
    }

    #[test]
    fn update_file_renders_only_changed_fields() {
        // Path same, content different: only the `content` line
        // should appear. `!= → ==` would invert that.
        let plan = Plan {
            changes: vec![ResourceChange {
                address: "/x".into(),
                kind: ResourceKind::Template,
                action: Action::Update,
                before: Some(ResourceState::Template {
                    path: PathBuf::from("/x"),
                    content: "old".into(),
                }),
                after: Some(ResourceState::Template {
                    path: PathBuf::from("/x"),
                    content: "new".into(),
                }),
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
                }),
                after: Some(ResourceState::Template {
                    path: PathBuf::from("/new"),
                    content: "same".into(),
                }),
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
    fn update_directory_renders_path_change() {
        let plan = Plan {
            changes: vec![ResourceChange {
                address: "/old".into(),
                kind: ResourceKind::Directory,
                action: Action::Update,
                before: Some(ResourceState::Directory {
                    path: PathBuf::from("/old"),
                }),
                after: Some(ResourceState::Directory {
                    path: PathBuf::from("/new"),
                }),
            }],
        };
        let out = render(&plan);
        assert!(out.contains("~ path = \"/old\" -> \"/new\""), "got: {out}");
    }

    #[test]
    fn update_directory_with_unchanged_path_renders_no_path_line() {
        // When the path is identical, the `bp != ap` guard suppresses
        // the path line. `!= → ==` would emit a stale (or empty)
        // diff line.
        let plan = Plan {
            changes: vec![ResourceChange {
                address: "/d".into(),
                kind: ResourceKind::Directory,
                action: Action::Update,
                before: Some(ResourceState::Directory {
                    path: PathBuf::from("/d"),
                }),
                after: Some(ResourceState::Directory {
                    path: PathBuf::from("/d"),
                }),
            }],
        };
        let out = render(&plan);
        assert!(!out.contains("~ path"), "should be no path diff: {out}");
    }

    #[test]
    fn update_symlink_renders_only_changed_field() {
        // `from` same, `to` different — only the `to` line should appear.
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
            }],
        };
        let out = render(&plan);
        assert!(out.contains("~ to   = \"/t1\" -> \"/t2\""), "got: {out}");
        assert!(!out.contains("~ from"), "from should be unchanged: {out}");
    }

    #[test]
    fn update_kind_change_renders_destroy_then_create() {
        // Heterogeneous before/after falls through to the `_` arm and
        // emits both a `-` and a `+` block. Pins that fallback path.
        let plan = Plan {
            changes: vec![ResourceChange {
                address: "/x".into(),
                kind: ResourceKind::Template,
                action: Action::Update,
                before: Some(ResourceState::Template {
                    path: PathBuf::from("/x"),
                    content: "old".into(),
                }),
                after: Some(ResourceState::Directory {
                    path: PathBuf::from("/x"),
                }),
            }],
        };
        let out = render(&plan);
        assert!(
            out.contains("- path    = \"/x\""),
            "missing destroy half: {out}"
        );
        assert!(
            out.contains("+ path = \"/x\""),
            "missing create half: {out}"
        );
    }

    #[test]
    fn action_color_uses_distinct_codes() {
        assert_eq!(action_color(Action::Create), GREEN);
        assert_eq!(action_color(Action::Update), YELLOW);
        assert_eq!(action_color(Action::Destroy), RED);
        assert_eq!(action_color(Action::NoOp), RESET);
    }

    #[test]
    fn color_output_uses_action_specific_code_for_each_action() {
        // Construct a plan with one of each non-NoOp action, render
        // with color, and assert the expected ANSI code appears for
        // each action's symbol. Mutating `action_color` to "" would
        // strip the codes.
        let mut buf = Vec::new();
        render_plan(&mut buf, &Plan::sample(), RenderOptions { color: true }).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains(GREEN), "create should be green: {out}");
        assert!(out.contains(YELLOW), "update should be yellow: {out}");
        assert!(out.contains(RED), "destroy should be red: {out}");
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
    fn create_with_special_chars_in_content_escapes_them() {
        // End-to-end: render a File with a tab/newline content and
        // verify `escape_inline` actually fires in the diff output.
        let plan = Plan {
            changes: vec![ResourceChange {
                address: "/x".into(),
                kind: ResourceKind::Template,
                action: Action::Create,
                before: None,
                after: Some(ResourceState::Template {
                    path: PathBuf::from("/x"),
                    content: "a\tb\n".into(),
                }),
            }],
        };
        let out = render(&plan);
        assert!(out.contains("\\t"), "tab not escaped: {out}");
        assert!(out.contains("\\n"), "newline not escaped: {out}");
    }
}
