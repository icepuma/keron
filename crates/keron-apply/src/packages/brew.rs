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

    #[cfg(unix)]
    fn write_brew_spy(dir: &std::path::Path, stdout: &str, exit_code: i32) -> std::path::PathBuf {
        use std::fmt::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let spy = dir.join("brew-spy.sh");
        let mut body = String::from("#!/bin/sh\n");
        for line in stdout.lines() {
            // Escape any single quotes in the line by using printf.
            let escaped = line.replace('\\', "\\\\").replace('\'', "'\\''");
            let _ = writeln!(body, "printf '%s\\n' '{escaped}'");
        }
        let _ = writeln!(body, "exit {exit_code}");
        std::fs::write(&spy, body).unwrap();
        std::fs::set_permissions(&spy, std::fs::Permissions::from_mode(0o755)).unwrap();
        spy
    }

    #[cfg(unix)]
    fn spy_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "keron-brew-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos()),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn set_brew_override(spy: &std::path::Path) {
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
            std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", spy);
        }
    }

    #[cfg(unix)]
    fn clear_brew_override() {
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
        }
    }

    #[cfg(unix)]
    #[test]
    fn fetch_formulae_returns_parsed_set_from_spy_stdout() {
        // Drive fetch_formulae through the test binary override.
        // The function-body mutations (`Ok(HashSet::new())`,
        // `Ok(HashSet::from_iter([""]))`, `Ok(HashSet::from_iter(["xyzzy"]))`)
        // all produce a fixed set that doesn't match the spy's output;
        // asserting the exact contents catches every such replacement.
        let _g = super::super::lock_env();
        let dir = spy_dir("formulae");
        let spy = write_brew_spy(&dir, "git\nripgrep\nfd\n", 0);
        set_brew_override(&spy);
        let got = fetch_formulae();
        clear_brew_override();
        let _ = std::fs::remove_dir_all(&dir);
        let mut sorted: Vec<_> = got.expect("spy succeeds").into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["fd", "git", "ripgrep"]);
    }

    #[cfg(unix)]
    #[test]
    fn fetch_casks_returns_parsed_set_from_spy_stdout() {
        let _g = super::super::lock_env();
        let dir = spy_dir("casks");
        let spy = write_brew_spy(&dir, "alacritty\nghostty\n", 0);
        set_brew_override(&spy);
        let got = fetch_casks();
        clear_brew_override();
        let _ = std::fs::remove_dir_all(&dir);
        let mut sorted: Vec<_> = got.expect("spy succeeds").into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["alacritty", "ghostty"]);
    }

    #[cfg(unix)]
    #[test]
    fn fetch_taps_returns_parsed_set_from_spy_stdout() {
        let _g = super::super::lock_env();
        let dir = spy_dir("taps");
        let spy = write_brew_spy(&dir, "icepuma/keron\nfluxcd/tap\n", 0);
        set_brew_override(&spy);
        let got = fetch_taps();
        clear_brew_override();
        let _ = std::fs::remove_dir_all(&dir);
        let mut sorted: Vec<_> = got.expect("spy succeeds").into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["fluxcd/tap", "icepuma/keron"]);
    }

    #[cfg(unix)]
    #[test]
    fn brew_probe_bails_on_nonzero_exit_with_stderr_context() {
        // Pin the `!status.success()` gate inside brew_probe. A spy
        // that exits non-zero must surface as an error from the
        // public fetch_* wrappers — the `delete !` mutation would
        // accept failure as success and try to parse whatever
        // (possibly empty) stdout came back.
        let _g = super::super::lock_env();
        let dir = spy_dir("brew-fail");
        let spy = write_brew_spy(&dir, "", 1);
        set_brew_override(&spy);
        let result = fetch_formulae();
        clear_brew_override();
        let _ = std::fs::remove_dir_all(&dir);
        let err = result.expect_err("nonzero exit must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exited with status"),
            "expected status diagnostic, got: {msg}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn do_tap_bails_on_nonzero_exit() {
        // Pin the `!status.success()` gate inside do_tap. The
        // mutation would consume a non-zero exit as success and
        // silently return Ok — letting the planner believe a tap
        // ran when it didn't.
        let _g = super::super::lock_env();
        let dir = spy_dir("do-tap-fail");
        let spy = write_brew_spy(&dir, "", 7);
        set_brew_override(&spy);
        let result = do_tap("icepuma/keron", None, false);
        clear_brew_override();
        let _ = std::fs::remove_dir_all(&dir);
        let err = result.expect_err("nonzero exit must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exited with status"),
            "expected status diagnostic, got: {msg}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_binary_override_returns_override_when_allow_gate_set() {
        // Pin the test seam: when the allow-gate is set and the
        // env var carries a path, the function returns it. Catches
        // the function-body replacements (`-> None`, `-> Some("")`,
        // `-> Some("xyzzy")`) and the `delete !` mutation on the
        // gate (which would invert the gate logic).
        let _g = super::super::lock_env();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
            std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", "/tmp/fake-brew");
        }
        let got = test_binary_override();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
        }
        assert_eq!(got, Some("/tmp/fake-brew".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_binary_override_returns_none_without_allow_gate() {
        // Same fixture without `KERON_ALLOW_TEST_OVERRIDES`. The gate
        // must refuse even with the override path set, so a hostile
        // env in production can't silently swap brew. Catches the
        // `delete !` mutation that would invert the gate.
        let _g = super::super::lock_env();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
            std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", "/tmp/fake-brew");
        }
        let got = test_binary_override();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
        }
        assert!(
            got.is_none(),
            "override must require allow-gate, got: {got:?}"
        );
    }
}
