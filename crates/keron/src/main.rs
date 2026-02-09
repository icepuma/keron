// Target-specific transitive dependency split (mio/crossterm stack) is accepted for now.
#![allow(clippy::multiple_crate_versions)]

fn main() {
    match keron_cli::run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}
