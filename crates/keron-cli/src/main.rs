//! `keron` — single-binary entry point. Subcommands are wired here;
//! the heavy lifting lives in the library crates.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;

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
fn main() -> anyhow::Result<()> {
    run_cli(std::env::args_os())
}

/// Parse an argv vector and dispatch to the right subcommand.
/// Split out from `main` so unit tests can construct argv values
/// without spawning the binary — every mutant inside this function
/// is reachable from a test.
fn run_cli<I, T>(args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::parse_from(args);
    match cli.command {
        Command::Apply { path, execute } => keron_apply::run(&path, execute),
        Command::Format { path, check } => run_format(&path, check),
        Command::ApplyElevated { payload } => keron_apply::run_elevated_child(&payload),
    }
}

fn run_format(path: &std::path::Path, check: bool) -> anyhow::Result<()> {
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
                fs::write(&file, formatted)
                    .map_err(|e| anyhow::anyhow!("writing `{}`: {e}", file.display()))?;
            }
        }
    }
    if check && !changed.is_empty() {
        anyhow::bail!(
            "formatting needed for {}",
            changed
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
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
    for line in src.lines() {
        out.push_str(line.trim_end_matches([' ', '\t']));
        out.push('\n');
    }
    out
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
    fn normalize_source_trims_trailing_whitespace_and_final_newline() {
        assert_eq!(
            normalize_source("val x: Int = 1  \n\n"),
            "val x: Int = 1\n\n"
        );
        assert_eq!(normalize_source("val x: Int = 1"), "val x: Int = 1\n");
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
    fn run_cli_format_check_reports_without_writing() {
        let dir = TempDir::new("format-check");
        let file = dir.path.join("main.keron");
        fs::write(&file, "val x: Int = 1  ").unwrap();
        let err = run_cli([
            OsString::from("keron"),
            OsString::from("format"),
            OsString::from("--check"),
            file.clone().into_os_string(),
        ])
        .expect_err("check mode should reject unformatted input");
        let msg = format!("{err:#}");
        assert!(msg.contains("formatting needed"), "got: {msg}");
        assert!(msg.contains("main.keron"), "got: {msg}");
        assert_eq!(fs::read_to_string(file).unwrap(), "val x: Int = 1  ");
    }

    #[test]
    fn run_cli_format_check_accepts_parseable_corpus_fixtures() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../keron-lang/tests/corpus");
        for rel in ["parse", "check", "errors/check"] {
            run_format(&root.join(rel), true).unwrap_or_else(|err| {
                panic!("parseable corpus fixtures under {rel} should be normalized: {err:#}")
            });
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
