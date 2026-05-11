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
    if !out.status.success() {
        bail!(
            "`winget list` exited with status {}; stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    // Winget prints UTF-16 on some shells; try UTF-8 first, fall
    // back to lossy.
    let text = String::from_utf8(out.stdout.clone())
        .unwrap_or_else(|_| String::from_utf8_lossy(&out.stdout).into_owned());
    Ok(parse(&text))
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
            // Find a header that contains both "Name" and "Id" with
            // "Id" appearing after "Name" — distinguishes the real
            // header from any progress lines that happen to mention
            // either word.
            if let (Some(name_pos), Some(id_pos)) = (line.find("Name"), line.find("Id"))
                && id_pos > name_pos
            {
                id_col = Some(id_pos);
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
        let rest = &line[col..];
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
