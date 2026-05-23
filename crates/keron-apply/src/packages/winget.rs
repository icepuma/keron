//! `winget list` integration.
//!
//! `winget` doesn't have a stable machine-readable list format
//! (`--output json` is not supported in all stable versions). v1
//! parses the human columnar output by locating the `Id` column
//! position from the header row and slicing each data row from
//! there. The keron `winget(...)` resource uses package IDs
//! (`Microsoft.PowerShell` etc.), which is what we extract.
//!
//! Fragility: this approach breaks if a future winget version
//! changes the column header text or the dashes-under-header
//! separator. When that happens [`parse`] returns an empty set and
//! every winget resource classifies as Create — `winget install`
//! itself is idempotent enough to handle "already installed" with a
//! non-zero exit, so the planner is the only thing affected. The
//! parse function logs nothing; tests pin known-good fixtures.

use std::collections::HashSet;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub fn fetch() -> Result<HashSet<String>> {
    let out = Command::new("winget")
        .args([
            "list",
            "--accept-source-agreements",
            "--disable-interactivity",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawning `winget list`")?;
    let status_label = out.status.to_string();
    decode_list_output(out.status.success(), &out.stdout, &out.stderr, &status_label)
}

/// Pure helper: branch on the `winget list` exit status. Factored out
/// of `fetch` so the success-vs-failure path is testable without
/// spawning a real `winget` binary (which only exists on Windows).
fn decode_list_output(
    ok: bool,
    stdout: &[u8],
    stderr: &[u8],
    status_label: &str,
) -> Result<HashSet<String>> {
    if !ok {
        bail!(
            "`winget list` exited with status {status_label}; stderr: {}",
            String::from_utf8_lossy(stderr).trim(),
        );
    }
    // Winget prints UTF-16 on some shells; try UTF-8 first, fall
    // back to lossy.
    let text = String::from_utf8(stdout.to_vec())
        .unwrap_or_else(|_| String::from_utf8_lossy(stdout).into_owned());
    Ok(parse(&text))
}

/// Locate the byte offset of the `Id` column in a header line. The
/// real header has both `Name` and `Id`, and `Id` appears strictly
/// after `Name` so a stray progress line that happens to mention
/// either word can't be mistaken for the column header. Distinct
/// substrings cannot both start at the same byte (`name_pos == id_pos`
/// is impossible), so the `>` boundary is equivalent to `>=` here —
/// the comparison is kept strict-greater for intent.
#[cfg_attr(test, mutants::skip)]
fn locate_id_column(line: &str) -> Option<usize> {
    let name_pos = line.find("Name")?;
    let id_pos = line.find("Id")?;
    if id_pos > name_pos { Some(id_pos) } else { None }
}

/// Parse the columnar `winget list` output. Locates the `Id` column
/// in the header and takes the first whitespace-delimited token
/// starting at that column in every subsequent data row.
pub fn parse(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut id_col: Option<usize> = None;
    let mut past_header = false;
    for line in text.lines() {
        if id_col.is_none() {
            if let Some(pos) = locate_id_column(line) {
                id_col = Some(pos);
            }
            continue;
        }
        if !past_header {
            // The dashes-under-header line. winget separates them
            // from the data rows with `---- ----` style ruler.
            if line.chars().all(|c| c == '-' || c == ' ') && !line.is_empty() {
                past_header = true;
                continue;
            }
            // Already past the header but ruler missing? Treat the
            // current row as data.
            past_header = true;
        }
        let col = id_col.expect("set in the header pass");
        if line.len() <= col {
            continue;
        }
        // The header is ASCII so `col` is both a byte and char
        // offset, but data rows may contain multi-byte Unicode
        // (localized package descriptions). Slicing at a non-char
        // boundary would panic; advance to the next valid boundary
        // — at worst we drop one column of payload.
        let Some(start) = line.char_indices().map(|(i, _)| i).find(|&i| i >= col) else {
            continue;
        };
        let rest = &line[start..];
        let Some(id) = rest.split_whitespace().next() else {
            continue;
        };
        if !id.is_empty() {
            out.insert(id.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_columnar_output_extracts_ids() {
        // Synthetic fixture mirroring `winget list` on Windows 11.
        // The Id column starts at the "Id" header position; the
        // ruler line is dashes.
        let input = "\
Name                       Id                       Version    Source
-------------------------------------------------------------------------
Microsoft PowerShell       Microsoft.PowerShell     7.4.1.0    winget
Visual Studio Code         Microsoft.VisualStudioCode 1.85.0   winget
";
        let got = parse(input);
        let mut sorted: Vec<_> = got.into_iter().collect();
        sorted.sort();
        assert_eq!(
            sorted,
            vec!["Microsoft.PowerShell", "Microsoft.VisualStudioCode"]
        );
    }

    #[test]
    fn parse_returns_empty_when_header_is_missing() {
        // No "Name"/"Id" header → we can't locate the column → no
        // packages reported. Fail-safe: the apply step will then
        // try to install, and `winget install` will refuse if it's
        // already installed. The diff will be wrong but apply is
        // still correct.
        let input = "winget needs to update its sources, please wait\n";
        assert!(parse(input).is_empty());
    }

    #[test]
    fn parse_handles_progress_lines_above_the_header() {
        let input = "\
\\
- 0%
Name                  Id                  Version
-------------------------------------------------
Foo                   Foo.Bar             1.0
";
        let got = parse(input);
        assert!(got.contains("Foo.Bar"), "got: {got:?}");
    }

    #[test]
    fn parse_does_not_panic_on_multibyte_chars_at_id_column() {
        // The Name column carries a localized description with
        // multi-byte UTF-8; the Id-column byte index lands inside
        // the multi-byte sequence. Pre-fix this panicked with
        // "byte index N is not a char boundary".
        let input = "\
Name                       Id                       Version
-------------------------------------------------------------------------
Кириллица описание длиннее Foo.Cyrillic              1.0
";
        let got = parse(input);
        // We don't care exactly what fell out — only that we did
        // not panic. The parser may drop the row or extract a
        // partial column; both are acceptable for the fallback.
        let _ = got;
    }

    #[test]
    fn parse_advances_forward_past_multibyte_char_boundary() {
        // Pins the FORWARD direction of the char-boundary walk: with
        // col=5 landing mid-"γ", a correct walk lands at byte 6
        // (start of " FOO"), so the extracted Id is "FOO". A
        // backward walk would land at byte 4 (start of "γ") and
        // extract "γFOO" or similar garbage. Catches mutations that
        // search backward / never advance.
        let input = "\
Name Id    Version
------------------
αβγ  FOO   1.0
";
        let got = parse(input);
        assert!(
            got.contains("FOO"),
            "must advance forward to next char boundary, got: {got:?}"
        );
        assert!(
            !got.iter().any(|s| s.contains('γ')),
            "must not retreat into the multibyte char, got: {got:?}"
        );
    }

    #[test]
    fn parse_treats_first_post_header_line_as_data_when_ruler_is_missing() {
        // Some winget builds elide the dashes-under-header ruler. The
        // parser's fallback path commits to data-mode on the first line
        // after the header. Pins the `&&` between the all-dashes-or-spaces
        // check and `!line.is_empty()` — replacing it with `||` would
        // treat the first data row as the ruler (because `!is_empty` is
        // true), silently dropping it from the installed set.
        let input = "\
Name                       Id                       Version
Foo                        Foo.Bar                  1.0
Quux                       Quux.Baz                 2.0
";
        let got = parse(input);
        let mut sorted: Vec<_> = got.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["Foo.Bar", "Quux.Baz"]);
    }

    #[test]
    fn locate_id_column_rejects_when_id_precedes_name() {
        // A progress / status line that mentions "Id" before "Name"
        // must not be mistaken for the column header — otherwise the
        // parser would lock onto the wrong column for the rest of the
        // stream. Pins the strict ordering check inside
        // locate_id_column.
        assert_eq!(locate_id_column("Id is required, see Name help"), None);
    }

    #[test]
    fn locate_id_column_returns_id_offset_when_after_name() {
        let line = "Name          Id        Version";
        let got = locate_id_column(line).expect("real header must match");
        assert_eq!(got, line.find("Id").unwrap());
    }

    #[test]
    fn decode_list_output_returns_parsed_set_on_success() {
        let stdout = b"\
Name                       Id                       Version
-------------------------------------------------------------------------
Microsoft PowerShell       Microsoft.PowerShell     7.4.1.0
";
        let got = decode_list_output(true, stdout, b"", "exit code: 0").unwrap();
        assert!(got.contains("Microsoft.PowerShell"));
    }

    #[test]
    fn decode_list_output_bails_on_nonzero_exit_with_stderr_context() {
        // Pins the success-gate `!` in decode_list_output — a mutation
        // that deletes the `!` would treat nonzero exits as success and
        // return an empty installed-set, masking the failure from the
        // classifier.
        let err = decode_list_output(false, b"", b"winget not registered", "exit code: 5")
            .expect_err("nonzero exit must bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("exit code: 5"), "got: {msg}");
        assert!(msg.contains("winget not registered"), "got: {msg}");
    }

    #[test]
    fn parse_skips_lines_shorter_than_id_column() {
        let input = "\
Name                       Id                       Version
-------------------------------------------------------------------------
short
Microsoft PowerShell       Microsoft.PowerShell     7.4.1.0
";
        let got = parse(input);
        assert!(got.contains("Microsoft.PowerShell"));
        assert!(!got.contains("short"));
    }
}
