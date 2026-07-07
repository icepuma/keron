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

mod capability;
mod confirm;
mod diff;
mod elevated;
mod eval;
mod execute;
mod load;
mod packages;
mod plan;
mod platform;
mod report;
mod terminal_safe;

pub use load::collect_keron_paths;
pub use terminal_safe::sanitize_terminal_message;

pub use elevated::child::run as run_elevated_child;
pub use elevated::payload::{PayloadExpectation, PayloadIdentity};

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use keron_modules::{EntrySource, ModuleId, resolve};
use thiserror::Error;

use crate::diff::RenderOptions;

/// Category of a non-cancel failure from [`run`].
///
/// The CLI binary maps each variant to a distinct exit code so a CI
/// script can act on the category. The variants intentionally only
/// distinguish phases the user can do something different about:
/// "fix your manifest" (pre-apply) vs "the apply itself failed"
/// (apply) vs "the elevated re-exec failed" (elevation).
///
/// Inner type stays `anyhow::Error` so existing `.context(...)`
/// chains thread through unmodified.
#[derive(Debug, Error)]
pub enum RunError {
    /// Refused to run as root / Administrator directly.
    #[error("{0}")]
    DirectElevation(#[source] anyhow::Error),
    /// Module loading, parsing, type-checking, or plan-building
    /// failed — i.e. anything before the executor.
    #[error("{0}")]
    PreApply(#[source] anyhow::Error),
    /// The unprivileged executor or one of its IO calls failed.
    #[error("{0}")]
    Apply(#[source] anyhow::Error),
    /// The elevated re-exec failed: missing elevator, password
    /// denied, child crashed, partial chown failures.
    #[error("{0}")]
    Elevation(#[source] anyhow::Error),
    /// stdin/stdout failure while rendering the diff or running a
    /// confirmation prompt.
    #[error("{0}")]
    Io(#[source] io::Error),
}

impl RunError {
    /// Stable exit-code mapping. Distinct codes let CI scripts
    /// distinguish "fix your manifest" from "the apply broke" from
    /// "elevation refused/failed". 130 is reserved for
    /// `Outcome::Cancelled` (SIGINT convention).
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::DirectElevation(_) => 4,
            Self::PreApply(_) => 2,
            Self::Apply(_) => 3,
            Self::Elevation(_) => 5,
            Self::Io(_) => 1,
        }
    }
}

/// Result of a [`run`] invocation.
///
/// Carries the distinction between "we did the requested work" and
/// "the user declined at the confirmation prompt", so the CLI
/// binary can exit with a code scripts can act on (`Cancelled` →
/// exit 130, matching the SIGINT convention; everything else →
/// exit 0). Without this, a scripted
/// `keron apply --execute && deploy` would run `deploy` even when
/// the operator answered "no".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The plan was rendered, the prompts (if any) were answered
    /// affirmatively, and any requested execution completed.
    Applied,
    /// The user declined at the value prompt or the force prompt.
    Cancelled,
}

/// Per-run flags bundled into a single argument so callers don't
/// have to thread three bools through the public and test-only entry
/// points. Grouped semantically — every field is set once at process
/// boundary and never mutated.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunFlags {
    /// User passed `--execute`: prompt and apply after planning.
    pub execute: bool,
    /// Stdout is a terminal — paint ANSI escapes into the diff.
    pub color: bool,
    /// User passed `--verbose-will-reveal-sensitive-content`: render
    /// body fields as full unified diffs from the start. When false,
    /// the renderer emits the one-line `N lines added / M lines
    /// removed` summary and prints a single footer pointing at the
    /// flag — there is no interactive prompt.
    pub verbose: bool,
}

/// Plan a keron program at `path`. With `execute`, prompt and apply.
///
/// `verbose` is the CLI's
/// `--verbose-will-reveal-sensitive-content` flag — when true, body
/// fields (template `content`, shell `script`) render as full
/// unified diffs from the start. When false, the renderer emits the
/// one-line `N lines added / M lines removed` summary and adds a
/// single footer hint pointing at the flag.
///
/// # Errors
/// Returns a [`RunError`] tagged with the failure phase so the CLI
/// can map each phase to a distinct exit code (see
/// [`RunError::exit_code`]).
pub fn run(path: &Path, execute: bool, verbose: bool) -> std::result::Result<Outcome, RunError> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    // Honor `NO_COLOR` (https://no-color.org) like the `format`
    // subcommand does, in addition to requiring a tty.
    let color = stdout.is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let mut sin = stdin.lock();
    let mut sout = stdout.lock();
    let flags = RunFlags {
        execute,
        color,
        verbose,
    };
    run_with_io(path, &mut sin, &mut sout, flags)
}

/// Parse, resolve, and type-check `path` without building a plan.
///
/// This is the cheap, side-effect-free validation path for CI and
/// editor integration: unlike [`run`] it never reaches
/// `build_prechecked_plan`, so it does not resolve `secret(...)` URIs
/// against `op`/`bw`/`infisical` or probe package managers. It catches
/// every parse, module-resolution, and type error.
///
/// # Errors
/// Returns [`RunError::PreApply`] when loading, module resolution, or
/// type checking fails.
pub fn check(path: &Path) -> std::result::Result<(), RunError> {
    let source = load::load(path).map_err(RunError::PreApply)?;
    let roots = entry_sources(source);
    resolve(roots).map_err(|bundle| {
        RunError::PreApply(anyhow::anyhow!(
            "module resolution failed:\n{}",
            report::render(&bundle, false)
        ))
    })?;
    Ok(())
}

/// Build the resolver's per-file [`EntrySource`] list from a loaded
/// source tree. Shared by [`run_with_io`] and [`check`].
fn entry_sources(source: load::LoadedSource) -> Vec<EntrySource> {
    source
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
                id: ModuleId(f.path),
            }
        })
        .collect()
}

/// Test-friendly entry: same logic as [`run`] but with explicit IO so
/// tests can drive the apply confirmation prompts without touching
/// real stdio. The public `run` wrapper feeds in real
/// `stdin`/`stdout` and detects terminal-ness for color.
///
/// `#[allow(too_many_lines)]` is intentional — this is the apply-pipeline
/// orchestrator, and the natural read order (load → resolve → plan →
/// render → execute) makes a serial implementation more legible than a
/// graph of micro-helpers.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_with_io<R, W>(
    path: &Path,
    stdin: &mut R,
    stdout: &mut W,
    flags: RunFlags,
) -> std::result::Result<Outcome, RunError>
where
    R: BufRead,
    W: Write,
{
    let RunFlags {
        execute,
        color,
        verbose,
    } = flags;
    refuse_direct_elevation().map_err(RunError::DirectElevation)?;

    let source = load::load(path).map_err(RunError::PreApply)?;
    let keron_root = keron_root_for(path, &source).map_err(RunError::PreApply)?;
    let roots = entry_sources(source);

    let graph = resolve(roots).map_err(|bundle| {
        // Render the diagnostic report WITHOUT color. This string is
        // wrapped in an error chain that the CLI later runs through
        // `sanitize_terminal_message`, which escapes every ESC byte to a
        // literal `\u{001b}` — so baking ANSI color in here (as
        // `color` would on an interactive run) turns every syntax /
        // type / module error into escaped-ANSI soup. Plain text renders
        // cleanly through the sanitizer; the source snippet's
        // manifest-derived content still gets neutralized.
        RunError::PreApply(anyhow::anyhow!(
            "module resolution failed:\n{}",
            report::render(&bundle, false)
        ))
    })?;

    let prechecked =
        plan::build_prechecked_plan(&graph, &keron_root).map_err(RunError::PreApply)?;
    let plan = prechecked.plan;
    let precheck = prechecked.precheck;

    // Render once in the operator-requested mode. Default mode hides
    // template `content` / shell `script` bodies as a `lines added /
    // lines removed` summary; verbose mode (opt-in via the
    // intentionally long `--verbose-will-reveal-sensitive-content`
    // flag) prints the full unified diff. The flag's name carries the
    // consent — the renderer does not redact in verbose mode.
    diff::render_plan(stdout, &plan, RenderOptions { color, verbose }).map_err(RunError::Io)?;
    render_precheck(stdout, &precheck).map_err(RunError::Io)?;

    // Warn when an unprivileged shell hook is declared after an elevated
    // resource: the elevation partition runs it before the elevated
    // child, so declaration order is not honored and a hook depending on
    // that earlier resource can never converge. Names are file paths /
    // shell resource names, already sanitized by the plan.
    for address in plan.elevation_ordering_hazards() {
        writeln!(
            stdout,
            "warning: shell `{address}` is declared after an elevated resource but runs before it \
             (unprivileged changes apply before the elevated phase); if it depends on that \
             resource, reorder so the shell comes first or make the resource unprivileged."
        )
        .map_err(RunError::Io)?;
    }

    if !execute {
        return Ok(Outcome::Applied);
    }

    // Nothing to apply short-circuits *before* any prompt. When the plan
    // is empty but a precheck failed (e.g. every winget package on this
    // OS was filtered out), continuing would do nothing, so asking the
    // operator to confirm-continue-past-the-precheck is pure noise — the
    // precheck was already rendered above for information.
    if plan.is_empty() {
        return Ok(Outcome::Applied);
    }

    if !precheck.is_empty() {
        let approved = confirm::prompt_precheck_continue(stdin, stdout).map_err(RunError::Io)?;
        if !approved {
            writeln!(stdout, "Apply cancelled.").map_err(RunError::Io)?;
            return Ok(Outcome::Cancelled);
        }
    }

    let approved = confirm::prompt_yes_no(stdin, stdout).map_err(RunError::Io)?;
    if !approved {
        writeln!(stdout, "Apply cancelled.").map_err(RunError::Io)?;
        return Ok(Outcome::Cancelled);
    }

    let summary = plan.summary();
    if summary.force > 0 {
        let approved = confirm::prompt_force(stdin, stdout, summary.force).map_err(RunError::Io)?;
        if !approved {
            writeln!(stdout, "Apply cancelled.").map_err(RunError::Io)?;
            return Ok(Outcome::Cancelled);
        }
    }

    let (unprivileged, elevated_plan) = plan.partition_by_elevation();
    let mut unpriv_summary = execute::execute(&unprivileged).map_err(RunError::Apply)?;

    // Warnings collected during the unprivileged apply (e.g. a managed
    // tap that registered but couldn't be trusted). The elevated child
    // never produces these — no warning-yielding resource is elevated —
    // so the unprivileged summary is the sole source. Spill them after
    // the status line so the user still sees "Apply complete!" first.
    let mut warnings = std::mem::take(&mut unpriv_summary.warnings);

    if elevated_plan.changes.is_empty() {
        writeln!(
            stdout,
            "Apply complete! Resources: {} added, {} changed, {} ran.",
            unpriv_summary.added, unpriv_summary.changed, unpriv_summary.ran
        )
        .map_err(RunError::Io)?;
    } else {
        writeln!(
            stdout,
            "Unprivileged phase complete. Resources: {} added, {} changed, {} ran.",
            unpriv_summary.added, unpriv_summary.changed, unpriv_summary.ran
        )
        .map_err(RunError::Io)?;
        writeln!(
            stdout,
            "{} resource(s) require elevated rights; you may be asked for your password.",
            elevated_plan.changes.len(),
        )
        .map_err(RunError::Io)?;
        let elevated_summary =
            elevated::run_elevated(&elevated_plan).map_err(RunError::Elevation)?;
        warnings.extend(elevated_summary.warnings);
        writeln!(
            stdout,
            "Apply complete! Resources: {} added, {} changed, {} ran.",
            unpriv_summary.added + elevated_summary.added,
            unpriv_summary.changed + elevated_summary.changed,
            unpriv_summary.ran + elevated_summary.ran
        )
        .map_err(RunError::Io)?;
    }
    spill_warnings(stdout, &warnings).map_err(RunError::Io)?;
    Ok(Outcome::Applied)
}

/// Render collected apply warnings to the user, one indented line each.
/// Empty input writes nothing so a clean run stays quiet.
///
/// A `Warning` may embed a package manager's captured `stderr` (e.g. a
/// failed `brew trust`), which is subprocess-controlled and can carry
/// `\r`/ESC sequences. Sanitize before printing so it can't redraw or
/// forge the surrounding output — this is the one `run_with_io` sink
/// that carries external bytes raw otherwise.
fn spill_warnings<W: Write>(stdout: &mut W, warnings: &[execute::Warning]) -> io::Result<()> {
    for warning in warnings {
        writeln!(
            stdout,
            "warning: {}",
            terminal_safe::sanitize_terminal_message(&warning.to_string())
        )?;
    }
    Ok(())
}

fn render_precheck<W: Write>(stdout: &mut W, precheck: &plan::Precheck) -> io::Result<()> {
    if precheck.is_empty() {
        return Ok(());
    }
    writeln!(stdout)?;
    writeln!(
        stdout,
        "Precheck: some package resources are not supported on this OS and will be skipped."
    )?;
    for pkg in &precheck.unsupported_packages {
        writeln!(
            stdout,
            "  - package.\"{}\" uses {} package `{}`, unsupported on {} (supported on: {})",
            terminal_safe::show_str(&pkg.address),
            pkg.manager.label(),
            terminal_safe::show_str(&pkg.name),
            pkg.os.label(),
            pkg.manager.supported_os_label(),
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
///
/// The elevated child (`keron __apply-elevated <payload>`) enters
/// through `run_elevated_child` and bypasses this guard by
/// construction — `main.rs` dispatches the two subcommands to
/// distinct entry points so the child never traverses
/// [`run_with_io`].
// `#[mutants::skip]` because this guard's "bail when running as
// root" path can only be reached when the test process itself is
// uid 0 — which CI test runners deliberately are not. The unprivileged
// fall-through path is exercised by every existing keron-apply test,
// but no test can flip euid to 0 without re-execing under sudo.
// Windows has no Unix-style euid check, so on that platform the body
// collapses to `Ok(())` and clippy flags it as eligible for `const fn`
// AND as `unnecessary_wraps`. The Unix path calls a non-const FFI
// helper and DOES need the Result to surface the elevation refusal,
// so allow both lints here rather than fork the signature per platform.
#[allow(clippy::missing_const_for_fn, clippy::unnecessary_wraps)]
#[cfg_attr(test, mutants::skip)]
fn refuse_direct_elevation() -> Result<()> {
    #[cfg(unix)]
    {
        let euid = unix_effective_uid();
        let sudo_uid_set = std::env::var_os("SUDO_UID").is_some();
        if euid == 0 {
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

// `#[mutants::skip]` because this is a one-line FFI wrapper around
// `libc::geteuid()`. A mutation `replace -> u32 with 1` would only be
// caught by a test that knows the host's actual euid — but CI test
// runners run as a non-root uid that varies by host (501 on macOS
// dev, 1000 on Linux CI), so any concrete-value assertion is brittle.
#[cfg(unix)]
#[cfg_attr(test, mutants::skip)]
fn unix_effective_uid() -> u32 {
    // Platform FFI for the elevated-rights guard: `geteuid` is the
    // authoritative source. Heuristic fallbacks (tempfile probes,
    // CWD stat) failed open on locked-down hosts.
    #[allow(unsafe_code)]
    unsafe {
        libc::geteuid()
    }
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

    /// Convert a Path to a string suitable for embedding in a keron
    /// manifest string literal. Keron's string syntax treats `\` as an
    /// escape introducer (`\n`, `\t`, …), so a real Windows path like
    /// `C:\Users\foo` would tokenise as `\U`, an invalid escape. The
    /// runtime accepts forward slashes on every supported OS, so the
    /// safe lowest-common-denominator is to swap `\` → `/`. No-op on
    /// Unix.
    fn manifest_path(p: &Path) -> String {
        p.display().to_string().replace('\\', "/")
    }

    /// Drives `run_with_io` and returns (result, captured stdout).
    fn drive(
        path: &Path,
        execute: bool,
        stdin: &str,
    ) -> (std::result::Result<Outcome, RunError>, String) {
        let mut sin = Cursor::new(stdin.as_bytes().to_vec());
        let mut sout: Vec<u8> = Vec::new();
        let res = run_with_io(
            path,
            &mut sin,
            &mut sout,
            RunFlags {
                execute,
                color: false,
                verbose: false,
            },
        );
        (res, String::from_utf8(sout).unwrap())
    }

    #[test]
    fn module_resolution_error_carries_no_ansi_even_on_color_run() {
        // Regression for the "ANSI soup" bug: a type/module error
        // rendered with color baked in is later escaped to literal
        // `\u{001b}` by the CLI sanitizer, turning every interactive
        // error into garbage. With color flowing in as `true`, the
        // error string must still contain no raw ESC byte.
        let proj = TempProject::new("ansi-soup");
        let entry = proj.write("entry.keron", "val x: Int = \"nope\"\n");
        let mut sin = Cursor::new(Vec::new());
        let mut sout: Vec<u8> = Vec::new();
        let err = run_with_io(
            &entry,
            &mut sin,
            &mut sout,
            RunFlags {
                execute: false,
                color: true,
                verbose: false,
            },
        )
        .expect_err("type error must fail");
        let msg = format!("{err:#}");
        assert!(
            !msg.contains('\u{001b}'),
            "error must not carry raw ANSI: {msg:?}"
        );
        // Sanity: it really is the diagnostic, not some other failure.
        assert!(msg.contains("module resolution failed"), "got: {msg}");
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
        let err = res.expect_err("missing entry should fail");
        // A missing entry is a pre-apply failure (load failed) and
        // must map to exit code 2 so CI can act on the category.
        assert!(
            matches!(err, RunError::PreApply(_)),
            "expected PreApply category, got: {err:?}"
        );
        assert_eq!(err.exit_code(), 2, "exit code for {err:?}");
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
        let res = run(&missing, false, false);
        assert!(res.is_err(), "expected Err for missing path");
    }

    #[test]
    fn run_renders_diff_and_returns_when_not_executing() {
        let proj = TempProject::new("not-execute");
        let entry = proj.write(
            "entry.keron",
            "reconcile template(source = \"tmpl.tpl\", target = \"/x\", vars = {\"body\": \"y\"})\n",
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
            "val f: Template = template(source = \"tmpl.tpl\", target = \"/x\", vars = {\"body\": \"\"})\n",
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
    fn run_plan_only_renders_unsupported_package_precheck_without_prompt() {
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Linux);
        let proj = TempProject::new("precheck-plan-only");
        let entry = proj.write(
            "entry.keron",
            "reconcile winget(\"Microsoft.PowerShell\")\n",
        );
        let (res, out) = drive(&entry, false, "");
        res.unwrap();
        assert!(out.contains("Precheck"), "missing precheck: {out}");
        assert!(
            out.contains("winget:Microsoft.PowerShell"),
            "missing skipped package: {out}",
        );
        assert!(
            !out.contains("Do you still want to proceed"),
            "plan-only run should not prompt: {out}",
        );
    }

    #[test]
    fn run_with_execute_cancels_when_unsupported_package_precheck_declined() {
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Linux);
        let proj = TempProject::new("precheck-no");
        let dest = proj.root.join("out");
        let src = format!(
            "reconcile {{\n\
             winget(\"Microsoft.PowerShell\")\n\
             template(source = \"tmpl.tpl\", target = \"{}\", vars = {{\"body\": \"y\"}})\n\
             }}\n",
            manifest_path(&dest),
        );
        let entry = proj.write("entry.keron", &src);
        let (res, out) = drive(&entry, true, "no\n");
        let outcome = res.unwrap();
        assert_eq!(outcome, Outcome::Cancelled);
        assert!(
            out.contains("Do you still want to proceed"),
            "missing precheck prompt: {out}",
        );
        assert!(
            !out.contains("Do you want to perform these actions"),
            "normal apply prompt should not run after precheck decline: {out}",
        );
        assert!(!dest.exists(), "template should not be written");
    }

    #[test]
    fn run_with_execute_skips_unsupported_packages_after_precheck_approval() {
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Linux);
        let proj = TempProject::new("precheck-yes");
        let dest = proj.root.join("out");
        let src = format!(
            "reconcile {{\n\
             winget(\"Microsoft.PowerShell\")\n\
             template(source = \"tmpl.tpl\", target = \"{}\", vars = {{\"body\": \"y\"}})\n\
             }}\n",
            manifest_path(&dest),
        );
        let entry = proj.write("entry.keron", &src);
        let (res, out) = drive(&entry, true, "yes\nyes\n");
        res.expect("supported template should apply");
        assert!(
            out.contains("winget:Microsoft.PowerShell"),
            "missing skipped package: {out}",
        );
        assert!(
            out.contains("1 added"),
            "summary should count template only: {out}"
        );
        assert_eq!(fs::read_to_string(dest).unwrap(), "y");
    }

    #[test]
    fn run_with_execute_and_no_approval_prints_cancelled() {
        let proj = TempProject::new("no-approval");
        let entry = proj.write(
            "entry.keron",
            "reconcile template(source = \"tmpl.tpl\", target = \"/x\", vars = {\"body\": \"y\"})\n",
        );
        let (res, out) = drive(&entry, true, "no\n");
        let outcome = res.unwrap();
        assert_eq!(
            outcome,
            Outcome::Cancelled,
            "user-no should surface as Cancelled so the CLI can exit 130"
        );
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
            "reconcile template(source = \"tmpl.tpl\", target = \"{}\", vars = {{\"body\": \"new\"}})\n",
            manifest_path(&dest),
        );
        let entry = proj.write("entry.keron", &src);
        // First "yes" approves the value prompt; "no" cancels the
        // follow-up force prompt. (Pre-fix, an empty second line
        // would EOF-cancel; that path now errors loudly, so we feed
        // an explicit "no".)
        let (res, out) = drive(&entry, true, "yes\nno\n");
        let outcome = res.unwrap();
        assert_eq!(
            outcome,
            Outcome::Cancelled,
            "force-prompt no should also surface as Cancelled"
        );
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
            "reconcile template(source = \"tmpl.tpl\", target = \"{}\", vars = {{\"body\": \"y\"}})\n",
            manifest_path(&dest),
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
            "reconcile symlink(source = \"{}\", target = \"{}\")\n",
            manifest_path(&target),
            manifest_path(&link),
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
            "reconcile symlink(source = \"{}\", target = \"{}\")\n",
            manifest_path(&target),
            manifest_path(&link),
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
