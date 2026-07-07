//! Render a `Plan` as an OpenTofu-style diff. Symbols and colors
//! follow the well-worn convention: `+` create (green), `~` update
//! (yellow), `#` header (dim).
//!
//! ANSI escape codes are emitted inline rather than pulled in via a
//! crate to keep the dep surface small. Color is opt-in per call —
//! the caller decides based on `IsTerminal`.
//!
//! Body fields that can contain secrets hide by default and render
//! only a `lines added / lines removed` summary. Shell scripts are
//! always shown because they are executable code the user must review
//! before approval. The caller opts in to other full unified-diff
//! bodies via
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

/// Single footer hint printed once at the bottom of a default-mode
/// plan that has at least one hidden body block. Kept verbose on
/// purpose: the flag name itself is the consent that subsequent
/// verbose-mode output may print secrets to the terminal,
/// screen-shares, CI logs, etc.
const VERBOSE_FOOTER_HINT: &str = "Template content and key material are hidden by default. Re-run with --verbose-will-reveal-sensitive-content to see full diffs.";

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
    /// When true, hidden body fields such as template `content` and
    /// key material render as a full unified diff inside their
    /// resource block. Shell scripts render in full in both modes.
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

    // One footer hint instead of repeating the flag on every hidden
    // body line. Verbose mode skips this (the user already opted in)
    // and plans with no body blocks skip it too (nothing to reveal).
    if !opts.verbose && plan_has_body_blocks(plan) {
        writeln!(out)?;
        writeln!(out, "{}", paint(opts.color, DIM, VERBOSE_FOOTER_HINT))?;
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

/// Whether the plan would hide at least one body-shaped field
/// (template `content` or key material) in default mode. Used by:
///   - the verbose banner emission (no banner if nothing's going to
///     reveal anything anyway), and
///   - the footer hint at the bottom of a default-mode plan — no
///     point pointing at `--verbose-will-reveal-sensitive-content`
///     when there's no content it would reveal.
pub fn plan_has_body_blocks(plan: &Plan) -> bool {
    plan.changes.iter().any(|c| {
        if matches!(c.action, Action::NoOp) {
            return false;
        }
        let body_of = |s: &Option<ResourceState>| -> Option<String> {
            s.as_ref().and_then(|state| match state {
                ResourceState::Template { content, .. } => Some(content.clone()),
                ResourceState::SshKey {
                    private_key,
                    public_key,
                    ..
                } => Some(format!("{private_key}\n{public_key}")),
                ResourceState::GpgKey { key, .. } => Some(key.clone()),
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

/// Render a body-shaped field that is hidden in default mode.
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
        render_body_verbose(out, field, before, after, sign, false, opts)
    } else {
        render_body_summary(out, field, before, after, sign, sensitive, opts)
    }
}

fn render_visible_body_field<W: Write>(
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
    render_body_verbose(out, field, before, after, sign, sensitive, opts)
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
            "      {sign} {field}{marker}: {removed} {line_removed} removed, {added} {line_added} added",
            line_removed = if removed == 1 { "line" } else { "lines" },
            line_added = if added == 1 { "line" } else { "lines" },
        )?;
    } else {
        let n = count_lines(after);
        writeln!(
            out,
            "      {sign} {field}{marker}: {n} {word}",
            word = if n == 1 { "line" } else { "lines" },
        )?;
    }
    Ok(())
}

/// Render a body field as a unified diff block, indented inside the
/// resource body. Real `\n` / `\t` pass
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
    sensitive: bool,
    opts: RenderOptions,
) -> io::Result<()> {
    let marker = if sensitive {
        format!(" {}", paint(opts.color, RED, "[sensitive]"))
    } else {
        String::new()
    };
    writeln!(out, "      {sign} {field}{marker}:")?;
    let before_text = before.unwrap_or("");
    let diff = similar::TextDiff::from_lines(before_text, after);
    let rendered = diff.unified_diff().context_radius(3).to_string();
    let safe = sanitize_terminal_message(&rendered);
    for line in safe.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n');
        // No `---` / `+++` header filtering here: `unified_diff()` only
        // emits those lines when `.header(...)` is set (it isn't), so
        // any line that looks like one is real content — e.g. a removed
        // Lua comment `-- x` renders as `--- x` and must stay visible.
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
        ResourceState::Package { manager, name, tap } => {
            writeln!(out, "      {s} manager = \"{}\"", manager.kind_label())?;
            writeln!(out, "      {s} name    = \"{}\"", escape_inline(name))?;
            if let Some(spec) = tap {
                writeln!(
                    out,
                    "      {s} tap     = \"{}\"",
                    escape_inline(&spec.user_tap)
                )?;
                if let Some(url) = &spec.url {
                    writeln!(out, "      {s} tap_url = \"{}\"", escape_inline(url))?;
                }
            }
        }
        ResourceState::Tap(spec) => {
            writeln!(
                out,
                "      {s} user_tap = \"{}\"",
                escape_inline(&spec.user_tap)
            )?;
            if let Some(url) = &spec.url {
                writeln!(out, "      {s} url      = \"{}\"", escape_inline(url))?;
            }
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
            render_visible_body_field(out, "script", None, script, &s, *sensitive, opts)?;
        }
        ResourceState::SshKey {
            private_path,
            public_path,
            private_key,
            public_key,
        } => {
            writeln!(
                out,
                "      {s} private_path = \"{}\"",
                show_path(private_path)
            )?;
            writeln!(
                out,
                "      {s} public_path  = \"{}\"",
                show_path(public_path)
            )?;
            // SSH keys are inherently sensitive — both halves go
            // through `render_body_field` with `sensitive = true` so
            // default mode prints `[sensitive]` and verbose mode
            // (`--verbose-will-reveal-sensitive-content`) prints the
            // PEM blob in full.
            render_body_field(out, "private", None, private_key, &s, true, opts)?;
            render_body_field(out, "public", None, public_key, &s, true, opts)?;
        }
        ResourceState::GpgKey { fingerprint, key } => {
            writeln!(
                out,
                "      {s} fingerprint = \"{}\"",
                escape_inline(fingerprint)
            )?;
            render_body_field(out, "key", None, key, &s, true, opts)?;
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
                tap: before_tap,
            },
            ResourceState::Package {
                manager: after_manager,
                name: after_name,
                tap: after_tap,
            },
        ) => {
            let before = PackageRenderState {
                manager: *before_manager,
                name: before_name,
                tap: before_tap.as_ref(),
            };
            let after = PackageRenderState {
                manager: *after_manager,
                name: after_name,
                tap: after_tap.as_ref(),
            };
            render_package_diff(out, before, after, &s)?;
        }
        (ResourceState::Tap(before_spec), ResourceState::Tap(after_spec)) => {
            render_tap_diff(out, before_spec, after_spec, &s)?;
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
    tap: Option<&'a crate::plan::TapSpec>,
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
    // Content unchanged with the same path means the only reason this is
    // an Update is a permission clamp on a sensitive file (back to
    // 0o600) — say so, otherwise the body renders empty and the Update
    // looks inexplicable while (previously) demanding a force override.
    if before.path == after.path && before.content == after.content {
        if sensitive {
            writeln!(
                out,
                "      {s} mode -> 0600 (restricting permissions on a sensitive file)"
            )?;
        }
        return Ok(());
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

fn render_tap_diff<W: Write>(
    out: &mut W,
    before_spec: &crate::plan::TapSpec,
    after_spec: &crate::plan::TapSpec,
    s: &str,
) -> io::Result<()> {
    // The only field that meaningfully changes between before/after for
    // a tap Update is the remote URL; the `user_tap` identity is the
    // address itself. The URLs are manifest- (and, for the `before`
    // remote, `brew tap-info`-) controlled and only shape-validated, so
    // escape them like every other body line before they reach the
    // terminal.
    if before_spec.url != after_spec.url {
        writeln!(
            out,
            "      {s} url = \"{}\" -> \"{}\"",
            before_spec
                .url
                .as_deref()
                .map_or_else(|| "(none)".to_string(), escape_inline),
            after_spec
                .url
                .as_deref()
                .map_or_else(|| "(none)".to_string(), escape_inline),
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
            before.manager.kind_label(),
            after.manager.kind_label()
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
    if before.tap != after.tap {
        let render = |t: Option<&crate::plan::TapSpec>| {
            t.map_or_else(
                || "(none)".to_string(),
                |spec| {
                    // `user_tap` and `url` are manifest-controlled and
                    // reach the terminal raw here; escape both.
                    spec.url.as_ref().map_or_else(
                        || escape_inline(&spec.user_tap),
                        |url| format!("{} ({})", escape_inline(&spec.user_tap), escape_inline(url)),
                    )
                },
            )
        };
        writeln!(
            out,
            "      {s} tap     = \"{}\" -> \"{}\"",
            render(before.tap),
            render(after.tap),
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
    render_visible_body_field(
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
#[path = "diff_tests.rs"]
mod tests;
