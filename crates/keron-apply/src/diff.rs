//! Render a `Plan` as an OpenTofu-style diff. Symbols and colors
//! follow the well-worn convention: `+` create (green), `~` update
//! (yellow), `-` destroy (red), `#` header (dim).
//!
//! ANSI escape codes are emitted inline rather than pulled in via a
//! crate to keep the dep surface small. Color is opt-in per call —
//! the caller decides based on `IsTerminal`.

#![allow(clippy::redundant_pub_crate)]

use std::io::{self, Write};

use crate::plan::{Action, Plan, ResourceChange, ResourceState};

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";

#[derive(Debug, Clone, Copy)]
pub(crate) struct RenderOptions {
    pub(crate) color: bool,
}

pub(crate) fn render_plan<W: Write>(
    out: &mut W,
    plan: &Plan,
    opts: RenderOptions,
) -> io::Result<()> {
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
        ResourceState::File { path, content } => {
            writeln!(out, "      {s} path    = \"{}\"", path.display())?;
            writeln!(
                out,
                "      {s} content = \"{}\"",
                escape_inline(content)
            )?;
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
            ResourceState::File {
                path: bp,
                content: bc,
            },
            ResourceState::File {
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
        (
            ResourceState::Directory { path: bp },
            ResourceState::Directory { path: ap },
        ) => {
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
        assert!(out.contains("file.\"~/.zshrc\" will be created"));
        assert!(out.contains("symlink.\"~/.config/nvim\" will be updated in-place"));
        assert!(out.contains("directory.\"/tmp/scratch\" will be destroyed"));
        assert!(out.contains("Plan: 1 to add, 1 to change, 1 to destroy."));
        assert!(out.contains("+ resource \"file\""));
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
}
