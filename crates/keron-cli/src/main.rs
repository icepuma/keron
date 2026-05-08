//! `keron` — single-binary entry point. Subcommands are wired here;
//! the heavy lifting lives in the library crates.

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
    /// Run the keron language server over stdio. Editors invoke this
    /// to drive parse and type-check feedback.
    Lsp,

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
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Lsp => keron_lsp::run(),
        Command::Apply { path, execute } => keron_apply::run(&path, execute),
    }
}
