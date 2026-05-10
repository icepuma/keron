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
mod report;

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result};
use keron_modules::{EntrySource, ModuleId, resolve};

use crate::diff::RenderOptions;

/// Plan a keron program at `path`. With `execute`, prompt and apply.
///
/// # Errors
/// Returns an error if loading, parsing, type-checking, plan building,
/// or (when `execute`) the executor fails.
pub fn run(path: &Path, execute: bool) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let color = stdout.is_terminal();
    let mut sin = stdin.lock();
    let mut sout = stdout.lock();
    run_with_io(path, execute, &mut sin, &mut sout, color)
}

/// Test-friendly entry: same logic as [`run`] but with explicit IO so
/// tests can drive the prompt path without touching real stdio. The
/// public `run` wrapper feeds in real `stdin`/`stdout` and detects
/// terminal-ness for color.
pub(crate) fn run_with_io<R, W>(
    path: &Path,
    execute: bool,
    stdin: &mut R,
    stdout: &mut W,
    color: bool,
) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    let source = load::load(path)?;
    let entry_id = entry_module_id(path)?;
    let base_dir = entry_base_dir(path)?;

    let graph = resolve(EntrySource {
        text: source.text,
        base_dir,
        id: entry_id,
    })
    .map_err(|bundle| {
        anyhow::anyhow!(
            "module resolution failed:\n{}",
            report::render(&bundle, color)
        )
    })?;

    let plan = plan::build_plan(&graph)?;

    diff::render_plan(stdout, &plan, RenderOptions { color })?;

    if !execute || plan.is_empty() {
        return Ok(());
    }

    let approved = confirm::prompt_yes_no(stdin, stdout)?;
    if !approved {
        writeln!(stdout, "Apply cancelled.")?;
        return Ok(());
    }

    let summary = execute::execute(&plan)?;
    writeln!(
        stdout,
        "Apply complete! Resources: {} added, {} changed, {} destroyed.",
        summary.added, summary.changed, summary.destroyed
    )?;
    Ok(())
}

fn entry_module_id(path: &Path) -> Result<ModuleId> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalizing entry `{}`", path.display()))?;
    Ok(ModuleId::File(canonical))
}

fn entry_base_dir(path: &Path) -> Result<std::path::PathBuf> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalizing entry `{}`", path.display()))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Ok(canonical.parent().unwrap_or(&canonical).to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempProject {
        root: PathBuf,
    }

    impl TempProject {
        fn new(name: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let root =
                env::temp_dir().join(format!("keron-run-test-{name}-{}-{n}", std::process::id()));
            if root.exists() {
                fs::remove_dir_all(&root).ok();
            }
            fs::create_dir_all(&root).unwrap();
            // Seed the same `tmpl.tpl` shim the eval-side tests use so
            // entry files can call `template(path = X, source =
            // "tmpl.tpl", vars = {"body": Y})` as a direct stand-in
            // for the old `file(path = X, content = Y)`.
            fs::write(root.join("tmpl.tpl"), "${body}").unwrap();
            Self { root }
        }
        fn write(&self, rel: &str, src: &str) -> PathBuf {
            let path = self.root.join(rel);
            fs::write(&path, src).unwrap();
            path
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Drives `run_with_io` and returns (result, captured stdout).
    fn drive(path: &Path, execute: bool, stdin: &str) -> (Result<()>, String) {
        let mut sin = Cursor::new(stdin.as_bytes().to_vec());
        let mut sout: Vec<u8> = Vec::new();
        let res = run_with_io(path, execute, &mut sin, &mut sout, false);
        (res, String::from_utf8(sout).unwrap())
    }

    #[test]
    fn entry_base_dir_for_file_returns_parent() {
        let dir = std::env::temp_dir().join("keron-base-dir-file");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("entry.keron");
        fs::write(&file, "").unwrap();
        let got = entry_base_dir(&file).unwrap();
        let canonical_dir = fs::canonicalize(&dir).unwrap();
        assert_eq!(got, canonical_dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_base_dir_for_dir_returns_dir_itself() {
        let dir = std::env::temp_dir().join("keron-base-dir-dir");
        fs::create_dir_all(&dir).unwrap();
        let got = entry_base_dir(&dir).unwrap();
        let canonical_dir = fs::canonicalize(&dir).unwrap();
        assert_eq!(got, canonical_dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_module_id_canonicalizes_path() {
        let dir = std::env::temp_dir().join("keron-entry-mod-id");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("entry.keron");
        fs::write(&file, "").unwrap();
        let got = entry_module_id(&file).unwrap();
        let ModuleId::File(p) = got;
        assert_eq!(p, fs::canonicalize(&file).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_returns_err_for_missing_entry() {
        // Pin the `Ok(())` mutation: run on a path that doesn't exist
        // returns Err.
        let missing = PathBuf::from("/no/such/keron-test-entry.keron");
        let (res, _) = drive(&missing, false, "");
        assert!(res.is_err());
    }

    #[test]
    fn public_run_wrapper_returns_err_for_missing_entry() {
        // Pin the `Ok(())` mutation on the *thin wrapper* `run`. The
        // wrapper just constructs real stdin/stdout and delegates to
        // `run_with_io` — but mutating the wrapper itself to `Ok(())`
        // would skip the delegation entirely. Calling with a path
        // that can't be read makes the underlying `run_with_io` bail
        // before any stdin read happens, so this works without
        // touching real terminal IO.
        let missing = PathBuf::from(format!(
            "/no/such/keron-public-run-{}-{}.keron",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        let res = run(&missing, false);
        assert!(res.is_err(), "expected Err for missing path");
    }

    #[test]
    fn run_renders_diff_and_returns_when_not_executing() {
        // execute=false hits the early return at line 63 *via*
        // `!execute` being true. Verifies the diff was rendered (so
        // the body isn't replaced by `Ok(())`) AND that no prompt
        // was emitted (early return fired).
        let proj = TempProject::new("not-execute");
        let entry = proj.write(
            "entry.keron",
            "reconcile template(path = \"/x\", source = \"tmpl.tpl\", vars = {\"body\": \"y\"})\n",
        );
        let (res, out) = drive(&entry, false, "");
        res.unwrap();
        assert!(out.contains("will be created"), "diff missing: {out}");
        assert!(
            !out.contains("Only 'yes' will be accepted"),
            "should not prompt: {out}",
        );
    }

    #[test]
    fn run_returns_early_when_plan_is_empty_even_with_execute() {
        // Plan-empty branch of `!execute || plan.is_empty()`. Mutating
        // `||` to `&&` would leave the condition false here (execute
        // is true, so `!execute` is false; with `&&` even an empty
        // plan can't trigger the early return), pushing through to
        // the prompt. We pass empty stdin: if we ever reach the
        // prompt, `prompt_yes_no` would still succeed (treating EOF
        // as not-approved), so we instead assert the early return
        // signature: `Apply cancelled` must NOT appear and the
        // executor's "not implemented" error must NOT appear.
        let proj = TempProject::new("empty-plan");
        let entry = proj.write(
            "entry.keron",
            "val f: Template = template(path = \"/x\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        let (res, out) = drive(&entry, true, "");
        res.unwrap();
        assert!(
            out.contains("No changes"),
            "expected no-changes message: {out}"
        );
        assert!(!out.contains("Apply cancelled"), "should not prompt: {out}");
        assert!(
            !out.contains("Only 'yes' will be accepted"),
            "should not prompt: {out}",
        );
    }

    #[test]
    fn run_with_execute_and_no_approval_prints_cancelled() {
        // Runs the prompt path. Stdin sends "no\n" → `approved=false`.
        // The `!approved` branch must print "Apply cancelled." and
        // return Ok. Without `!`, the branch would fall through to
        // the executor which currently errors.
        let proj = TempProject::new("no-approval");
        let entry = proj.write(
            "entry.keron",
            "reconcile template(path = \"/x\", source = \"tmpl.tpl\", vars = {\"body\": \"y\"})\n",
        );
        let (res, out) = drive(&entry, true, "no\n");
        res.unwrap();
        assert!(
            out.contains("Apply cancelled"),
            "missing cancel message: {out}"
        );
        assert!(!out.contains("not yet implemented"), "executor ran: {out}");
    }

    #[test]
    fn run_with_execute_and_approval_invokes_executor() {
        // "yes" input → `approved=true`, original goes past the `!approved`
        // branch into the executor (which currently errors with "not
        // yet implemented"). Verifies the success path actually
        // reaches `execute::execute`.
        let proj = TempProject::new("yes-approval");
        let entry = proj.write(
            "entry.keron",
            "reconcile template(path = \"/x\", source = \"tmpl.tpl\", vars = {\"body\": \"y\"})\n",
        );
        let (res, out) = drive(&entry, true, "yes\n");
        let err = res.unwrap_err();
        assert!(
            err.to_string().contains("not yet implemented"),
            "expected executor error, got: {err}",
        );
        // The diff should still be rendered before the executor fired.
        assert!(out.contains("will be created"), "diff missing: {out}");
        // No "Apply cancelled" because approved was true.
        assert!(!out.contains("Apply cancelled"), "got: {out}");
    }
}
