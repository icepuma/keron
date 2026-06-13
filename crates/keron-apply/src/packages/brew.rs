//! Homebrew integration.
//!
//! State is observed through read-only probes:
//!   - `brew list --formula -1` — installed formulae (bare names)
//!   - `brew list --cask -1` — installed casks (bare names)
//!   - `brew tap` — installed taps (`user/repo` per line)
//!   - `brew tap-info --json=v1 USER/REPO` — one tap's remote URL and
//!     trust flag (the `trusted` field is new in brew 6.0)
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
//! Brew 6.0 introduced tap trust: non-official taps must be explicitly
//! trusted before their formulae/casks load. Since keron owns a tap's
//! lifecycle (it taps before it installs), it also trusts every tap it
//! manages via `brew trust --tap` (see [`do_trust`]).
//!
//! Mutating commands (`tap`, `trust`, `install`) inherit stdio so the
//! user sees real progress bars and download status. Installs are
//! additionally launched with `HOMEBREW_NO_ASK=1` by the package
//! executor to skip brew 6.0's new interactive install prompt.

use std::collections::HashSet;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Suppress mid-probe `brew update` runs. Without this, brew can
/// silently spend a minute fetching upstream commits the first time a
/// probe runs on a stale machine — fine interactively, ugly inside
/// `keron apply`.
const NO_AUTO_UPDATE: (&str, &str) = ("HOMEBREW_NO_AUTO_UPDATE", "1");

/// Skip brew 6.0's new install confirmation prompt (`-y, --no-ask` is
/// enabled by default when this is set, per `man brew`). Inert for
/// read-only commands, load-bearing for installs run with captured or
/// inherited stdio.
const NO_ASK: (&str, &str) = ("HOMEBREW_NO_ASK", "1");

/// Apply the brew subprocess environment uniformly to every `brew`
/// invocation keron makes — probes, taps, trusts and installs. Without
/// `HOMEBREW_NO_AUTO_UPDATE` a `brew tap`/`brew trust` can kick off an
/// implicit `brew update`, racing the concurrent install phase for
/// brew's global lock and producing flaky "Another active Homebrew
/// process is already in progress" failures; `do_tap` previously forgot
/// it entirely. The vars are inert for commands that don't trigger the
/// behaviour, so applying them everywhere is strictly safer than
/// per-call opt-in.
pub fn apply_brew_env(cmd: &mut Command) {
    cmd.env(NO_AUTO_UPDATE.0, NO_AUTO_UPDATE.1);
    cmd.env(NO_ASK.0, NO_ASK.1);
}

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

/// Remote URL and trust state for one tap, from `brew tap-info --json=v1`.
/// `trusted` is `None` when the field is absent (pre-6.0 brew); callers
/// treat `None` as "trust not enforced" so older brew never emits a
/// spurious drift action.
#[derive(Debug, Clone)]
pub struct TapInfo {
    pub remote: Option<String>,
    pub trusted: Option<bool>,
}

/// Fetch one tap's remote URL and trust flag. `Ok(None)` when the tap
/// isn't installed (`brew tap-info` returns an empty array). Callers
/// only invoke this when they believe the tap is installed, so a `None`
/// is mildly surprising but not an error.
pub fn fetch_tap_info(user_tap: &str) -> Result<Option<TapInfo>> {
    let out = brew_probe(&["tap-info", "--json=v1", user_tap])?;
    let parsed: Vec<TapInfoJson> = serde_json::from_str(&out)
        .with_context(|| format!("parsing `brew tap-info --json=v1 {user_tap}` output"))?;
    Ok(parsed.into_iter().next().map(|i| TapInfo {
        remote: i.remote,
        trusted: i.trusted,
    }))
}

/// One element of the `brew tap-info --json=v1` array. Only fields we
/// actually read are deserialized; both default so older brew output
/// (which omits `trusted`) still parses.
#[derive(Debug, Deserialize)]
struct TapInfoJson {
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    trusted: Option<bool>,
}

/// Canonicalise a tap remote URL for equality comparison. Brew 6.0
/// ignores a trailing `.git` (and trailing slash) when matching GitHub
/// remotes, so two forms differing only by those must classify as the
/// same tap rather than a URL drift that re-taps on every apply.
pub fn normalize_remote(url: &str) -> &str {
    let url = url.trim_end_matches('/');
    url.strip_suffix(".git").unwrap_or(url)
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
    apply_brew_env(&mut cmd);
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

/// Run `brew trust --tap user_tap`. Idempotent on brew's side. Brew 6.0
/// requires non-official taps to be explicitly trusted before their
/// formulae/casks load; since keron manages a tap's full lifecycle, it
/// trusts every tap it taps.
///
/// `brew trust` shipped in 5.1.15; older brew exits non-zero with an
/// "unknown command"-style diagnostic. We tolerate that single case so
/// keron keeps working on pre-6.0 installs, but propagate every other
/// failure (network, permissions, …). Fully-qualified installs already
/// auto-trust per-item on 6.0, so a failed `do_trust` only degrades the
/// `brew doctor` / bare-name experience rather than blocking installs.
pub fn do_trust(user_tap: &str) -> Result<()> {
    let binary = test_binary_override().unwrap_or_else(|| "brew".to_string());
    let mut cmd = Command::new(&binary);
    cmd.args(["trust", "--tap", user_tap]);
    apply_brew_env(&mut cmd);
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawning `{binary} trust --tap {user_tap}`"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lower = stderr.to_lowercase();
    if lower.contains("unknown command") || lower.contains("unknown subcommand") {
        return Ok(());
    }
    bail!(
        "`{binary} trust --tap {user_tap}` exited with status {}; stderr: {}",
        out.status,
        stderr.trim(),
    );
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

/// Shell out to `brew` with the standard brew env (see
/// [`apply_brew_env`]) and return stdout. Routes through the test
/// binary override so unit tests can pin behavior without a real brew.
fn brew_probe(args: &[&str]) -> Result<String> {
    let binary = test_binary_override().unwrap_or_else(|| "brew".to_string());
    let mut cmd = Command::new(&binary);
    cmd.args(args);
    apply_brew_env(&mut cmd);
    let out = cmd
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
    fn tap_info_json_extracts_remote_and_trusted() {
        let json = r#"[{"name":"icepuma/keron","remote":"https://github.com/icepuma/keron","trusted":true,"custom_remote":true}]"#;
        let parsed: Vec<TapInfoJson> = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].remote.as_deref(),
            Some("https://github.com/icepuma/keron"),
        );
        assert_eq!(parsed[0].trusted, Some(true));
    }

    #[test]
    fn tap_info_json_defaults_trusted_to_none_when_absent() {
        // Pre-6.0 brew output omits `trusted`; the field must deserialize
        // to None rather than erroring so older brew stays supported.
        let json = r#"[{"name":"icepuma/keron","remote":"https://github.com/icepuma/keron"}]"#;
        let parsed: Vec<TapInfoJson> = serde_json::from_str(json).unwrap();
        assert_eq!(parsed[0].trusted, None);
    }

    #[test]
    fn tap_info_json_handles_empty_array() {
        let parsed: Vec<TapInfoJson> = serde_json::from_str("[]").unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn normalize_remote_strips_git_suffix() {
        assert_eq!(
            normalize_remote("https://github.com/icepuma/keron.git"),
            "https://github.com/icepuma/keron",
        );
    }

    #[test]
    fn normalize_remote_strips_trailing_slash() {
        assert_eq!(
            normalize_remote("https://github.com/icepuma/keron/"),
            "https://github.com/icepuma/keron",
        );
    }

    #[test]
    fn normalize_remote_strips_git_and_slash() {
        assert_eq!(
            normalize_remote("https://github.com/icepuma/keron.git/"),
            "https://github.com/icepuma/keron",
        );
    }

    #[test]
    fn normalize_remote_passes_through_plain_url() {
        assert_eq!(
            normalize_remote("https://github.com/icepuma/keron"),
            "https://github.com/icepuma/keron",
        );
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
    fn write_brew_spy_stderr(
        dir: &std::path::Path,
        stderr: &str,
        exit_code: i32,
    ) -> std::path::PathBuf {
        use std::fmt::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let spy = dir.join("brew-spy-stderr.sh");
        let mut body = String::from("#!/bin/sh\n");
        let escaped = stderr.replace('\\', "\\\\").replace('\'', "'\\''");
        let _ = writeln!(body, "printf '%s\\n' '{escaped}' >&2");
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
    fn do_trust_ok_on_success() {
        let _g = super::super::lock_env();
        let dir = spy_dir("do-trust-ok");
        let spy = write_brew_spy_stderr(&dir, "", 0);
        set_brew_override(&spy);
        let result = do_trust("icepuma/keron");
        clear_brew_override();
        let _ = std::fs::remove_dir_all(&dir);
        result.expect("success exit must return Ok");
    }

    #[cfg(unix)]
    #[test]
    fn do_trust_tolerates_unknown_command_on_older_brew() {
        // `brew trust` shipped in 5.1.15; older brew rejects it as an
        // unknown command. do_trust must swallow that single case so
        // keron keeps working on pre-6.0 installs.
        let _g = super::super::lock_env();
        let dir = spy_dir("do-trust-unknown");
        let spy = write_brew_spy_stderr(&dir, "Error: Unknown command: trust", 1);
        set_brew_override(&spy);
        let result = do_trust("icepuma/keron");
        clear_brew_override();
        let _ = std::fs::remove_dir_all(&dir);
        result.expect("unknown-command on older brew must be tolerated");
    }

    #[cfg(unix)]
    #[test]
    fn do_trust_bails_on_real_error() {
        // Any non-success exit that isn't the tolerated unknown-command
        // diagnostic must surface as an error — swallowing it would let
        // the planner believe a tap is trusted when it isn't.
        let _g = super::super::lock_env();
        let dir = spy_dir("do-trust-fail");
        let spy = write_brew_spy_stderr(&dir, "Error: network down", 1);
        set_brew_override(&spy);
        let result = do_trust("icepuma/keron");
        clear_brew_override();
        let _ = std::fs::remove_dir_all(&dir);
        let err = result.expect_err("real error must bail");
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
