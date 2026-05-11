//! `cargo install` integration.
//!
//! `cargo install --list` produces output like:
//!
//! ```text
//! ripgrep v14.0.0:
//!     rg
//! bat v0.24.0:
//!     bat
//! ```
//!
//! Each *header* line is `<name> v<version>:` flush-left; the
//! indented lines below are the installed binaries. We only care
//! about the package name — the first whitespace-delimited token on
//! a flush-left line.

use std::collections::HashSet;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub fn fetch() -> Result<HashSet<String>> {
    let out = Command::new("cargo")
        .args(["install", "--list"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawning `cargo install --list`")?;
    if !out.status.success() {
        bail!(
            "`cargo install --list` exited with status {}; stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    let text =
        String::from_utf8(out.stdout).context("decoding `cargo install --list` output as UTF-8")?;
    Ok(parse(&text))
}

/// Parse `cargo install --list` output. Flush-left lines (no
/// leading whitespace) are package headers; the first token before
/// any whitespace is the crate name. Indented lines list binaries
/// — ignored.
pub fn parse(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        // Cargo's header lines are flush-left; the binary lines
        // start with whitespace. `chars().next()` is correct here
        // because cargo uses ASCII spaces for indentation.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(name) = line.split_whitespace().next() else {
            continue;
        };
        // The first token includes a trailing colon when there's
        // no version (rare but possible: `cargo install` from a
        // path or git source). Strip a trailing `:` to be safe.
        let name = name.trim_end_matches(':');
        if !name.is_empty() {
            out.insert(name.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_output_extracts_crate_names() {
        let input = "ripgrep v14.0.0:\n    rg\nbat v0.24.0:\n    bat\n";
        let got = parse(input);
        let mut sorted: Vec<_> = got.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["bat", "ripgrep"]);
    }

    #[test]
    fn parse_handles_trailing_colon_without_version() {
        // `cargo install --git ...` produces a header without a
        // version, like `mycrate:` on one line. The trailing colon
        // is part of the first whitespace-delimited token.
        let input = "mycrate:\n    binary\n";
        let got = parse(input);
        let names: Vec<_> = got.into_iter().collect();
        assert_eq!(names, vec!["mycrate"]);
    }

    #[test]
    fn parse_skips_indented_binary_lines() {
        // The binary line `    rg` would parse as "rg" if we
        // mistakenly used `lines()` and the first token. Pin that
        // we ignore indented lines.
        let input = "ripgrep v14.0.0:\n    rg\n";
        let got = parse(input);
        assert_eq!(got.len(), 1);
        assert!(got.contains("ripgrep"));
        assert!(!got.contains("rg"));
    }

    #[test]
    fn parse_empty_input_returns_empty_set() {
        assert!(parse("").is_empty());
    }
}
