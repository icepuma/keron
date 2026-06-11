//! `keron` — single-binary entry point. Subcommands are wired here;
//! the heavy lifting lives in the library crates.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "keron",
    version,
    about = "keron: user-level dotfile + package manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Plan changes for a keron program, and optionally apply them.
    ///
    /// Without `--execute`, prints an OpenTofu-style diff and exits.
    /// With `--execute`, prints the diff, prompts for confirmation,
    /// and (on `yes`) runs the executor.
    Apply {
        /// Path to a `.keron` file or a directory containing
        /// `.keron` files (loaded in sorted order).
        path: PathBuf,

        /// After showing the plan, prompt for confirmation and apply.
        #[arg(long)]
        execute: bool,

        /// Show full content diffs in the plan. WARNING: this will
        /// print sensitive values (private keys, tokens, secrets)
        /// verbatim to stdout. Shell scripts are always shown because
        /// they are executable code; this flag controls template and
        /// key material. The flag name is deliberately long so it
        /// cannot be confused with a generic `--verbose` — typing it
        /// is the consent. When omitted, hidden body fields render as
        /// a `lines added / lines removed` summary and a footer points
        /// at this flag.
        #[arg(long)]
        verbose_will_reveal_sensitive_content: bool,
    },

    /// Validate a keron program without planning or applying.
    ///
    /// Parses, resolves imports, and type-checks. Unlike `apply`, it
    /// never resolves `secret(...)` URIs or probes package managers, so
    /// it is the cheap, side-effect-free path for CI and editor
    /// integration. Exits 0 when valid, non-zero on any parse, module,
    /// or type error.
    Check {
        /// Path to a `.keron` file or a directory containing
        /// `.keron` files (loaded in sorted order).
        path: PathBuf,
    },

    /// Normalize `.keron` files. Writes in place by default; in
    /// `--check` mode prints a unified diff per file that would
    /// change.
    Format {
        /// One or more `.keron` files or directories. When empty,
        /// reads source from stdin and writes formatted output to
        /// stdout. A single `-` is treated identically to no paths.
        paths: Vec<PathBuf>,

        /// Print a unified diff per file that would change and exit
        /// with status 2; don't modify files on disk.
        #[arg(long)]
        check: bool,

        /// Suppress per-file informational output ("Formatted …").
        /// Errors still print to stderr, and `--check` still prints
        /// its diff — that's the requested artifact, not chatter.
        #[arg(long, short)]
        quiet: bool,
    },

    /// Internal: invoked by the unprivileged keron process under
    /// sudo / `ShellExecuteExW` to apply the subset of a plan that
    /// requires elevated rights. Reads the work payload from the
    /// path argument and chowns each created path back to the
    /// calling user. Not part of the public CLI surface.
    #[command(name = "__apply-elevated", hide = true)]
    ApplyElevated {
        /// Path to the JSON payload written by the unprivileged
        /// parent process.
        payload: PathBuf,

        /// Expected SHA-256 digest of the payload bytes.
        digest: String,

        /// Expected payload file metadata captured by the parent.
        identity: String,
    },
}

/// Bin entry. `#[mutants::skip]` because covering the `Ok(())` body
/// mutation would require spawning the compiled binary; the
/// dispatch logic is exhaustively covered via [`run_cli`] below.
#[cfg_attr(test, mutants::skip)]
fn main() -> ExitCode {
    match run_cli(std::env::args_os()) {
        Ok(code) => code,
        Err(CliError { error, exit_code }) => {
            // Route through `sanitize_terminal_message` so a hostile
            // `.keron` manifest that embedded `\r` or `\x1b[A` in a
            // path / address can't forge the rendered error chain
            // — same threat as the diff renderer, different sink.
            let raw = format_cli_error(&error);
            eprintln!("{}", keron_apply::sanitize_terminal_message(&raw));
            ExitCode::from(exit_code)
        }
    }
}

/// `RunError`'s Display delegates to its wrapped `anyhow::Error` (see
/// `#[error("{0}")]`), but the same anyhow is also exposed as
/// `#[source]`. anyhow's Debug walks both, so the user sees the
/// top-level message twice — once as `Error:` and once under
/// `Caused by:`. Skip the wrapper by formatting the inner anyhow
/// directly; for non-`RunError` anyhow values (e.g. from `run_format`),
/// fall back to the standard chain-walking Debug.
fn format_cli_error(error: &anyhow::Error) -> String {
    error.downcast_ref::<keron_apply::RunError>().map_or_else(
        || format!("Error: {error:?}"),
        |run| match run {
            keron_apply::RunError::Io(io) => format!("Error: {io}"),
            keron_apply::RunError::DirectElevation(a)
            | keron_apply::RunError::PreApply(a)
            | keron_apply::RunError::Apply(a)
            | keron_apply::RunError::Elevation(a) => format!("Error: {a:?}"),
        },
    )
}

/// CLI failure: the source chain to render plus the exit code to
/// return. `run_cli` constructs one of these for each subcommand's
/// failure path so `main` doesn't have to downcast.
struct CliError {
    error: anyhow::Error,
    exit_code: u8,
}

impl<E> From<E> for CliError
where
    E: Into<anyhow::Error>,
{
    fn from(e: E) -> Self {
        Self {
            error: e.into(),
            exit_code: 1,
        }
    }
}

impl std::fmt::Debug for CliError {
    // `#[mutants::skip]` because this Debug impl is only invoked by
    // `expect_err`/`unwrap_err` panic paths and `{e:?}` formatting.
    // Tests use `format!("{err:#}")` (Display) for assertions; no
    // test inspects the Debug output of CliError, so a mutation
    // that returns `Ok(Default::default())` produces an empty Debug
    // string but no test observes it. Behavior covered by anyhow's
    // own Debug impl.
    #[cfg_attr(test, mutants::skip)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Delegate to the inner anyhow error so `{e:?}` /
        // `expect_err`'s panic message render the full source chain
        // rather than a useless `CliError { ... }`.
        std::fmt::Debug::fmt(&self.error, f)
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

/// Parse an argv vector and dispatch to the right subcommand.
/// Split out from `main` so unit tests can construct argv values
/// without spawning the binary — every mutant inside this function
/// is reachable from a test.
///
/// Returns the process exit code:
/// - `0` — normal run, including a plan-only invocation with no
///   execution requested.
/// - `1` — `keron format` I/O or parse failure (the catch-all).
/// - `2` — `format --check` drift, or pre-apply failure (module
///   resolution, type check, plan build).
/// - `3` — apply failed mid-run (executor).
/// - `4` — direct-elevation refusal (`sudo keron apply`).
/// - `5` — elevation phase failed (elevator missing, child crash,
///   ownership-fixup failure).
/// - `130` — user declined at an `apply --execute` confirmation
///   prompt (SIGINT convention).
fn run_cli<I, T>(args: I) -> std::result::Result<ExitCode, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::parse_from(args);
    match cli.command {
        Command::Apply {
            path,
            execute,
            verbose_will_reveal_sensitive_content,
        } => match keron_apply::run(&path, execute, verbose_will_reveal_sensitive_content) {
            Ok(keron_apply::Outcome::Applied) => Ok(ExitCode::SUCCESS),
            Ok(keron_apply::Outcome::Cancelled) => Ok(ExitCode::from(130)),
            Err(e) => {
                let exit_code = e.exit_code();
                Err(CliError {
                    error: anyhow::Error::from(e),
                    exit_code,
                })
            }
        },
        Command::Check { path } => match keron_apply::check(&path) {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(e) => {
                let exit_code = e.exit_code();
                Err(CliError {
                    error: anyhow::Error::from(e),
                    exit_code,
                })
            }
        },
        Command::Format {
            paths,
            check,
            quiet,
        } => {
            // Lock stdin/stdout *here*, not at the top of run_cli —
            // `Stdin::lock()` is a regular Mutex, not reentrant, so
            // holding it across the dispatch would deadlock `Apply`'s
            // own `stdin.lock()` in `keron_apply::run`.
            let stdin = io::stdin();
            let stdout = io::stdout();
            let target = resolve_format_target(paths)?;
            run_format(target, check, quiet, &mut stdin.lock(), &mut stdout.lock())
                .map_err(CliError::from)
        }
        Command::ApplyElevated {
            payload,
            digest,
            identity,
        } => {
            let expected = keron_apply::PayloadExpectation {
                digest_hex: digest,
                identity: keron_apply::PayloadIdentity::decode(&identity)?,
            };
            keron_apply::run_elevated_child(&payload, &expected)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[derive(Debug)]
enum FormatTarget {
    Stdin,
    Paths(Vec<PathBuf>),
}

/// Resolve the positional `paths` arg into a format target:
/// - `[]` or `[-]` → stdin
/// - anything containing `-` mixed with real paths → error
/// - otherwise → the path list
fn resolve_format_target(paths: Vec<PathBuf>) -> Result<FormatTarget, CliError> {
    let dash = std::path::Path::new("-");
    if paths.is_empty() {
        return Ok(FormatTarget::Stdin);
    }
    if paths.len() == 1 && paths[0] == dash {
        return Ok(FormatTarget::Stdin);
    }
    if paths.iter().any(|p| p == dash) {
        return Err(CliError {
            error: anyhow::anyhow!("`-` (stdin) cannot be mixed with file paths"),
            exit_code: 1,
        });
    }
    Ok(FormatTarget::Paths(paths))
}

fn run_format<R, W>(
    target: FormatTarget,
    check: bool,
    quiet: bool,
    stdin: &mut R,
    stdout: &mut W,
) -> anyhow::Result<ExitCode>
where
    R: io::Read,
    W: io::Write,
{
    match target {
        FormatTarget::Stdin => run_format_stdin(check, stdin, stdout),
        FormatTarget::Paths(paths) => run_format_paths(&paths, check, quiet, stdout),
    }
}

fn run_format_stdin<R, W>(check: bool, stdin: &mut R, stdout: &mut W) -> anyhow::Result<ExitCode>
where
    R: io::Read,
    W: io::Write,
{
    let mut before = String::new();
    stdin
        .read_to_string(&mut before)
        .map_err(|e| anyhow::anyhow!("reading stdin: {e}"))?;
    let after = match keron_lang::format(&before) {
        Ok(s) => s,
        Err(diags) => {
            let msg = diags
                .into_iter()
                .map(|d| d.message)
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!("cannot format stdin: {msg}");
        }
    };
    if check {
        if before == after {
            return Ok(ExitCode::SUCCESS);
        }
        let diff = render_diff("<stdin>", &before, &after, color_enabled());
        stdout
            .write_all(diff.as_bytes())
            .map_err(|e| anyhow::anyhow!("writing diff: {e}"))?;
        return Ok(ExitCode::from(2));
    }
    stdout
        .write_all(after.as_bytes())
        .map_err(|e| anyhow::anyhow!("writing stdout: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

fn run_format_paths<W>(
    paths: &[PathBuf],
    check: bool,
    quiet: bool,
    stdout: &mut W,
) -> anyhow::Result<ExitCode>
where
    W: io::Write,
{
    let mut files = Vec::new();
    for path in paths {
        let mut sub = collect_keron_files(path)?;
        files.append(&mut sub);
    }
    let mut drifted: Vec<(PathBuf, String, String)> = Vec::new();
    for file in files {
        let before = fs::read_to_string(&file)
            .map_err(|e| anyhow::anyhow!("reading `{}`: {e}", file.display()))?;
        let after = match keron_lang::format(&before) {
            Ok(s) => s,
            Err(diags) => {
                let msg = diags
                    .into_iter()
                    .map(|d| d.message)
                    .collect::<Vec<_>>()
                    .join("; ");
                anyhow::bail!("cannot format `{}`: {msg}", file.display());
            }
        };
        if before == after {
            continue;
        }
        if check {
            drifted.push((file, before, after));
        } else {
            write_atomically(&file, after.as_bytes())
                .map_err(|e| anyhow::anyhow!("writing `{}`: {e}", file.display()))?;
            if !quiet {
                let line = format!("Formatted {}\n", file.display());
                stdout
                    .write_all(keron_apply::sanitize_terminal_message(&line).as_bytes())
                    .map_err(|e| anyhow::anyhow!("writing stdout: {e}"))?;
            }
        }
    }
    if check && !drifted.is_empty() {
        let color = color_enabled();
        for (path, before, after) in &drifted {
            let label = path.display().to_string();
            let diff = render_diff(&label, before, after, color);
            stdout
                .write_all(diff.as_bytes())
                .map_err(|e| anyhow::anyhow!("writing diff: {e}"))?;
        }
        // Drift-reported, not a tool error: exit 2 so CI can
        // distinguish "rerun format" from "tool broke".
        return Ok(ExitCode::from(2));
    }
    Ok(ExitCode::SUCCESS)
}

/// Render a unified diff comparing `before` (the source as-found) to
/// `after` (the formatter's canonical output) with a `cargo fmt`-style
/// header. Sanitizes through `keron_apply::sanitize_terminal_message`
/// to defuse `\r` / `\x1b` / U+202E in either the path or the source
/// content before optionally applying ANSI color, so a hostile file
/// can't forge or rewrite the displayed output.
fn render_diff(label: &str, before: &str, after: &str, color: bool) -> String {
    let diff = similar::TextDiff::from_lines(before, after);
    let rendered = diff
        .unified_diff()
        .context_radius(3)
        .header(label, &format!("{label} (formatted)"))
        .to_string();
    let safe = keron_apply::sanitize_terminal_message(&rendered);
    if color { colorize_diff(&safe) } else { safe }
}

/// Apply ANSI colors to a unified-diff string. The structure follows
/// the universal `diff -u` convention recognized by `git`, `gofmt`,
/// `rustfmt`, and `prettier`:
/// - `---`/`+++` file headers: bold.
/// - `@@ ... @@` hunk headers: cyan.
/// - lines beginning with `-` (and not `---`): red.
/// - lines beginning with `+` (and not `+++`): green.
/// - context lines: untouched.
fn colorize_diff(diff: &str) -> String {
    const RESET: &str = "\x1b[0m";
    const RED: &str = "\x1b[31m";
    const GREEN: &str = "\x1b[32m";
    const CYAN: &str = "\x1b[36m";
    const BOLD: &str = "\x1b[1m";
    let mut out = String::with_capacity(diff.len() + 64);
    for line in diff.split_inclusive('\n') {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            out.push_str(BOLD);
            out.push_str(line.trim_end_matches('\n'));
            out.push_str(RESET);
            if line.ends_with('\n') {
                out.push('\n');
            }
        } else if line.starts_with("@@") {
            out.push_str(CYAN);
            out.push_str(line.trim_end_matches('\n'));
            out.push_str(RESET);
            if line.ends_with('\n') {
                out.push('\n');
            }
        } else if line.starts_with('-') {
            out.push_str(RED);
            out.push_str(line.trim_end_matches('\n'));
            out.push_str(RESET);
            if line.ends_with('\n') {
                out.push('\n');
            }
        } else if line.starts_with('+') {
            out.push_str(GREEN);
            out.push_str(line.trim_end_matches('\n'));
            out.push_str(RESET);
            if line.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Color is enabled when stdout is a real terminal AND the user
/// hasn't opted out via the de-facto `NO_COLOR` env var (honored by
/// rustfmt, cargo, ripgrep, prettier, ...).
///
/// The outer wrapper reads global state (`NO_COLOR`, `stdout().is_terminal()`)
/// so it cannot be unit-tested without a real PTY; the pure decision lives in
/// [`color_enabled_from`] which IS unit-tested. Skipped from mutation testing
/// because every reachable test runs without a TTY and would silently match
/// any `→ false` mutation.
#[cfg_attr(test, mutants::skip)]
fn color_enabled() -> bool {
    use std::io::IsTerminal;
    color_enabled_from(
        std::env::var_os("NO_COLOR").is_some(),
        io::stdout().is_terminal(),
    )
}

/// Pure decision: color iff stdout is a terminal AND `NO_COLOR` is unset.
const fn color_enabled_from(no_color_set: bool, stdout_is_tty: bool) -> bool {
    !no_color_set && stdout_is_tty
}

/// Write `bytes` to `target` via a sibling tempfile + rename. A
/// SIGINT or crash between fsync and rename leaves the original
/// file untouched, instead of a half-written truncation. The
/// tempfile name carries the pid and a per-call nanosecond suffix
/// so two concurrent `keron format` runs on the same dir can't
/// collide, and a [`TmpFileGuard`] drop-removes the tempfile on
/// every error path.
fn write_atomically(target: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = target.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file_name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp-{}-{nanos}", std::process::id()));
    let tmp = dir.join(tmp_name);
    let guard = TmpFileGuard::new(tmp.clone());
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, target)?;
    guard.disarm();
    Ok(())
}

/// Removes `path` on drop unless [`Self::disarm`] is called first.
/// Used by [`write_atomically`] so every failure path between the
/// `open(.tmp)` and the final `rename` cleans up the sibling temp.
struct TmpFileGuard {
    path: PathBuf,
    disarmed: bool,
}

impl TmpFileGuard {
    const fn new(path: PathBuf) -> Self {
        Self {
            path,
            disarmed: false,
        }
    }

    fn disarm(mut self) {
        self.disarmed = true;
    }
}

impl Drop for TmpFileGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn collect_keron_files(path: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> {
    let meta =
        fs::metadata(path).map_err(|e| anyhow::anyhow!("reading `{}`: {e}", path.display()))?;
    if meta.is_file() {
        if path.extension().and_then(|e| e.to_str()) != Some("keron") {
            anyhow::bail!("`{}` is not a .keron file", path.display());
        }
        return Ok(vec![path.to_path_buf()]);
    }
    if !meta.is_dir() {
        anyhow::bail!(
            "`{}` is neither a regular file nor a directory",
            path.display()
        );
    }
    let mut files = Vec::new();
    collect_keron_files_from_dir(path, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_keron_files_from_dir(dir: &std::path::Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_keron_files_from_dir(&path, out)?;
        } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("keron") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("keron-cli-{tag}-{}-{n}", std::process::id()));
            if path.exists() {
                fs::remove_dir_all(&path).ok();
            }
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn run_cli_apply_with_missing_path_errors() {
        let err = run_cli([
            "keron",
            "apply",
            "/no/such/keron-cli-test-missing-path.keron",
        ])
        .expect_err("missing path should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("/no/such/keron-cli-test-missing-path.keron"),
            "error should name the missing path: {msg}",
        );
        // Pre-apply failures map to exit code 2 so CI can distinguish
        // "fix the manifest" from "the apply itself broke" (exit 3)
        // or "elevation failed" (exit 5).
        assert_eq!(err.exit_code, 2, "missing entry is a pre-apply failure");
    }

    #[test]
    fn run_cli_check_validates_without_planning() {
        let d = TempDir::new("cli-check");
        let good = d.path.join("good.keron");
        fs::write(&good, "val x: Int = 1\n").unwrap();
        assert!(
            run_cli(["keron", "check", good.to_str().unwrap()]).is_ok(),
            "a valid manifest must check clean"
        );

        let bad = d.path.join("bad.keron");
        fs::write(&bad, "val x: Int = \"nope\"\n").unwrap();
        let err =
            run_cli(["keron", "check", bad.to_str().unwrap()]).expect_err("type error must fail");
        assert_eq!(err.exit_code, 2, "a type error is a pre-apply failure");
    }

    #[test]
    fn run_cli_apply_threads_execute_flag_to_keron_apply() {
        let err = run_cli([
            "keron",
            "apply",
            "--execute",
            "/no/such/keron-cli-test-exec.keron",
        ])
        .expect_err("missing path should still fail with --execute");
        assert!(
            format!("{err:#}").contains("/no/such/keron-cli-test-exec.keron"),
            "error should name the missing path",
        );
    }

    #[test]
    fn tmp_file_guard_removes_file_on_drop_when_armed() {
        // Pins `Drop::drop with ()`: a no-op drop would leak the
        // tempfile that the atomic-format-write path uses. Also pins
        // `delete ! in drop`: with the inversion, an armed guard
        // would not delete.
        let d = TempDir::new("cli-guard-armed");
        let path = d.path.join("scratch.tmp");
        fs::write(&path, "x").unwrap();
        {
            let _g = TmpFileGuard::new(path.clone());
        }
        assert!(!path.exists(), "armed guard's drop must delete: {path:?}");
    }

    #[test]
    fn tmp_file_guard_disarm_prevents_removal_on_drop() {
        // Pins `TmpFileGuard::disarm with ()`: if disarm fails to
        // set the flag, the file the caller just renamed gets
        // silently removed. Also pins `delete ! in drop`: inverted,
        // disarm would still trigger deletion.
        let d = TempDir::new("cli-guard-disarmed");
        let path = d.path.join("kept.tmp");
        fs::write(&path, "stay").unwrap();
        {
            let g = TmpFileGuard::new(path.clone());
            g.disarm();
        }
        assert!(path.exists(), "disarmed guard must keep file: {path:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "stay");
    }

    #[test]
    fn run_cli_format_writes_keron_files() {
        let dir = TempDir::new("format-write");
        let file = dir.path.join("main.keron");
        fs::write(&file, "val x: Int = 1  ").unwrap();
        run_cli([
            "keron".into(),
            "format".into(),
            file.clone().into_os_string(),
        ])
        .unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "val x: Int = 1\n");
    }

    #[test]
    fn run_cli_format_check_reports_drift_via_exit_code_2() {
        let dir = TempDir::new("format-check");
        let file = dir.path.join("main.keron");
        fs::write(&file, "val x: Int = 1  ").unwrap();
        let code = run_cli([
            OsString::from("keron"),
            OsString::from("format"),
            OsString::from("--check"),
            file.clone().into_os_string(),
        ])
        .expect("drift is not a tool error");
        // gofmt/prettier convention: exit 2 = drift detected (CI
        // should rerun the formatter), exit 1 = real failure.
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(2)));
        // Source file untouched in check mode.
        assert_eq!(fs::read_to_string(file).unwrap(), "val x: Int = 1  ");
    }

    #[test]
    fn run_cli_format_check_accepts_parseable_corpus_fixtures() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../keron-lang/tests/corpus");
        for rel in ["parse", "check", "errors/check"] {
            let mut stdin: &[u8] = &[];
            let mut stdout = Vec::<u8>::new();
            let target = FormatTarget::Paths(vec![root.join(rel)]);
            let code =
                run_format(target, true, false, &mut stdin, &mut stdout).unwrap_or_else(|err| {
                    panic!("parseable corpus fixtures under {rel} should be normalized: {err:#}")
                });
            assert_eq!(
                format!("{code:?}"),
                format!("{:?}", ExitCode::SUCCESS),
                "{rel} should not report drift"
            );
        }
    }

    #[test]
    fn run_cli_format_recurses_into_subdirectories() {
        let dir = TempDir::new("format-recursive");
        let sub = dir.path.join("nested");
        fs::create_dir_all(&sub).unwrap();
        let file = sub.join("main.keron");
        fs::write(&file, "val x: Int = 1  ").unwrap();
        run_cli([
            OsString::from("keron"),
            OsString::from("format"),
            dir.path.clone().into_os_string(),
        ])
        .unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "val x: Int = 1\n");
    }

    #[test]
    fn run_cli_format_ignores_non_keron_files_in_directories() {
        let dir = TempDir::new("format-ignore");
        let keron = dir.path.join("main.keron");
        let notes = dir.path.join("notes.txt");
        fs::write(&keron, "val x: Int = 1").unwrap();
        fs::write(&notes, "not keron syntax !!!").unwrap();
        run_cli([
            OsString::from("keron"),
            OsString::from("format"),
            dir.path.clone().into_os_string(),
        ])
        .unwrap();
        assert_eq!(fs::read_to_string(keron).unwrap(), "val x: Int = 1\n");
        assert_eq!(fs::read_to_string(notes).unwrap(), "not keron syntax !!!");
    }

    #[cfg(unix)]
    #[test]
    fn collect_keron_files_rejects_special_files() {
        let err = collect_keron_files(std::path::Path::new("/dev/null"))
            .expect_err("special files should be rejected before read_dir");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("neither a regular file nor a directory"),
            "got: {msg}",
        );
    }

    // -----------------------------------------------------------
    // cargo-fmt parity: diff in --check, stdin mode, color gating
    //
    // These exercise `run_format` directly (not `run_cli`) because
    // the in-process harness can pump bytes through `&[u8]` /
    // `Vec<u8>` without spawning a child or touching the real
    // stdin/stdout. The clap-routing layer is covered by the
    // existing `run_cli_format_*` tests above.
    // -----------------------------------------------------------

    /// Drift in a single file → unified diff on stdout with the
    /// conventional `--- ` / `+++ ` headers and at least one `-` /
    /// `+` line. Exit 2. Under `cargo test`, stdout is a pipe (not a
    /// TTY), so `color_enabled()` returns false and the diff is
    /// plain ASCII — no env-var fiddling needed.
    #[test]
    fn run_cli_format_check_emits_unified_diff() {
        let dir = TempDir::new("format-diff");
        let file = dir.path.join("main.keron");
        fs::write(&file, "val x: Int = 1  ").unwrap();
        let mut stdin: &[u8] = &[];
        let mut stdout = Vec::<u8>::new();
        let target = FormatTarget::Paths(vec![file]);
        let code = run_format(target, true, false, &mut stdin, &mut stdout).expect("format check");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(2)));
        let out = String::from_utf8(stdout).expect("utf8 diff");
        assert!(out.contains("--- "), "missing --- header: {out:?}");
        assert!(out.contains("+++ "), "missing +++ header: {out:?}");
        assert!(
            out.lines()
                .any(|l| l.starts_with('-') && !l.starts_with("---")),
            "missing - line: {out:?}",
        );
        assert!(
            out.lines()
                .any(|l| l.starts_with('+') && !l.starts_with("+++")),
            "missing + line: {out:?}",
        );
    }

    /// Two paths in one invocation: only the drifted file is
    /// touched; the clean one stays byte-for-byte identical.
    #[test]
    fn run_cli_format_multiple_paths() {
        let dir = TempDir::new("format-multi");
        let clean = dir.path.join("clean.keron");
        let drifted = dir.path.join("drift.keron");
        fs::write(&clean, "val x: Int = 1\n").unwrap();
        fs::write(&drifted, "val y: Int = 2  ").unwrap();
        let mut stdin: &[u8] = &[];
        let mut stdout = Vec::<u8>::new();
        let target = FormatTarget::Paths(vec![clean.clone(), drifted.clone()]);
        let code = run_format(target, false, true, &mut stdin, &mut stdout).expect("format multi");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(
            fs::read_to_string(&clean).unwrap(),
            "val x: Int = 1\n",
            "clean file was rewritten",
        );
        assert_eq!(
            fs::read_to_string(&drifted).unwrap(),
            "val y: Int = 2\n",
            "drifted file wasn't normalized",
        );
    }

    /// Stdin mode (empty paths, default flags): reads source, writes
    /// formatted output to stdout, exit 0.
    #[test]
    fn run_cli_format_stdin_writes_stdout() {
        let mut stdin: &[u8] = b"val x : Int=1";
        let mut stdout = Vec::<u8>::new();
        let target = FormatTarget::Stdin;
        let code = run_format(target, false, false, &mut stdin, &mut stdout).expect("format stdin");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(String::from_utf8(stdout).unwrap(), "val x: Int = 1\n",);
    }

    /// Stdin mode with `--check`: drift produces a unified diff
    /// labelled `<stdin>` and exit 2. Stdout-is-not-TTY under cargo
    /// test → color stays disabled, no env mutation needed.
    #[test]
    fn run_cli_format_stdin_check_emits_diff() {
        let mut stdin: &[u8] = b"val x : Int=1";
        let mut stdout = Vec::<u8>::new();
        let target = FormatTarget::Stdin;
        let code =
            run_format(target, true, false, &mut stdin, &mut stdout).expect("format stdin check");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(2)));
        let out = String::from_utf8(stdout).unwrap();
        assert!(out.contains("<stdin>"), "missing <stdin> label: {out:?}");
        assert!(out.contains("--- "), "missing --- header: {out:?}");
    }

    /// `resolve_format_target` collapses `[]` and `[-]` to Stdin and
    /// surfaces the mixed-paths error.
    #[test]
    fn run_cli_format_dash_is_stdin() {
        assert!(matches!(
            resolve_format_target(vec![]).unwrap(),
            FormatTarget::Stdin,
        ));
        assert!(matches!(
            resolve_format_target(vec![PathBuf::from("-")]).unwrap(),
            FormatTarget::Stdin,
        ));
        let err = resolve_format_target(vec![PathBuf::from("-"), PathBuf::from("file.keron")])
            .expect_err("mixed `-` + path should error");
        assert!(
            err.error.to_string().contains("cannot be mixed"),
            "got: {err}",
        );
    }

    /// `color_enabled_from` is the pure-function decision: only true
    /// when both inputs say "yes color, real terminal". Pins each
    /// quadrant so mutations of the `!no_color_set && stdout_is_tty`
    /// expression (e.g. `&&` → `||`, `!` deletion) fail at least one row.
    #[test]
    fn color_enabled_from_truth_table() {
        assert!(color_enabled_from(false, true), "tty + no NO_COLOR → color");
        assert!(!color_enabled_from(true, true), "NO_COLOR set must veto");
        assert!(!color_enabled_from(false, false), "no tty must veto");
        assert!(!color_enabled_from(true, false), "both vetoes still off");
    }

    /// `colorize_diff` styles `--- ` and `+++ ` headers in BOLD —
    /// distinct from the RED / GREEN treatment given to plain `-` /
    /// `+` content lines. Pins both halves of the `||` so the
    /// `&&` mutation (which would collapse the branch and let `--- `
    /// fall through to RED, `+++ ` to GREEN) is caught.
    #[test]
    fn colorize_diff_styles_file_headers_bold_distinct_from_content_lines() {
        let diff = "--- a/x\n+++ b/x\n-old\n+new\n";
        let got = colorize_diff(diff);
        // Header lines: BOLD (\x1b[1m), not RED or GREEN.
        let bold_minus = got.find("\x1b[1m--- a/x").expect("--- header must be bold");
        let bold_plus = got.find("\x1b[1m+++ b/x").expect("+++ header must be bold");
        assert!(
            !got[bold_minus..bold_minus + 16].contains("\x1b[31m"),
            "--- header must not be red"
        );
        assert!(
            !got[bold_plus..bold_plus + 16].contains("\x1b[32m"),
            "+++ header must not be green"
        );
        // Content lines: RED for `-old`, GREEN for `+new`.
        assert!(
            got.contains("\x1b[31m-old"),
            "content `-old` must be red: {got:?}"
        );
        assert!(
            got.contains("\x1b[32m+new"),
            "content `+new` must be green: {got:?}"
        );
    }

    /// `color_enabled` returns false when stdout isn't a TTY (the
    /// state under `cargo test` and any pipe / CI), and
    /// `render_diff(..., color=false)` produces strictly plain ASCII
    /// with no ANSI escapes. The `NO_COLOR` override is exercised via
    /// the smoke step in CI rather than here — touching env vars from
    /// a parallel-test process is hazardous (env-mutation is global)
    /// and the practical guarantee users care about is the
    /// "captured-output path stays plain" one we assert here.
    #[test]
    fn run_cli_format_check_stays_plain_in_non_tty() {
        assert!(
            !color_enabled(),
            "cargo test's stdout is captured into a pipe; color_enabled \
             must report false there"
        );
        let diff = render_diff("foo.keron", "val x : Int=1\n", "val x: Int = 1\n", false);
        assert!(!diff.contains('\x1b'), "ANSI escape leaked: {diff:?}");
    }

    /// `--quiet` suppresses the per-file "Formatted …" line that
    /// non-check mode otherwise prints to stdout.
    #[test]
    fn run_cli_format_quiet_suppresses_per_file_summary() {
        let dir = TempDir::new("format-quiet");
        let file = dir.path.join("main.keron");
        fs::write(&file, "val x: Int = 1  ").unwrap();
        let mut stdin: &[u8] = &[];
        let mut stdout = Vec::<u8>::new();
        let target = FormatTarget::Paths(vec![file.clone()]);
        let code = run_format(target, false, true, &mut stdin, &mut stdout).expect("format quiet");
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stdout.is_empty(), "expected no chatter: {stdout:?}");
        // Sanity: without --quiet the same call would print a line.
        let mut stdout2 = Vec::<u8>::new();
        fs::write(&file, "val x: Int = 1  ").unwrap();
        let target2 = FormatTarget::Paths(vec![file]);
        run_format(target2, false, false, &mut stdin, &mut stdout2).expect("format loud");
        assert!(
            String::from_utf8_lossy(&stdout2).contains("Formatted "),
            "loud mode missing summary: {stdout2:?}",
        );
    }
}
