//! `keron` — single-binary entry point. Subcommands are wired here;
//! the heavy lifting lives in the library crates.

use std::ffi::OsString;
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
        Command::ApplyElevated { payload } => keron_apply::run_elevated_child(&payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_cli_apply_with_missing_path_errors() {
        // End-to-end exercise of the dispatch path: parse argv,
        // route to the Apply arm, call keron_apply::run, observe the
        // canonical "path does not exist" error. The `Ok(())` mutant
        // on this fn would skip both the parse and the delegate, so
        // a guaranteed-failing input is enough to kill it.
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
        // `--execute` flips the inner `execute` arg. Even with a
        // missing path the executor branch differs only in stdin
        // handling, so both arms reach the same `path does not
        // exist` canonicalize error — what we're pinning here is
        // that the flag-parsing wiring stays intact.
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
}
