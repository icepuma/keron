//! Homebrew integration.
//!
//! State is observed through three read-only probes:
//!   - `brew list --formula -1` — installed formulae (bare names)
//!   - `brew list --cask -1` — installed casks (bare names)
//!   - `brew tap` — installed taps (`user/repo` per line)
//!   - `brew tap-info --json USER/REPO` — one tap's remote URL
//!
//! All probes run with `HOMEBREW_NO_AUTO_UPDATE=1` so a sleepy local
//! brew doesn't auto-update mid-classify, polluting stderr and racing
//! against the snapshot we just read.
//!
//! `--formula` excludes casks; `-1` forces one entry per line so we
//! don't have to deal with columnar output that varies with terminal
//! width. Casks live in their own namespace because brew treats them
//! distinctly (`--cask` flag, separate install root).
//!
//! `keron apply` only ensures *presence* — there is no `brew outdated`
//! probe and no `brew upgrade` path. Upgrading installed packages is
//! the user's job via the underlying manager.
//!
//! Mutating commands (`tap`, `install`) inherit stdio so the user
//! sees real progress bars and download status.

use std::collections::HashSet;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Suppress mid-probe `brew update` runs. Without this, brew can
/// silently spend a minute fetching upstream commits the first time a
/// probe runs on a stale machine — fine interactively, ugly inside
/// `keron apply`.
const NO_AUTO_UPDATE: (&str, &str) = ("HOMEBREW_NO_AUTO_UPDATE", "1");

pub fn fetch_formulae() -> Result<HashSet<String>> {
    let out = brew_probe(&["list", "--formula", "-1"])?;
    Ok(parse_lines(&out))
}

pub fn fetch_casks() -> Result<HashSet<String>> {
    let out = brew_probe(&["list", "--cask", "-1"])?;
    Ok(parse_lines(&out))
}

pub fn fetch_taps() -> Result<HashSet<String>> {
    let out = brew_probe(&["tap"])?;
    Ok(parse_lines(&out))
}

/// Returns the remote URL of `user_tap` per `brew tap-info --json`.
/// `Ok(None)` when the tap isn't installed (callers only invoke this
/// when they believe it is, so a `None` here is mildly surprising but
/// not an error).
pub fn fetch_tap_remote(user_tap: &str) -> Result<Option<String>> {
    let out = brew_probe(&["tap-info", "--json", user_tap])?;
    let parsed: Vec<TapInfo> = serde_json::from_str(&out)
        .with_context(|| format!("parsing `brew tap-info --json {user_tap}` output"))?;
    Ok(parsed.into_iter().next().map(|i| i.remote))
}

/// One element of the `brew tap-info --json` array. Only fields we
/// actually read are deserialized.
#[derive(Debug, Deserialize)]
struct TapInfo {
    remote: String,
}

/// Run `brew tap user_tap [URL]`. With `custom_remote=true`, passes
/// `--custom-remote` so brew rewrites the local clone's remote when
/// the tap already exists. Stdio inherited so the user sees progress.
pub fn do_tap(user_tap: &str, url: Option<&str>, custom_remote: bool) -> Result<()> {
    let binary = test_binary_override().unwrap_or_else(|| "brew".to_string());
    let mut cmd = Command::new(&binary);
    cmd.arg("tap");
    if custom_remote {
        cmd.arg("--custom-remote");
    }
    cmd.arg(user_tap);
    if let Some(u) = url {
        cmd.arg(u);
    }
    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("spawning `{binary} tap {user_tap}`"))?;
    if !status.success() {
        bail!("`{binary} tap {user_tap}` exited with status {status}");
    }
    Ok(())
}

/// Reject tap URLs that would smuggle behavior past `brew tap` — same
/// flag-injection and NUL guards as [`super::validate_package_name`],
/// plus a transport prefix check so we don't accept `file://` or bare
/// paths (both technically work with `brew tap URL`, but neither is a
/// shape keron should silently encourage from a manifest).
///
/// # Errors
/// Errors when `url` is empty, begins with `-`, contains a NUL byte,
/// or doesn't begin with `http://`, `https://`, or `git@`.
pub fn validate_tap_url(url: &str) -> Result<()> {
    if url.is_empty() {
        bail!("tap URL must not be empty");
    }
    if url.starts_with('-') {
        bail!("tap URL must not begin with `-`: `{url}`");
    }
    if url.contains('\0') {
        bail!("tap URL must not contain a NUL byte");
    }
    if !(url.starts_with("https://") || url.starts_with("http://") || url.starts_with("git@")) {
        bail!(
            "tap URL `{url}` must start with `https://`, `http://`, or `git@` \
             (file:// and bare paths are not accepted)"
        );
    }
    Ok(())
}

/// Parse a one-name-per-line probe output. Strips blank lines so a
/// trailing newline doesn't become a phantom empty entry.
pub fn parse_lines(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Shell out to `brew` with `HOMEBREW_NO_AUTO_UPDATE=1` and return
/// stdout. Routes through the test binary override so unit tests can
/// pin behavior without a real brew.
fn brew_probe(args: &[&str]) -> Result<String> {
    let binary = test_binary_override().unwrap_or_else(|| "brew".to_string());
    let out = Command::new(&binary)
        .args(args)
        .env(NO_AUTO_UPDATE.0, NO_AUTO_UPDATE.1)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawning `{binary} {}`", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "`{binary} {}` exited with status {}; stderr: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    String::from_utf8(out.stdout)
        .with_context(|| format!("decoding `{binary} {}` output as UTF-8", args.join(" ")))
}

fn test_binary_override() -> Option<String> {
    if !super::test_overrides_allowed() {
        return None;
    }
    std::env::var("KERON_TEST_PACKAGE_BIN_BREW").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lines_one_per_line_collects_names() {
        let input = "git\nripgrep\nfd\n";
        let mut sorted: Vec<_> = parse_lines(input).into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["fd", "git", "ripgrep"]);
    }

    #[test]
    fn parse_lines_skips_blank_lines_and_trims() {
        let input = "  git  \n\nripgrep\n   \n";
        let mut sorted: Vec<_> = parse_lines(input).into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["git", "ripgrep"]);
    }

    #[test]
    fn parse_lines_empty_input_returns_empty_set() {
        assert!(parse_lines("").is_empty());
        assert!(parse_lines("\n\n\n").is_empty());
    }

    #[test]
    fn parse_lines_handles_qualified_tap_names() {
        // `brew outdated --formula --quiet` reports tapped formulae as
        // `user/tap/formula`. Pin that the parser passes them through
        // without surgery.
        let input = "icepuma/keron/keron\nfluxcd/tap/flux\n";
        let mut sorted: Vec<_> = parse_lines(input).into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["fluxcd/tap/flux", "icepuma/keron/keron"]);
    }

    #[test]
    fn tap_info_json_extracts_remote() {
        let json = r#"[{"name":"icepuma/keron","remote":"https://github.com/icepuma/keron","custom_remote":true}]"#;
        let parsed: Vec<TapInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].remote, "https://github.com/icepuma/keron");
    }

    #[test]
    fn tap_info_json_handles_empty_array() {
        let parsed: Vec<TapInfo> = serde_json::from_str("[]").unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn validate_tap_url_accepts_https() {
        validate_tap_url("https://github.com/icepuma/keron").unwrap();
    }

    #[test]
    fn validate_tap_url_accepts_http() {
        validate_tap_url("http://internal.example/keron").unwrap();
    }

    #[test]
    fn validate_tap_url_accepts_ssh_alias() {
        validate_tap_url("git@github.com:icepuma/keron.git").unwrap();
    }

    #[test]
    fn validate_tap_url_rejects_file_scheme() {
        let err = validate_tap_url("file:///tmp/local-tap").unwrap_err();
        assert!(format!("{err:#}").contains("file://"), "got: {err:#}");
    }

    #[test]
    fn validate_tap_url_rejects_leading_dash() {
        let err = validate_tap_url("--anything").unwrap_err();
        assert!(format!("{err:#}").contains("must not begin with `-`"));
    }

    #[test]
    fn validate_tap_url_rejects_nul_byte() {
        let err = validate_tap_url("https://example.com/x\0evil").unwrap_err();
        assert!(format!("{err:#}").contains("NUL byte"));
    }

    #[test]
    fn validate_tap_url_rejects_empty() {
        let err = validate_tap_url("").unwrap_err();
        assert!(format!("{err:#}").contains("must not be empty"));
    }
}
