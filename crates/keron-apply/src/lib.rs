//! keron-apply: plan and execute resources produced by keron-lang.
//!
//! Public surface is a single `run` function called by the CLI. It
//! loads the input (a single `.keron` file or a directory of them),
//! parses + type-checks via `keron-lang`, builds a `Plan`, renders an
//! OpenTofu-style diff, and — when `execute` is set — prompts the
//! user before applying.
//!
//! The evaluator (AST → concrete resources) and executor are stubbed
//! today; both fail with a clear "not implemented" error so the
//! surrounding scaffolding can be exercised in isolation.

mod confirm;
mod diff;
mod eval;
mod execute;
mod load;
mod plan;

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use anyhow::Result;

use crate::diff::RenderOptions;

/// Plan a keron program at `path`. With `execute`, prompt and apply.
///
/// # Errors
/// Returns an error if loading, parsing, type-checking, plan building,
/// or (when `execute`) the executor fails.
pub fn run(path: &Path, execute: bool) -> Result<()> {
    let source = load::load(path)?;

    let program = keron_lang::parse(&source.text).map_err(|diags| {
        anyhow::anyhow!(
            "parse failed:\n{}",
            render_diagnostics(&source.text, &diags)
        )
    })?;

    keron_lang::check(&program).map_err(|diags| {
        anyhow::anyhow!(
            "type check failed:\n{}",
            render_diagnostics(&source.text, &diags)
        )
    })?;

    let plan = plan::build_plan(&program)?;

    let opts = RenderOptions {
        color: io::stdout().is_terminal(),
    };
    {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        diff::render_plan(&mut out, &plan, opts)?;
    }

    if !execute || plan.is_empty() {
        return Ok(());
    }

    let approved = {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut sin = stdin.lock();
        let mut sout = stdout.lock();
        confirm::prompt_yes_no(&mut sin, &mut sout)?
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if !approved {
        writeln!(out, "Apply cancelled.")?;
        return Ok(());
    }

    let summary = execute::execute(&plan)?;
    writeln!(
        out,
        "Apply complete! Resources: {} added, {} changed, {} destroyed.",
        summary.added, summary.changed, summary.destroyed
    )?;
    Ok(())
}

fn render_diagnostics(source: &str, diags: &[keron_lang::Diagnostic]) -> String {
    let mut out = String::new();
    for d in diags {
        let (line, col) = line_col(source, d.span.start);
        // Infallible: writing to a `String`.
        let _ = writeln!(out, "  [{line}:{col}] {message}", message = d.message);
    }
    out
}

fn line_col(source: &str, byte_offset: usize) -> (usize, usize) {
    let clamped = byte_offset.min(source.len());
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in source.char_indices() {
        if i >= clamped {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}
