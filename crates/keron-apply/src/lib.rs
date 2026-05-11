//! keron-apply: plan and execute resources produced by keron-lang.
//!
//! Public surface is a single `run` function called by the CLI. It
//! loads the input (a single `.keron` file or a directory of them),
//! parses + type-checks via `keron-lang`, builds a `Plan`, renders an
//! OpenTofu-style diff, and — when `execute` is set — prompts the
//! user before applying.
//!
//! The executor wires up symlinks, templates, and packages end-to-end
//! today. The planner diffs desired resources against live filesystem
//! state, but it does not infer removals from absence because keron
//! has no persisted ownership state.

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

    let summary = plan.summary();
    if summary.force > 0 {
        let approved = confirm::prompt_force(stdin, stdout, summary.force)?;
        if !approved {
            writeln!(stdout, "Apply cancelled.")?;
            return Ok(());
        }
    }

    let (unprivileged, elevated_plan) = plan.partition_by_elevation();
    let unpriv_summary = execute::execute(&unprivileged)?;

    if elevated_plan.changes.is_empty() {
        writeln!(
            stdout,
            "Apply complete! Resources: {} added, {} changed.",
            unpriv_summary.added, unpriv_summary.changed
        )?;
    } else {
        writeln!(
            stdout,
            "Unprivileged phase complete. Resources: {} added, {} changed.",
            unpriv_summary.added, unpriv_summary.changed
        )?;
        writeln!(
            stdout,
            "{} resource(s) require elevated rights; you may be asked for your password.",
            elevated_plan.changes.len(),
        )?;
        let elevated_summary = elevated::run_elevated(&elevated_plan)?;
        writeln!(
            stdout,
            "Apply complete! Resources: {} added, {} changed.",
            unpriv_summary.added + elevated_summary.added,
            unpriv_summary.changed + elevated_summary.changed
        )?;
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
    // No libc in the workspace deps and `MetadataExt::uid()` reports
    // *file* uid not process — so create a temp file and read its uid
    // back; created files inherit the effective uid.
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
        // Probe failed: conservatively assume non-root so we don't
        // refuse to run on weird hosts.
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
            fs::write(root.join("tmpl.tpl"), "{{ body }}").unwrap();
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
        let proj = TempProject::new("keron-root-dir");
        proj.write("entry.keron", "");
        let source = load::load(&proj.root).unwrap();
        let root = keron_root_for(&proj.root, &source).unwrap();
        assert_eq!(root, fs::canonicalize(&proj.root).unwrap());
    }

    #[test]
    fn keron_root_for_returns_parent_when_path_is_file() {
        let proj = TempProject::new("keron-root-file");
        let entry = proj.write("entry.keron", "");
        let source = load::load(&entry).unwrap();
        let root = keron_root_for(&entry, &source).unwrap();
        assert_eq!(root, fs::canonicalize(&proj.root).unwrap());
    }

    #[test]
    fn keron_root_for_errors_when_path_is_missing() {
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
        let missing = PathBuf::from("/no/such/keron-test-entry.keron");
        let (res, _) = drive(&missing, false, "");
        assert!(res.is_err());
    }

    #[test]
    fn public_run_wrapper_returns_err_for_missing_entry() {
        // Pins the `Ok(())` mutation on the thin `run` wrapper:
        // bailing before any stdin read happens lets us avoid real
        // terminal IO.
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
        // Mutating `||` to `&&` would push through to the prompt; we
        // assert the early-return signature instead of trusting that
        // `prompt_yes_no` would fail closed on empty stdin.
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
    fn run_with_execute_requires_force_for_updates() {
        let proj = TempProject::new("force-update");
        let dest = proj.root.join("out");
        fs::write(&dest, "old").unwrap();
        let src = format!(
            "reconcile template(path = \"{}\", source = \"tmpl.tpl\", vars = {{\"body\": \"new\"}})\n",
            dest.display(),
        );
        let entry = proj.write("entry.keron", &src);
        let (res, out) = drive(&entry, true, "yes\n");
        res.unwrap();
        assert!(out.contains("Only 'force'"), "force prompt missing: {out}");
        assert!(out.contains("Apply cancelled"), "cancel missing: {out}");
        assert_eq!(fs::read_to_string(dest).unwrap(), "old");
    }

    #[test]
    fn run_with_execute_and_approval_renders_template_to_disk() {
        // Temp-dir destination keeps the elevation pre-check at
        // "unprivileged" so we exercise the in-process executor, not
        // the elevated re-exec.
        let proj = TempProject::new("yes-approval");
        let dest = proj.root.join("out");
        let src = format!(
            "reconcile template(path = \"{}\", source = \"tmpl.tpl\", vars = {{\"body\": \"y\"}})\n",
            dest.display(),
        );
        let entry = proj.write("entry.keron", &src);
        let (res, out) = drive(&entry, true, "yes\n");
        res.expect("template apply should succeed");
        assert!(out.contains("will be created"), "diff missing: {out}");
        assert!(!out.contains("Apply cancelled"), "got: {out}");
        assert!(out.contains("1 added"), "summary missing: {out}");
        let content = fs::read_to_string(&dest).expect("template file written");
        assert_eq!(content, "y");
    }

    #[cfg(unix)]
    #[test]
    fn run_with_execute_creates_symlink_on_disk_end_to_end() {
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
        // macOS' `/var/folders/...` temp dir is itself a symlink to
        // `/private/var/...`; canonicalize both sides so the assertion
        // doesn't depend on that.
        let resolved = fs::canonicalize(&link).expect("symlink not created");
        let expected = fs::canonicalize(&target).unwrap();
        assert_eq!(resolved, expected);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_execute_is_idempotent_for_symlinks() {
        let proj = TempProject::new("symlink-idempotent");
        let target = proj.root.join("target");
        fs::write(&target, "payload").unwrap();
        let link = proj.root.join("alias");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let src = format!(
            "reconcile symlink(from = \"{}\", to = \"{}\")\n",
            link.display(),
            target.display(),
        );
        let entry = proj.write("entry.keron", &src);

        let (res, out) = drive(&entry, true, "yes\n");
        res.unwrap();
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
