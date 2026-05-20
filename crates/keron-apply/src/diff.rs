//! Render a `Plan` as an OpenTofu-style diff. Symbols and colors
//! follow the well-worn convention: `+` create (green), `~` update
//! (yellow), `#` header (dim).
//!
//! ANSI escape codes are emitted inline rather than pulled in via a
//! crate to keep the dep surface small. Color is opt-in per call —
//! the caller decides based on `IsTerminal`.
//!
//! Body fields (template `content`, shell `script`) hide by default
//! and render only a `lines added / lines removed` summary. The
//! caller opts in to full unified-diff bodies via
//! `RenderOptions::verbose`; the CLI exposes that as the
//! `--verbose-will-reveal-sensitive-content` flag. The flag name *is*
//! the warning — verbose mode does not redact, even values produced
//! by inputs marked `sensitive`.

use std::io::{self, Write};

use crate::plan::{Action, Plan, ResourceChange, ResourceState};
use crate::terminal_safe::{escape_inline, sanitize_terminal_message, show_path, show_str};

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";

/// Hint shown in the default-mode body summary so the user can find
/// the opt-in. Kept verbose on purpose: the flag name itself is the
/// consent that subsequent verbose-mode output may print secrets to
/// the terminal, screen-shares, CI logs, etc.
const VERBOSE_HINT: &str = "use --verbose-will-reveal-sensitive-content to see";

/// One-line warning emitted at the top of verbose output. Verbose
/// mode prints body content verbatim — including the values of
/// inputs marked `sensitive`. The banner makes that explicit so a
/// `--verbose-will-reveal-sensitive-content` typed reflexively still
/// surfaces a fresh reminder before the actual content scrolls.
const VERBOSE_BANNER: &str =
    "! Verbose mode: full content diffs follow. Sensitive values are not redacted.";

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub color: bool,
    /// When true, template `content` and shell `script` render as a
    /// full unified diff inside their resource block. When false, the
    /// body is replaced by a one-line `N lines added / M lines
    /// removed` summary with a hint pointing at the opt-in flag.
    pub verbose: bool,
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

    if opts.verbose && plan_has_body_blocks(plan) {
        writeln!(out, "{}", paint(opts.color, RED, VERBOSE_BANNER))?;
        writeln!(out)?;
    }

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

/// Whether the plan would emit at least one body-shaped field
/// (template `content` or shell `script`) in default mode. Used by:
///   - the verbose banner emission (no banner if nothing's going to
///     reveal anything anyway), and
///   - the CLI's interactive prompt — there's no point asking "show
///     full content?" when there's no content to show.
pub fn plan_has_body_blocks(plan: &Plan) -> bool {
    plan.changes.iter().any(|c| {
        if matches!(c.action, Action::NoOp) {
            return false;
        }
        let body_of = |s: &Option<ResourceState>| -> Option<String> {
            s.as_ref().and_then(|state| match state {
                ResourceState::Template { content, .. } => Some(content.clone()),
                ResourceState::Shell { script, .. } => Some(script.clone()),
                _ => None,
            })
        };
        match (body_of(&c.before), body_of(&c.after)) {
            (None, None) => false,
            (None, Some(_)) | (Some(_), None) => true,
            (Some(b), Some(a)) => b != a,
        }
    })
}

/// Render a body-shaped field — template `content` or shell `script`.
///
/// `before = None` means a one-sided action (Create / Run); the
/// helper counts the lines in `after` and emits one summary line, or
/// a one-sided unified diff (all `+` / `>` lines) if verbose.
///
/// `before = Some(...)` and `before == after` suppresses output
/// (matches the existing "only render changed fields" semantic). When
/// they differ, default mode emits `~ field: N lines removed, M
/// lines added (use --verbose-will-reveal-sensitive-content to see
/// diff)` and verbose mode emits the full unified diff.
///
/// `sensitive` controls a `[sensitive]` hint that prefixes the
/// summary in default mode. It is *informational only* — the body
/// is hidden by default regardless of the flag, and verbose mode
/// reveals it regardless. The hint exists so an operator scanning
/// the plan can tell that a particular body field is going to print
/// secrets if they opt in to verbose.
fn render_body_field<W: Write>(
    out: &mut W,
    field: &str,
    before: Option<&str>,
    after: &str,
    sign: &str,
    sensitive: bool,
    opts: RenderOptions,
) -> io::Result<()> {
    if let Some(b) = before
        && b == after
    {
        return Ok(());
    }
    if opts.verbose {
        render_body_verbose(out, field, before, after, sign, opts)
    } else {
        render_body_summary(out, field, before, after, sign, sensitive, opts)
    }
}

fn render_body_summary<W: Write>(
    out: &mut W,
    field: &str,
    before: Option<&str>,
    after: &str,
    sign: &str,
    sensitive: bool,
    opts: RenderOptions,
) -> io::Result<()> {
    // `[sensitive]` painted red so it draws the eye next to the
    // otherwise-yellow `~` / green `+` / yellow `>` action sign.
    // No color → plain `[sensitive]` text token.
    let marker = if sensitive {
        format!(" {}", paint(opts.color, RED, "[sensitive]"))
    } else {
        String::new()
    };
    if let Some(b) = before {
        let diff = similar::TextDiff::from_lines(b, after);
        let mut added = 0usize;
        let mut removed = 0usize;
        for change in diff.iter_all_changes() {
            match change.tag() {
                similar::ChangeTag::Insert => added += 1,
                similar::ChangeTag::Delete => removed += 1,
                similar::ChangeTag::Equal => {}
            }
        }
        writeln!(
            out,
            "      {sign} {field}{marker}: {removed} {line_removed} removed, {added} {line_added} added ({VERBOSE_HINT} diff)",
            line_removed = if removed == 1 { "line" } else { "lines" },
            line_added = if added == 1 { "line" } else { "lines" },
        )?;
    } else {
        let n = count_lines(after);
        writeln!(
            out,
            "      {sign} {field}{marker}: {n} {word} ({VERBOSE_HINT})",
            word = if n == 1 { "line" } else { "lines" },
        )?;
    }
    Ok(())
}

/// Render a body field as a unified diff block, indented inside the
/// resource body. Synthetic `---` / `+++` file-header lines are
/// dropped — they carry no useful info here. Real `\n` / `\t` pass
/// through; `\r` / ANSI / bidi controls inside any individual line
/// are escaped via `sanitize_terminal_message` so a hostile content
/// byte cannot redraw the rendered diff.
///
/// Verbose mode is opt-in via `--verbose-will-reveal-sensitive-content`
/// — no redaction here by design. See `render_plan`'s banner.
fn render_body_verbose<W: Write>(
    out: &mut W,
    field: &str,
    before: Option<&str>,
    after: &str,
    sign: &str,
    opts: RenderOptions,
) -> io::Result<()> {
    writeln!(out, "      {sign} {field}:")?;
    let before_text = before.unwrap_or("");
    let diff = similar::TextDiff::from_lines(before_text, after);
    let rendered = diff.unified_diff().context_radius(3).to_string();
    let safe = sanitize_terminal_message(&rendered);
    for line in safe.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n');
        // The two synthetic header lines (`--- file` / `+++ file`)
        // are placeholders since `header(...)` wasn't supplied; drop
        // them — the resource block already labels the field.
        if trimmed.starts_with("--- ") || trimmed.starts_with("+++ ") {
            continue;
        }
        let painted = if trimmed.starts_with("@@") {
            paint(opts.color, CYAN, trimmed)
        } else if trimmed.starts_with('-') {
            paint(opts.color, RED, trimmed)
        } else if trimmed.starts_with('+') {
            paint(opts.color, GREEN, trimmed)
        } else {
            trimmed.to_string()
        };
        writeln!(out, "          {painted}")?;
    }
    Ok(())
}

/// Line counter for one-sided body summaries: each newline counts a
/// line, plus a final trailing partial line if the content does not
/// end with `\n`. `""` → 0, `"a"` → 1, `"a\n"` → 1, `"a\nb"` → 2,
/// `"a\nb\n"` → 2. Matches what a reader counts when scanning the
/// file by eye.
fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let mut n = s.matches('\n').count();
    if !s.ends_with('\n') {
        n += 1;
    }
    n
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
            // The resource-level `sensitive` flag drives two things:
            // the executor's file-mode choice (0o600 vs 0o644) and
            // the `[sensitive]` hint emitted by the default-mode body
            // summary. It does NOT redact — verbose mode reveals the
            // content regardless.
            sensitive,
        } => {
            writeln!(out, "      {s} target  = \"{}\"", show_path(path))?;
            render_body_field(out, "content", None, content, &s, *sensitive, opts)?;
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
            render_body_field(out, "script", None, script, &s, *sensitive, opts)?;
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
            };
            let after = TemplateRenderState {
                path: after_path,
                content: after_content,
            };
            // Conservative: any side flagged sensitive → show the
            // hint. Matches the "if either was a secret, treat both
            // as secret-bearing for UI purposes" rule.
            let sensitive = *before_sensitive || *after_sensitive;
            render_template_diff(out, before, after, &s, sensitive, opts)?;
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
        (
            ResourceState::Shell {
                sensitive: before_sensitive,
                ..
            },
            ResourceState::Shell {
                sensitive: after_sensitive,
                ..
            },
        ) => {
            let sensitive = *before_sensitive || *after_sensitive;
            render_shell_resource_diff(out, before, after, &s, sensitive, opts)?;
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
}

fn render_template_diff<W: Write>(
    out: &mut W,
    before: TemplateRenderState<'_>,
    after: TemplateRenderState<'_>,
    s: &str,
    sensitive: bool,
    opts: RenderOptions,
) -> io::Result<()> {
    if before.path != after.path {
        writeln!(
            out,
            "      {s} target  = \"{}\" -> \"{}\"",
            show_path(before.path),
            show_path(after.path)
        )?;
    }
    render_body_field(
        out,
        "content",
        Some(before.content),
        after.content,
        s,
        sensitive,
        opts,
    )?;
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
    sensitive: bool,
    opts: RenderOptions,
) -> io::Result<()> {
    let ResourceState::Shell {
        kind: before_kind,
        name: before_name,
        cwd: before_cwd,
        script: before_script,
        sensitive: _,
    } = before
    else {
        unreachable!("caller already matched Shell before-state");
    };
    let ResourceState::Shell {
        kind: after_kind,
        name: after_name,
        cwd: after_cwd,
        script: after_script,
        sensitive: _,
    } = after
    else {
        unreachable!("caller already matched Shell after-state");
    };
    let before = ShellRenderState {
        kind: *before_kind,
        name: before_name,
        cwd: before_cwd,
        script: before_script,
    };
    let after = ShellRenderState {
        kind: *after_kind,
        name: after_name,
        cwd: after_cwd,
        script: after_script,
    };
    render_shell_diff(out, before, after, s, sensitive, opts)
}

fn render_shell_diff<W: Write>(
    out: &mut W,
    before: ShellRenderState<'_>,
    after: ShellRenderState<'_>,
    s: &str,
    sensitive: bool,
    opts: RenderOptions,
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
    render_body_field(
        out,
        "script",
        Some(before.script),
        after.script,
        s,
        sensitive,
        opts,
    )?;
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
        assert!(
            out.contains("--verbose-will-reveal-sensitive-content"),
            "missing opt-in hint: {out}",
        );
        // The literal content must not leak.
        assert!(!out.contains("\"hi\""), "raw content leaked: {out}");
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
    fn default_mode_shell_script_sensitive_attaches_marker() {
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
            out.contains("> script [sensitive]: 2 lines"),
            "missing sensitive shell summary: {out}",
        );
        assert!(!out.contains("TOKEN=abc"), "script content leaked: {out}");
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
    fn update_shell_renders_non_body_fields_inline_and_script_as_summary() {
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
            out.contains("~ script: 1 line removed, 1 line added"),
            "missing script summary: {out}",
        );
        assert!(!out.contains("echo old"), "old script leaked: {out}");
        assert!(!out.contains("echo new"), "new script leaked: {out}");
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
    fn run_shell_renders_non_body_fields_inline_and_script_as_summary() {
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
            out.contains("> script: 1 line"),
            "missing script summary: {out}",
        );
        assert!(!out.contains("echo ok"), "script content leaked: {out}");
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
        assert!(
            out.contains("\\u{001b}"),
            "expected ESC escape: {out:?}",
        );
    }

    #[test]
    fn plan_has_body_blocks_predicate_matches_renderer_output() {
        // The CLI uses this predicate to gate the interactive
        // verbose-reveal prompt; it must agree with whether the
        // renderer actually emits a body summary. Pinning this match
        // means a future change to one path must also update the
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
}
