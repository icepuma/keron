//! `keron` — single-binary entry point. Subcommands are wired here;
//! the heavy lifting lives in the library crates.

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
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Lsp => keron_lsp::run(),
    }
}
