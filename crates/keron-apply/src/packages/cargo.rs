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
    let status_label = out.status.to_string();
    decode_list_output(out.status.success(), &out.stdout, &out.stderr, &status_label)
}

/// Pure helper: branch on the `cargo install --list` exit status and
/// either parse stdout as UTF-8 (success) or surface a tagged error
/// with stderr (failure). Factored out so the success/failure branch
/// can be tested without spawning a real `cargo` process — the `fetch`
/// wrapper is the only producer of these inputs in production.
fn decode_list_output(
    ok: bool,
    stdout: &[u8],
    stderr: &[u8],
    status_label: &str,
) -> Result<HashSet<String>> {
    if !ok {
        bail!(
            "`cargo install --list` exited with status {status_label}; stderr: {}",
            String::from_utf8_lossy(stderr).trim(),
        );
    }
    let text =
        std::str::from_utf8(stdout).context("decoding `cargo install --list` output as UTF-8")?;
    Ok(parse(text))
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

    #[test]
    fn decode_list_output_returns_parsed_set_on_success() {
        let stdout = b"ripgrep v14.0.0:\n    rg\n";
        let got = decode_list_output(true, stdout, b"", "exit code: 0").unwrap();
        assert!(got.contains("ripgrep"));
    }

    #[test]
    fn decode_list_output_bails_on_nonzero_exit_with_stderr_context() {
        // Pins the success-gate `!` — a mutation that deletes the `!`
        // would treat nonzero exits as success and return whatever the
        // stdout parser produced (often an empty set), masking the real
        // failure from the user.
        let err = decode_list_output(false, b"", b"toolchain not found", "exit code: 101")
            .expect_err("nonzero exit must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("exit code: 101"), "got: {msg}");
        assert!(msg.contains("toolchain not found"), "got: {msg}");
    }
}
