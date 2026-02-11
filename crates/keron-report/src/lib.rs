use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write;
use std::io::{self, IsTerminal};

use console::Style;
use keron_domain::{
    ApplyOperationResult, ApplyReport, PlanAction, PlanReport, PlannedOperation, Resource,
};

mod error;
mod options;
mod redaction;

pub use error::ReportError;
pub use options::{ColorChoice, OutputFormat, RenderOptions};
pub use redaction::redact_sensitive;

/// Render a plan report in the requested output format.
///
/// # Errors
///
/// Returns an error if JSON serialization fails.
pub fn render_plan(
    report: &PlanReport,
    format: OutputFormat,
    options: &RenderOptions,
) -> std::result::Result<String, ReportError> {
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(report)
            .map_err(|source| ReportError::JsonSerialize { source }),
        OutputFormat::Text => Ok(render_plan_text(report, options)),
    }
}

/// Render an apply report in the requested output format.
///
/// # Errors
///
/// Returns an error if JSON serialization fails.
pub fn render_apply(
    report: &ApplyReport,
    format: OutputFormat,
    options: &RenderOptions,
) -> std::result::Result<String, ReportError> {
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(report)
            .map_err(|source| ReportError::JsonSerialize { source }),
        OutputFormat::Text => Ok(render_apply_text(report, options)),
    }
}

// ---------------------------------------------------------------------------
// Plan text
// ---------------------------------------------------------------------------

fn render_plan_text(report: &PlanReport, options: &RenderOptions) -> String {
    let mut output = String::new();
    let style = TextStyle::new(options.color);

    append_header(&mut output, "plan", options.target.as_deref(), None, &style);

    if report.operations.is_empty() {
        let _ = writeln!(output, "  Nothing to do.");
        append_warnings_and_errors(
            &mut output,
            &report.warnings,
            &report.errors,
            options.verbose,
            &style,
        );
        return output;
    }

    let (changed, noops): (Vec<&PlannedOperation>, Vec<&PlannedOperation>) = report
        .operations
        .iter()
        .partition(|op| op.would_change || op.conflict || op.error.is_some());

    if changed.is_empty() && !noops.is_empty() && !options.verbose {
        let _ = writeln!(output);
        let noop_counts = NoopCounts::from_ops(&noops);
        let _ = writeln!(output, "  {}", style.dim(&noop_counts.format()));
        append_warnings_and_errors(
            &mut output,
            &report.warnings,
            &report.errors,
            options.verbose,
            &style,
        );
        let _ = writeln!(output);
        let tally = TallyCounts::from_plan_ops(&report.operations);
        let _ = writeln!(output, "{}", tally.format_plan(&style));
        return output;
    }

    let _ = writeln!(output);
    append_warnings_and_errors(
        &mut output,
        &report.warnings,
        &report.errors,
        options.verbose,
        &style,
    );
    for op in &changed {
        append_plan_op_line(&mut output, op, options, &style);
    }

    if options.verbose {
        for op in &noops {
            append_plan_op_line(&mut output, op, options, &style);
        }
    }

    if !noops.is_empty() && !options.verbose {
        let _ = writeln!(output);
        let noop_counts = NoopCounts::from_ops(&noops);
        let _ = writeln!(output, "  {}", style.dim(&noop_counts.format()));
    }

    let _ = writeln!(output);
    let tally = TallyCounts::from_plan_ops(&report.operations);
    let _ = writeln!(output, "{}", tally.format_plan(&style));

    output
}

// ---------------------------------------------------------------------------
// Apply text
// ---------------------------------------------------------------------------

fn render_apply_text(report: &ApplyReport, options: &RenderOptions) -> String {
    let mut output = String::new();
    let style = TextStyle::new(options.color);
    let operation_map: HashMap<usize, &PlannedOperation> = report
        .plan
        .operations
        .iter()
        .map(|op| (op.id, op))
        .collect();

    append_header(
        &mut output,
        "apply",
        options.target.as_deref(),
        None,
        &style,
    );

    if report.results.is_empty() {
        let _ = writeln!(output, "  Nothing to do.");
        append_warnings_and_errors(&mut output, &[], &report.errors, options.verbose, &style);
        return output;
    }

    let (active, noop_results): (Vec<&ApplyOperationResult>, Vec<&ApplyOperationResult>) =
        report.results.iter().partition(|r| r.changed || !r.success);

    let _ = writeln!(output);
    append_warnings_and_errors(&mut output, &[], &report.errors, options.verbose, &style);
    for result in &active {
        let planned = operation_map.get(&result.operation_id).copied();
        append_apply_op_line(&mut output, result, planned, options, &style);
    }

    if !noop_results.is_empty() && !options.verbose {
        let _ = writeln!(output);
        let _ = writeln!(
            output,
            "  {}",
            style.dim(&format!("{} unchanged", noop_results.len()))
        );
    }

    if options.verbose {
        for result in &noop_results {
            let planned = operation_map.get(&result.operation_id).copied();
            append_apply_op_line(&mut output, result, planned, options, &style);
        }
    }

    let _ = writeln!(output);
    let tally = ApplyTally::from_results(&report.results);
    let _ = writeln!(output, "{}", tally.format(&style));

    output
}

// ---------------------------------------------------------------------------
// Line renderers
// ---------------------------------------------------------------------------

fn append_header(
    output: &mut String,
    command: &str,
    target: Option<&str>,
    suffix: Option<&str>,
    style: &TextStyle,
) {
    let _ = write!(output, "{}", style.header_command(command));
    if let Some(t) = target {
        let _ = write!(output, " {}", style.header_target(t));
    }
    if let Some(s) = suffix {
        let _ = write!(output, " {s}");
    }
    let _ = writeln!(output);
}

fn append_plan_op_line(
    output: &mut String,
    op: &PlannedOperation,
    options: &RenderOptions,
    style: &TextStyle,
) {
    let (symbol, label) = plan_symbol_and_label(op, style);
    let detail = format_resource_detail(&op.resource, op.action, style, options.verbose);
    let _ = writeln!(output, "  {symbol} {label}{detail}");

    if options.verbose {
        let manifest_name = op.manifest.file_name().map_or_else(
            || op.manifest.display().to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        let _ = writeln!(
            output,
            "    {}",
            style.dim(&format!("#{} {}", op.id, manifest_name))
        );

        if let Some(hash) = &op.content_hash {
            let short = &hash[..hash.len().min(12)];
            let _ = writeln!(
                output,
                "    {}",
                style.dim(&format!("content:  sha256:{short}"))
            );
        }
        if let Some(hash) = &op.dest_content_hash {
            let short = &hash[..hash.len().min(12)];
            let _ = writeln!(
                output,
                "    {}",
                style.dim(&format!("dest:     sha256:{short}"))
            );
        }
    }

    if let Some(hint) = &op.hint {
        let rendered_hint = render_warning(hint, options.verbose);
        let _ = writeln!(output, "    {} {rendered_hint}", style.warn_prefix("warn:"));
    }

    if let Some(error) = &op.error {
        let _ = writeln!(output, "    {} {error}", style.error_prefix("error:"));
    }
}

fn append_apply_op_line(
    output: &mut String,
    result: &ApplyOperationResult,
    planned: Option<&PlannedOperation>,
    options: &RenderOptions,
    style: &TextStyle,
) {
    let (symbol, label) = apply_symbol_and_label(result, planned, style);
    let detail = planned.map_or_else(String::new, |op| {
        format_resource_detail(&op.resource, op.action, style, options.verbose)
    });
    let _ = writeln!(output, "  {symbol} {label}{detail}");

    if !result.success
        && let Some(error) = &result.error
    {
        let _ = writeln!(output, "                     {}", style.error_detail(error));
    }

    if options.verbose
        && let Some(op) = planned
    {
        let manifest_name = op.manifest.file_name().map_or_else(
            || op.manifest.display().to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        let _ = writeln!(
            output,
            "    {}",
            style.dim(&format!("#{} {}", op.id, manifest_name))
        );
    }
}

// ---------------------------------------------------------------------------
// Warnings & Errors
// ---------------------------------------------------------------------------

fn append_warnings_and_errors(
    output: &mut String,
    warnings: &[String],
    errors: &[String],
    verbose: bool,
    style: &TextStyle,
) {
    if warnings.is_empty() && errors.is_empty() {
        return;
    }
    let _ = writeln!(output);
    for w in warnings {
        let warning = render_warning(w, verbose);
        let _ = writeln!(output, "  {} {warning}", style.warn_prefix("warn:"));
    }
    for e in errors {
        let _ = writeln!(output, "  {} {e}", style.error_prefix("error:"));
    }
}

fn render_warning(warning: &str, verbose: bool) -> Cow<'_, str> {
    const DETAIL_MARKER: &str = " (default folders: ";
    if verbose {
        return Cow::Borrowed(warning);
    }

    warning.split_once(DETAIL_MARKER).map_or_else(
        || Cow::Borrowed(warning),
        |(summary, _)| Cow::Borrowed(summary),
    )
}

// ---------------------------------------------------------------------------
// Symbol + Label helpers
// ---------------------------------------------------------------------------

fn plan_symbol_and_label(op: &PlannedOperation, style: &TextStyle) -> (String, String) {
    if op.error.is_some() && op.conflict {
        return (
            style.conflict_symbol("!"),
            format!("{} ", style.conflict_label("conflict")),
        );
    }
    if op.error.is_some() {
        return (
            style.error_op_symbol("!"),
            TextStyle::pad_label(&style.error_op_label("error")),
        );
    }
    if op.conflict {
        return (
            style.conflict_symbol("!"),
            format!("{} ", style.conflict_label("conflict")),
        );
    }

    let (sym, label) = action_label(op.action, false);
    match sym {
        "+" => (
            style.add_symbol("+"),
            TextStyle::pad_label(&style.add_label(label)),
        ),
        "~" => (
            style.change_symbol("~"),
            TextStyle::pad_label(&style.change_label(label)),
        ),
        "=" => (
            style.noop_symbol("="),
            TextStyle::pad_label(&style.noop_label(label)),
        ),
        _ => (style.dim(sym), TextStyle::pad_label(&style.dim(label))),
    }
}

fn apply_symbol_and_label(
    result: &ApplyOperationResult,
    planned: Option<&PlannedOperation>,
    style: &TextStyle,
) -> (String, String) {
    if !result.success {
        let fail_label = planned.map_or("failed", |op| match op.resource {
            Resource::Command(_) => "failed command",
            _ => "failed",
        });
        return (
            style.error_op_symbol("!"),
            TextStyle::pad_label(&style.error_op_label(fail_label)),
        );
    }

    if !result.changed {
        return (
            style.noop_symbol("="),
            TextStyle::pad_label(&style.noop_label("unchanged")),
        );
    }

    let (sym, label) = planned.map_or(("+", "applied"), |op| action_label(op.action, true));
    match sym {
        "+" => (
            style.add_symbol("+"),
            TextStyle::pad_label(&style.add_label(label)),
        ),
        "~" => (
            style.change_symbol("~"),
            TextStyle::pad_label(&style.change_label(label)),
        ),
        _ => (style.dim(sym), TextStyle::pad_label(&style.dim(label))),
    }
}

const fn action_label(action: PlanAction, past_tense: bool) -> (&'static str, &'static str) {
    match (action, past_tense) {
        (PlanAction::LinkCreate, false) => ("+", "create link"),
        (PlanAction::LinkCreate, true) => ("+", "created link"),
        (PlanAction::LinkReplace, false) => ("~", "replace link"),
        (PlanAction::LinkReplace, true) => ("~", "replaced link"),
        (PlanAction::LinkNoop, _) => ("=", "link up to date"),
        (PlanAction::LinkConflict | PlanAction::TemplateConflict, _) => ("!", "conflict"),
        (PlanAction::TemplateCreate, false) => ("+", "render template"),
        (PlanAction::TemplateCreate, true) => ("+", "rendered template"),
        (PlanAction::TemplateUpdate, false) => ("~", "rerender template"),
        (PlanAction::TemplateUpdate, true) => ("~", "rerendered template"),
        (PlanAction::TemplateNoop, _) => ("=", "template up to date"),
        (PlanAction::PackageInstall, false) => ("+", "install package"),
        (PlanAction::PackageInstall, true) => ("+", "installed"),
        (PlanAction::PackageRemove, false) => ("+", "remove package"),
        (PlanAction::PackageRemove, true) => ("+", "removed"),
        (PlanAction::PackageNoop, _) => ("=", "package up to date"),
        (PlanAction::CommandRun, false) => ("+", "run command"),
        (PlanAction::CommandRun, true) => ("+", "ran command"),
    }
}

// ---------------------------------------------------------------------------
// Resource detail rendering
// ---------------------------------------------------------------------------

fn format_resource_detail(
    resource: &Resource,
    _action: PlanAction,
    style: &TextStyle,
    verbose: bool,
) -> String {
    match resource {
        Resource::Link(link) => {
            let src = shorten_path(&link.src.display().to_string());
            let dest = shorten_path(&link.dest.display().to_string());
            format!(
                "{} {} {}",
                style.dim(&src),
                style.dim("->"),
                style.primary_text(&dest)
            )
        }
        Resource::Template(tmpl) => {
            let src = shorten_path(&tmpl.src.display().to_string());
            let dest = shorten_path(&tmpl.dest.display().to_string());
            format!(
                "{} {} {}",
                style.dim(&src),
                style.dim("->"),
                style.primary_text(&dest)
            )
        }
        Resource::Package(pkg) => {
            let provider = pkg.provider_hint.as_deref().map_or(String::new(), |p| {
                format!(" {}", style.dim(&format!("via {p}")))
            });
            format!("{}{provider}", style.primary_text(&pkg.name))
        }
        Resource::Command(cmd) => {
            let full = if cmd.args.is_empty() {
                cmd.binary.clone()
            } else {
                format!("{} {}", cmd.binary, cmd.args.join(" "))
            };
            let display = if !verbose && full.len() > 60 {
                format!("{}...", &full[..57])
            } else {
                full
            };
            style.dim(&display)
        }
    }
}

fn shorten_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME")
        && let Some(rest) = path.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    path.to_string()
}

// ---------------------------------------------------------------------------
// Noop Counts
// ---------------------------------------------------------------------------

struct NoopCounts {
    links: usize,
    templates: usize,
    packages: usize,
    commands: usize,
}

impl NoopCounts {
    fn from_ops(ops: &[&PlannedOperation]) -> Self {
        let mut counts = Self {
            links: 0,
            templates: 0,
            packages: 0,
            commands: 0,
        };
        for op in ops {
            match op.resource {
                Resource::Link(_) => counts.links += 1,
                Resource::Template(_) => counts.templates += 1,
                Resource::Package(_) => counts.packages += 1,
                Resource::Command(_) => counts.commands += 1,
            }
        }
        counts
    }

    const fn total(&self) -> usize {
        self.links + self.templates + self.packages + self.commands
    }

    fn format(&self) -> String {
        let total = self.total();
        let mut parts = Vec::new();
        if self.links > 0 {
            parts.push(format!(
                "{} {}",
                self.links,
                if self.links == 1 { "link" } else { "links" }
            ));
        }
        if self.templates > 0 {
            parts.push(format!(
                "{} {}",
                self.templates,
                if self.templates == 1 {
                    "template"
                } else {
                    "templates"
                }
            ));
        }
        if self.packages > 0 {
            parts.push(format!(
                "{} {}",
                self.packages,
                if self.packages == 1 {
                    "package"
                } else {
                    "packages"
                }
            ));
        }
        if self.commands > 0 {
            parts.push(format!(
                "{} {}",
                self.commands,
                if self.commands == 1 {
                    "command"
                } else {
                    "commands"
                }
            ));
        }
        if parts.is_empty() {
            format!("{total} unchanged")
        } else {
            format!("{total} unchanged ({})", parts.join(", "))
        }
    }
}

// ---------------------------------------------------------------------------
// Tally Counts (Plan)
// ---------------------------------------------------------------------------

struct TallyCounts {
    adds: usize,
    changes: usize,
    conflicts: usize,
    errors: usize,
    unchanged: usize,
}

impl TallyCounts {
    fn from_plan_ops(ops: &[PlannedOperation]) -> Self {
        let mut tally = Self {
            adds: 0,
            changes: 0,
            conflicts: 0,
            errors: 0,
            unchanged: 0,
        };
        for op in ops {
            if op.error.is_some() {
                tally.errors += 1;
            } else if op.conflict {
                tally.conflicts += 1;
            } else if matches!(
                op.action,
                PlanAction::LinkReplace | PlanAction::TemplateUpdate
            ) {
                tally.changes += 1;
            } else if op.would_change {
                tally.adds += 1;
            } else {
                tally.unchanged += 1;
            }
        }
        tally
    }

    fn format_plan(&self, style: &TextStyle) -> String {
        let mut parts = Vec::new();
        if self.adds > 0 {
            parts.push(style.add_label(&format!("{} to add", self.adds)));
        }
        if self.changes > 0 {
            parts.push(style.change_label(&format!("{} to change", self.changes)));
        }
        if self.conflicts > 0 {
            parts.push(style.conflict_label(&format!("{} conflict", self.conflicts)));
        }
        if self.errors > 0 {
            parts.push(style.error_op_label(&format!("{} error", self.errors)));
        }
        if self.unchanged > 0 {
            parts.push(style.dim(&format!("{} unchanged", self.unchanged)));
        }
        if parts.is_empty() {
            format!("{} nothing to do", style.tally_label("Plan:"))
        } else {
            format!("{} {}", style.tally_label("Plan:"), parts.join(", "))
        }
    }
}

// ---------------------------------------------------------------------------
// Apply Tally
// ---------------------------------------------------------------------------

struct ApplyTally {
    added: usize,
    changed: usize,
    failed: usize,
    unchanged: usize,
}

impl ApplyTally {
    fn from_results(results: &[ApplyOperationResult]) -> Self {
        let mut tally = Self {
            added: 0,
            changed: 0,
            failed: 0,
            unchanged: 0,
        };
        for r in results {
            if !r.success {
                tally.failed += 1;
            } else if r.changed {
                // We lump all changed into "added" for the tally — the plan
                // distinguishes add/change but apply results don't carry the
                // original action easily.
                tally.added += 1;
            } else {
                tally.unchanged += 1;
            }
        }
        tally
    }

    fn format(&self, style: &TextStyle) -> String {
        let mut parts = Vec::new();
        if self.added > 0 {
            parts.push(style.add_label(&format!("{} added", self.added)));
        }
        if self.changed > 0 {
            parts.push(style.change_label(&format!("{} changed", self.changed)));
        }
        if self.failed > 0 {
            parts.push(style.error_op_label(&format!("{} failed", self.failed)));
        }
        if self.unchanged > 0 {
            parts.push(style.dim(&format!("{} unchanged", self.unchanged)));
        }
        if parts.is_empty() {
            format!("{} nothing to do", style.tally_label("Applied:"))
        } else {
            format!("{} {}", style.tally_label("Applied:"), parts.join(", "))
        }
    }
}

// ---------------------------------------------------------------------------
// TextStyle
// ---------------------------------------------------------------------------

const LABEL_WIDTH: usize = 16;

#[derive(Debug, Clone)]
struct TextStyle {
    color_enabled: bool,
    // Symbols
    add_sym_style: Style,
    change_sym_style: Style,
    conflict_sym_style: Style,
    error_sym_style: Style,
    noop_sym_style: Style,
    // Labels
    add_label_style: Style,
    change_label_style: Style,
    conflict_label_style: Style,
    error_label_style: Style,
    noop_label_style: Style,
    // Content
    primary_style: Style,
    dim_style: Style,
    error_detail_style: Style,
    // Header
    header_cmd_style: Style,
    header_target_style: Style,
    // Prefixes
    warn_prefix_style: Style,
    error_prefix_style: Style,
    // Tally
    tally_label_style: Style,
}

impl TextStyle {
    fn new(choice: ColorChoice) -> Self {
        let enabled = should_color(choice);
        Self {
            color_enabled: enabled,
            add_sym_style: Style::new().green().bold(),
            change_sym_style: Style::new().cyan().bold(),
            conflict_sym_style: Style::new().yellow().bold(),
            error_sym_style: Style::new().red().bold(),
            noop_sym_style: Style::new().dim(),
            add_label_style: Style::new().green(),
            change_label_style: Style::new().cyan(),
            conflict_label_style: Style::new().yellow(),
            error_label_style: Style::new().red(),
            noop_label_style: Style::new().dim(),
            primary_style: Style::new().white(),
            dim_style: Style::new().dim(),
            error_detail_style: Style::new().red(),
            header_cmd_style: Style::new().white().bold(),
            header_target_style: Style::new().dim(),
            warn_prefix_style: Style::new().yellow().bold(),
            error_prefix_style: Style::new().red().bold(),
            tally_label_style: Style::new().white().bold(),
        }
    }

    fn paint<T: std::fmt::Display>(&self, style: &Style, text: T) -> String {
        if self.color_enabled {
            style.apply_to(text).to_string()
        } else {
            text.to_string()
        }
    }

    fn pad_label(painted: &str) -> String {
        // Compute visible length (strip ANSI codes)
        let visible_len = console::measure_text_width(painted);
        if visible_len < LABEL_WIDTH {
            format!("{painted}{}", " ".repeat(LABEL_WIDTH - visible_len))
        } else {
            format!("{painted} ")
        }
    }

    // Symbols
    fn add_symbol(&self, s: &str) -> String {
        self.paint(&self.add_sym_style, s)
    }
    fn change_symbol(&self, s: &str) -> String {
        self.paint(&self.change_sym_style, s)
    }
    fn conflict_symbol(&self, s: &str) -> String {
        self.paint(&self.conflict_sym_style, s)
    }
    fn error_op_symbol(&self, s: &str) -> String {
        self.paint(&self.error_sym_style, s)
    }
    fn noop_symbol(&self, s: &str) -> String {
        self.paint(&self.noop_sym_style, s)
    }

    // Labels
    fn add_label(&self, s: &str) -> String {
        self.paint(&self.add_label_style, s)
    }
    fn change_label(&self, s: &str) -> String {
        self.paint(&self.change_label_style, s)
    }
    fn conflict_label(&self, s: &str) -> String {
        self.paint(&self.conflict_label_style, s)
    }
    fn error_op_label(&self, s: &str) -> String {
        self.paint(&self.error_label_style, s)
    }
    fn noop_label(&self, s: &str) -> String {
        self.paint(&self.noop_label_style, s)
    }

    // Content
    fn primary_text(&self, s: &str) -> String {
        self.paint(&self.primary_style, s)
    }
    fn dim(&self, s: &str) -> String {
        self.paint(&self.dim_style, s)
    }
    fn error_detail(&self, s: &str) -> String {
        self.paint(&self.error_detail_style, s)
    }

    // Header
    fn header_command(&self, s: &str) -> String {
        self.paint(&self.header_cmd_style, s)
    }
    fn header_target(&self, s: &str) -> String {
        self.paint(&self.header_target_style, s)
    }

    // Prefixes
    fn warn_prefix(&self, s: &str) -> String {
        self.paint(&self.warn_prefix_style, s)
    }
    fn error_prefix(&self, s: &str) -> String {
        self.paint(&self.error_prefix_style, s)
    }

    // Tally
    fn tally_label(&self, s: &str) -> String {
        self.paint(&self.tally_label_style, s)
    }
}

fn should_color(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => io::stdout().is_terminal(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
