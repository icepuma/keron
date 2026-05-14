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
    },

    /// Normalize `.keron` files in place.
    Format {
        /// Path to a `.keron` file or directory tree containing
        /// `.keron` files.
        path: PathBuf,

        /// Check whether files are normalized without writing changes.
        #[arg(long)]
        check: bool,
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
        Command::Apply { path, execute } => match keron_apply::run(&path, execute) {
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
        Command::Format { path, check } => run_format(&path, check).map_err(CliError::from),
        Command::ApplyElevated { payload } => {
            keron_apply::run_elevated_child(&payload)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn run_format(path: &std::path::Path, check: bool) -> anyhow::Result<ExitCode> {
    let files = collect_keron_files(path)?;
    let mut changed = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file)
            .map_err(|e| anyhow::anyhow!("reading `{}`: {e}", file.display()))?;
        if let Err(diags) = keron_lang::parse(&text) {
            let msg = diags
                .into_iter()
                .map(|d| d.message)
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!("cannot format `{}`: {msg}", file.display());
        }
        let formatted = normalize_source(&text);
        if formatted != text {
            changed.push(file.clone());
            if !check {
                write_atomically(&file, formatted.as_bytes())
                    .map_err(|e| anyhow::anyhow!("writing `{}`: {e}", file.display()))?;
            }
        }
    }
    if check && !changed.is_empty() {
        // Drift-reported, not a tool error: exit 2 so CI can
        // distinguish "rerun format" from "tool broke". Sanitize
        // each filename so a hostile checkout whose paths contain
        // control bytes can't forge this output.
        let raw = format!(
            "formatting needed for {}",
            changed
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        eprintln!("{}", keron_apply::sanitize_terminal_message(&raw));
        return Ok(ExitCode::from(2));
    }
    Ok(ExitCode::SUCCESS)
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

fn normalize_source(src: &str) -> String {
    if src.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut multiline = None;
    for line in src.lines() {
        if let Some(close) = multiline {
            out.push_str(line);
            if is_multiline_close(line, close) {
                multiline = None;
            }
        } else {
            let trimmed = line.trim_end_matches([' ', '\t']);
            out.push_str(trimmed);
            multiline = multiline_open(trimmed);
        }
        out.push('\n');
    }
    out
}

#[derive(Clone, Copy)]
enum MultilineClose {
    Cooked,
    Raw(usize),
}

fn multiline_open(line: &str) -> Option<MultilineClose> {
    let mut in_string = false;
    let mut escaped = false;

    for (i, c) in line.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match c {
            '#' => break,
            '"' => {
                if &line[i..] == "\"\"\"" {
                    return Some(MultilineClose::Cooked);
                }
                in_string = true;
            }
            'r' => {
                if let Some(hashes) = raw_multiline_open_at(line, i) {
                    return Some(MultilineClose::Raw(hashes));
                }
            }
            _ => {}
        }
    }
    None
}

fn raw_multiline_open_at(line: &str, start: usize) -> Option<usize> {
    let mut rest = line.get(start..)?.strip_prefix('r')?;
    let mut hashes = 0usize;
    while let Some(next) = rest.strip_prefix('#') {
        hashes += 1;
        rest = next;
    }
    let rest = rest.strip_prefix("\"\"\"")?;
    if !rest.is_empty() {
        return None;
    }
    Some(hashes)
}

fn is_multiline_close(line: &str, close: MultilineClose) -> bool {
    let trimmed = line.trim_start_matches([' ', '\t']);
    match close {
        MultilineClose::Cooked => trimmed == "\"\"\"",
        MultilineClose::Raw(hashes) => {
            let Some(suffix) = trimmed.strip_prefix("\"\"\"") else {
                return false;
            };
            if suffix.len() != hashes {
                return false;
            }
            suffix.bytes().all(|b| b == b'#')
        }
    }
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
    fn normalize_source_trims_trailing_whitespace_and_final_newline() {
        assert_eq!(
            normalize_source("val x: Int = 1  \n\n"),
            "val x: Int = 1\n\n"
        );
        assert_eq!(normalize_source("val x: Int = 1"), "val x: Int = 1\n");
    }

    #[test]
    fn normalize_source_preserves_multiline_string_trailing_whitespace() {
        let src = "val x = \"\"\"\n  keep  \n  trim\n  \"\"\"\nval y = 1  ";
        assert_eq!(
            normalize_source(src),
            "val x = \"\"\"\n  keep  \n  trim\n  \"\"\"\nval y = 1\n"
        );
    }

    #[test]
    fn normalize_source_preserves_raw_multiline_string_trailing_whitespace() {
        let src = "val x = r#\"\"\"\n  keep  \n  \"\"\"#\nval y = 1  ";
        assert_eq!(
            normalize_source(src),
            "val x = r#\"\"\"\n  keep  \n  \"\"\"#\nval y = 1\n"
        );
    }

    #[test]
    fn normalize_source_ignores_openers_in_comments_and_single_line_strings() {
        assert_eq!(
            normalize_source("# \"\"\"\nval y = 1  "),
            "# \"\"\"\nval y = 1\n"
        );
        assert_eq!(
            normalize_source("val s = \"\\\"\\\"\\\"\"  \nval y = 1  "),
            "val s = \"\\\"\\\"\\\"\"\nval y = 1\n",
        );
    }

    #[test]
    fn normalize_source_requires_exact_raw_multiline_delimiters() {
        assert_eq!(
            normalize_source("val x = r#\"\"\" junk\nval y = 1  "),
            "val x = r#\"\"\" junk\nval y = 1\n",
        );

        let src = "val x = r#\"\"\"\nkeep  \n\"\"\"##  \nstill  \n\"\"\"#\nval y = 1  ";
        assert_eq!(
            normalize_source(src),
            "val x = r#\"\"\"\nkeep  \n\"\"\"##  \nstill  \n\"\"\"#\nval y = 1\n",
        );
    }

    #[test]
    fn multiline_open_skips_single_line_string_contents() {
        assert!(multiline_open(r#"val s = "\"""""#).is_none());
        assert!(matches!(
            multiline_open(r#"val s = "done" """"#),
            Some(MultilineClose::Cooked)
        ));
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
            let code = run_format(&root.join(rel), true).unwrap_or_else(|err| {
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
}
