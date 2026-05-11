//! keron-apply: plan and execute resources produced by keron-lang.
//!
//! Public surface is a single `run` function called by the CLI. It
//! loads the input (a single `.keron` file or a directory of them),
//! parses + type-checks via `keron-lang`, builds a `Plan`, renders an
//! OpenTofu-style diff, and — when `execute` is set — prompts the
//! user before applying.
//!
//! The executor wires up symlinks end-to-end today; templates,
//! directories, and packages still bail with a clear "not yet
//! implemented" diagnostic at apply time. The planner only diffs the
//! resource kinds the executor can act on, so the rendered diff
//! stays truthful as new kinds come online.

mod confirm;
mod diff;
mod elevated;
mod eval;
mod execute;
mod load;
mod packages;
mod plan;
mod report;

pub use elevated::child::run as run_elevated_child;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
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
    refuse_direct_elevation()?;

    let source = load::load(path)?;
    let keron_root = keron_root_for(path, &source)?;
    let roots: Vec<EntrySource> = source
        .files
        .into_iter()
        .map(|f| {
            // The file's parent is the resolution root for relative
            // `use` paths in that file (`./` and `../` resolve here).
            // `LoadedFile.path` is canonical, so `parent()` is too.
            let base_dir = f
                .path
                .parent()
                .map_or_else(|| f.path.clone(), Path::to_path_buf);
            EntrySource {
                text: f.text,
                base_dir,
                id: ModuleId::File(f.path),
            }
        })
        .collect();

    let graph = resolve(roots).map_err(|bundle| {
        anyhow::anyhow!(
            "module resolution failed:\n{}",
            report::render(&bundle, color)
        )
    })?;

    let plan = plan::build_plan(&graph, &keron_root)?;

    diff::render_plan(stdout, &plan, RenderOptions { color })?;

    if !execute || plan.is_empty() {
        return Ok(());
    }

    let approved = confirm::prompt_yes_no(stdin, stdout)?;
    if !approved {
        writeln!(stdout, "Apply cancelled.")?;
        return Ok(());
    }

    let (unprivileged, elevated_plan) = plan.partition_by_elevation();
    let unpriv_summary = execute::execute(&unprivileged)?;
    writeln!(
        stdout,
        "Apply complete! Resources: {} added, {} changed, {} destroyed.",
        unpriv_summary.added, unpriv_summary.changed, unpriv_summary.destroyed
    )?;

    if !elevated_plan.changes.is_empty() {
        writeln!(
            stdout,
            "{} resource(s) require elevated rights; you may be asked for your password.",
            elevated_plan.changes.len(),
        )?;
        elevated::run_elevated(&elevated_plan)?;
    }
    Ok(())
}

/// Bail loudly when `keron apply` is run as root / Administrator
/// directly. The elevated path is the *only* call site that should
/// ever run privileged: a direct `sudo keron apply` would apply the
/// whole manifest as root, leaving `~/.config` files owned by root
/// (the home-manager #4019 footgun). Mirrors rustup-init's elevation
/// guard on Windows.
fn refuse_direct_elevation() -> Result<()> {
    #[cfg(unix)]
    {
        // Bypassed when keron is invoked as the elevated child:
        // that path never reaches `run_with_io`; it routes through
        // `run_elevated_child` in keron-cli's hidden subcommand.
        let euid = unix_effective_uid();
        let sudo_uid_set = std::env::var_os("SUDO_UID").is_some();
        if euid == 0 && std::env::var_os("KERON_ELEVATED_CHILD").is_none() {
            anyhow::bail!(
                "keron should not be invoked under elevated rights directly. \
                 Run as your normal user; keron will prompt for elevation \
                 only for the resources that need it.{}",
                if sudo_uid_set {
                    " (Detected SUDO_UID; please re-run without `sudo`.)"
                } else {
                    ""
                },
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn unix_effective_uid() -> u32 {
    // The current process's euid drives elevation detection. We
    // can't use `geteuid()` from `libc` (not a workspace dep) and
    // `MetadataExt::uid()` reports *file* uid not process. Stat
    // `/proc/self` on Linux or fall back to a temp file owned by
    // the current process. Simplest portable trick: create a file
    // and read its uid back — created files inherit the effective
    // uid. The temp file is removed immediately.
    use std::os::unix::fs::MetadataExt;
    let probe = std::env::temp_dir().join(format!(
        ".keron-euid-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ));
    let Ok(f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    else {
        // Probe failed; conservatively assume non-root so we don't
        // refuse to run on weird hosts. The elevated path is
        // gated separately by the elevator probe.
        return 1;
    };
    let uid = f.metadata().map_or(1, |m| m.uid());
    drop(f);
    let _ = std::fs::remove_file(&probe);
    uid
}

/// Resolve the canonical "keron root" for the run — the path the user
/// passed to `keron apply`, surfaced to user code via `keron_root()`.
/// For a directory, that's the directory itself. For a single-file
/// invocation we use the file's parent, so the value is always a
/// directory regardless of what the CLI received.
fn keron_root_for(path: &Path, source: &load::LoadedSource) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| anyhow::anyhow!("canonicalizing `{}`: {e}", path.display()))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        // Single-file case: `LoadedFile.path` is already canonical, so
        // its parent is the directory the file lives in.
        Ok(source
            .files
            .first()
            .and_then(|f| f.path.parent())
            .map_or(canonical, Path::to_path_buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
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
    fn keron_root_for_returns_dir_when_path_is_dir() {
        // Directory inputs are taken as-is (canonicalized). This is
        // the common case: a user passes a folder of `.keron` files,
        // and `keron_root()` inside the manifests resolves to it.
        let proj = TempProject::new("keron-root-dir");
        // Drop a single `.keron` so `load::load` succeeds, then
        // verify keron_root_for picks the directory.
        proj.write("entry.keron", "");
        let source = load::load(&proj.root).unwrap();
        let root = keron_root_for(&proj.root, &source).unwrap();
        assert_eq!(root, fs::canonicalize(&proj.root).unwrap());
    }

    #[test]
    fn keron_root_for_returns_parent_when_path_is_file() {
        // Single-file invocation: `keron_root` is the parent of the
        // file, not the file itself. The eval path needs a directory
        // (it's the resolution base for relative imports / template
        // paths), so the helper normalizes both shapes.
        let proj = TempProject::new("keron-root-file");
        let entry = proj.write("entry.keron", "");
        let source = load::load(&entry).unwrap();
        let root = keron_root_for(&entry, &source).unwrap();
        assert_eq!(root, fs::canonicalize(&proj.root).unwrap());
    }

    #[test]
    fn keron_root_for_errors_when_path_is_missing() {
        // Canonicalize fails for a non-existent path; the error
        // chain must include the path so the diagnostic is locatable.
        let missing = PathBuf::from("/no/such/keron-root-test-path");
        let source = load::LoadedSource { files: vec![] };
        let err = keron_root_for(&missing, &source).expect_err("missing path should fail");
        assert!(
            format!("{err:#}").contains("/no/such/keron-root-test-path"),
            "error should include the missing path: {err:#}",
        );
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
    fn run_with_execute_and_approval_invokes_executor_for_unsupported_kind() {
        // "yes" input → `approved=true`, control reaches the executor.
        // Templates are still stubbed, so the executor surfaces a
        // "not yet implemented" diagnostic. This pins both halves of
        // the wiring: confirmation flips control past the cancel
        // branch, and the executor's per-kind error is propagated.
        // We target a path inside the test's temp dir so the
        // elevation pre-check classifies it as unprivileged (and we
        // hit the in-process executor, not the elevated re-exec).
        let proj = TempProject::new("yes-approval");
        let dest = proj.root.join("out");
        let src = format!(
            "reconcile template(path = \"{}\", source = \"tmpl.tpl\", vars = {{\"body\": \"y\"}})\n",
            dest.display(),
        );
        let entry = proj.write("entry.keron", &src);
        let (res, out) = drive(&entry, true, "yes\n");
        let err = res.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not yet implemented") && msg.contains("template"),
            "expected executor error naming the kind, got: {msg}",
        );
        // The diff should still be rendered before the executor fired.
        assert!(out.contains("will be created"), "diff missing: {out}");
        // No "Apply cancelled" because approved was true.
        assert!(!out.contains("Apply cancelled"), "got: {out}");
    }

    #[cfg(unix)]
    #[test]
    fn run_with_execute_creates_symlink_on_disk_end_to_end() {
        // End-to-end: parse → check → plan → diff → confirm → apply.
        // The executor walks a real `.keron` source through to a
        // working symlink on disk. Pins the CLI <-> apply wiring for
        // the first executor-supported resource kind.
        let proj = TempProject::new("symlink-e2e");
        let target = proj.root.join("target");
        fs::write(&target, "payload").unwrap();
        let link = proj.root.join("alias");
        let src = format!(
            "reconcile symlink(from = \"{}\", to = \"{}\")\n",
            link.display(),
            target.display(),
        );
        let entry = proj.write("entry.keron", &src);

        let (res, out) = drive(&entry, true, "yes\n");
        res.expect("symlink apply should succeed");
        assert!(out.contains("will be created"), "missing diff: {out}");
        assert!(
            out.contains("Apply complete"),
            "missing apply summary: {out}"
        );
        assert!(out.contains("1 added"), "summary should report add: {out}");
        // The eval-side `resolve_managed_path` canonicalizes the
        // user-supplied `to`, so the link points at the canonical
        // target — compare via `canonicalize` so the assertion holds
        // on platforms whose temp dir is itself a symlink (macOS:
        // `/var/folders/...` -> `/private/var/folders/...`).
        let resolved = fs::canonicalize(&link).expect("symlink not created");
        let expected = fs::canonicalize(&target).unwrap();
        assert_eq!(resolved, expected);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_execute_is_idempotent_for_symlinks() {
        // Second apply on an already-correct symlink hits the NoOp
        // path in the planner — no diff, no prompt, no executor side
        // effects. Mirrors what an end user sees on the second `keron
        // apply` of the same manifest.
        let proj = TempProject::new("symlink-idempotent");
        let target = proj.root.join("target");
        fs::write(&target, "payload").unwrap();
        let link = proj.root.join("alias");
        // Pre-existing correct symlink (same target the manifest wants).
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let src = format!(
            "reconcile symlink(from = \"{}\", to = \"{}\")\n",
            link.display(),
            target.display(),
        );
        let entry = proj.write("entry.keron", &src);

        let (res, out) = drive(&entry, true, "yes\n");
        res.unwrap();
        // Empty plan → "No changes." message, early return before
        // prompt, no Apply summary line.
        assert!(
            out.contains("No changes"),
            "expected idempotent output, got: {out}"
        );
        assert!(
            !out.contains("Apply complete"),
            "executor should not run: {out}"
        );
    }
}
