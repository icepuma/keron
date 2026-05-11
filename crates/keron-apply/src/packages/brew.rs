//! Homebrew integration.
//!
//! `brew list --formula -1` is the stable, machine-friendly way to
//! list installed formulae. `--formula` excludes casks (a separate
//! concern — keron's `brew()` resource maps to formulae only in v1);
//! `-1` forces one entry per line so we don't have to deal with
//! columnar output that varies with terminal width.

use std::collections::HashSet;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub fn fetch() -> Result<HashSet<String>> {
    let out = Command::new("brew")
        .args(["list", "--formula", "-1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawning `brew list`")?;
    if !out.status.success() {
        bail!(
            "`brew list --formula -1` exited with status {}; stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    let text = String::from_utf8(out.stdout).context("decoding `brew list` output as UTF-8")?;
    Ok(parse(&text))
}

/// Parse `brew list --formula -1` output into a set of formula
/// names. Format is one name per line; we strip empty lines so a
/// trailing newline (which `-1` doesn't always emit) doesn't get
/// recorded as a phantom package called "".
pub fn parse(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_one_per_line_collects_names() {
        let input = "git\nripgrep\nfd\n";
        let got = parse(input);
        let mut sorted: Vec<_> = got.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["fd", "git", "ripgrep"]);
    }

    #[test]
    fn parse_skips_blank_lines_and_trims() {
        let input = "  git  \n\nripgrep\n   \n";
        let got = parse(input);
        let mut sorted: Vec<_> = got.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["git", "ripgrep"]);
    }

    #[test]
    fn parse_empty_input_returns_empty_set() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n\n").is_empty());
    }
}
