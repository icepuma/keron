//! Lexical helpers shared by the parser, the trivia extractor, and
//! the formatter.
//!
//! The keron grammar is parsed by Chumsky combinators, but for
//! line-oriented passes (whitespace hygiene, comment extraction) we
//! need string-aware scanning that recognizes when a `#` is a real
//! comment vs. inside a `"..."` / `"""..."""` / `r#"""..."""#`. These
//! helpers stay deliberately small — they only answer two questions
//! per line:
//!
//! 1. Does this line *open* a multi-line string that continues onto
//!    the next line? ([`multiline_open`])
//! 2. Does this line *close* a multi-line string opened earlier?
//!    ([`is_multiline_close`])
//!
//! Single-line `"..."` strings are tracked internally by
//! `multiline_open` but never escape — they always close on the same
//! line they open on.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultilineClose {
    /// Cooked multi-line string: `"""..."""`. Escapes are processed,
    /// close is exactly `"""` on its own (possibly indented) line.
    Cooked,
    /// Raw multi-line string: `r#..."""..."""...#`. The `usize`
    /// records how many `#` characters appeared between the `r` and
    /// the opening `"""`, since the close must mirror the same count.
    Raw(usize),
}

/// Returns `Some(close)` if `line` opens a multi-line string that
/// continues onto the following line, otherwise `None`.
///
/// `#` outside a string ends scanning — the rest of the line is a
/// comment and cannot open a string. `#` inside a string is plain
/// content and is ignored.
#[must_use]
pub fn multiline_open(line: &str) -> Option<MultilineClose> {
    // Tolerate a trailing CR so CRLF input is treated identically to
    // LF: the triple-quote opener check below is an exact tail match.
    let line = line.strip_suffix('\r').unwrap_or(line);
    let mut in_string = false;
    let mut escaped = false;

    for (i, c) in line.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match c {
            '#' => break,
            '"' => {
                if &line[i..] == "\"\"\"" {
                    return Some(MultilineClose::Cooked);
                }
                in_string = true;
            }
            'r' => {
                // Only a raw-string opener when the `r` doesn't
                // continue an identifier: `bar#"""` is the identifier
                // `bar` plus the comment `#"""`, and treating the `r`
                // as a raw opener would make the scanner swallow every
                // following line (and every later comment) as string
                // body until a `"""#` line that may never come.
                if !prev_char_is_ident(line, i)
                    && let Some(hashes) = raw_multiline_open_at(line, i)
                {
                    return Some(MultilineClose::Raw(hashes));
                }
            }
            _ => {}
        }
    }
    None
}

/// True when the character immediately before byte offset `i` in `line`
/// can continue an identifier.
///
/// A leading `r` at `i` then continues an identifier rather than opening
/// a raw string. This uses Chumsky's Unicode XID continuation rule.
#[must_use]
pub fn prev_char_is_ident(line: &str, i: usize) -> bool {
    line.get(..i)
        .unwrap_or_default()
        .chars()
        .next_back()
        .is_some_and(unicode_ident::is_xid_continue)
}

/// If `line[start..]` begins with `r#*"""` and nothing follows after
/// the triple-quote, returns the hash count. Otherwise `None`.
///
/// Matches only the *opener* of a raw multi-line string that runs
/// past the end of the line; same-line raw strings (which close on
/// the same line) are handled inline by `multiline_open`.
#[must_use]
pub fn raw_multiline_open_at(line: &str, start: usize) -> Option<usize> {
    let mut rest = line.get(start..)?.strip_prefix('r')?;
    let mut hashes = 0usize;
    while let Some(next) = rest.strip_prefix('#') {
        hashes += 1;
        rest = next;
    }
    let rest = rest.strip_prefix("\"\"\"")?;
    if !rest.is_empty() {
        return None;
    }
    Some(hashes)
}

/// Returns true if `line` is the close of a multi-line string opened
/// with `close`.
///
/// Leading whitespace is allowed (indentation). For `Raw(n)`, the
/// close must be exactly `"""` followed by `n` hashes and nothing
/// else.
#[must_use]
pub fn is_multiline_close(line: &str, close: MultilineClose) -> bool {
    // Tolerate a trailing CR so CRLF input closes identically to LF.
    let line = line.strip_suffix('\r').unwrap_or(line);
    let trimmed = line.trim_start_matches([' ', '\t']);
    match close {
        MultilineClose::Cooked => trimmed == "\"\"\"",
        MultilineClose::Raw(hashes) => {
            let Some(suffix) = trimmed.strip_prefix("\"\"\"") else {
                return false;
            };
            if suffix.len() != hashes {
                return false;
            }
            suffix.bytes().all(|b| b == b'#')
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooked_triple_quote_opens_multiline() {
        assert_eq!(
            multiline_open("val s = \"\"\""),
            Some(MultilineClose::Cooked)
        );
    }

    #[test]
    fn raw_triple_quote_with_one_hash_records_hash_count() {
        assert_eq!(
            multiline_open("val s = r#\"\"\""),
            Some(MultilineClose::Raw(1))
        );
    }

    #[test]
    fn raw_triple_quote_with_three_hashes_records_three() {
        assert_eq!(
            multiline_open("val s = r###\"\"\""),
            Some(MultilineClose::Raw(3))
        );
    }

    #[test]
    fn single_line_string_does_not_open_multiline() {
        assert_eq!(multiline_open("val s = \"hello\""), None);
    }

    #[test]
    fn comment_lead_aborts_scan_before_quotes() {
        assert_eq!(multiline_open("# val s = \"\"\""), None);
    }

    #[test]
    fn hash_inside_single_line_string_is_content_not_comment() {
        // The `#` is inside the string, the trailing `"""` opens a
        // multi-line — the helper should still detect the opener
        // because the single-line string closes before it.
        assert_eq!(
            multiline_open("val s = \"a # b\"; val t = \"\"\""),
            Some(MultilineClose::Cooked)
        );
    }

    #[test]
    fn escaped_quote_inside_string_does_not_close_it_early() {
        assert_eq!(multiline_open("val s = \"a\\\"b\""), None);
    }

    #[test]
    fn cooked_close_matches_bare_triple_quote() {
        assert!(is_multiline_close("\"\"\"", MultilineClose::Cooked));
    }

    #[test]
    fn cooked_close_tolerates_leading_indent() {
        assert!(is_multiline_close("    \"\"\"", MultilineClose::Cooked));
        assert!(is_multiline_close("\t\"\"\"", MultilineClose::Cooked));
    }

    #[test]
    fn cooked_close_rejects_trailing_content() {
        assert!(!is_multiline_close("\"\"\" rest", MultilineClose::Cooked));
    }

    #[test]
    fn raw_close_requires_matching_hash_count() {
        assert!(is_multiline_close("  \"\"\"#", MultilineClose::Raw(1)));
        assert!(is_multiline_close("\"\"\"###", MultilineClose::Raw(3)));
        assert!(!is_multiline_close("\"\"\"##", MultilineClose::Raw(1)));
        assert!(!is_multiline_close("\"\"\"", MultilineClose::Raw(1)));
    }

    #[test]
    fn raw_open_at_rejects_missing_quotes() {
        assert_eq!(raw_multiline_open_at("r##", 0), None);
    }

    #[test]
    fn raw_open_at_rejects_content_after_quotes() {
        assert_eq!(raw_multiline_open_at("r#\"\"\"hello", 0), None);
    }

    #[test]
    fn unicode_xid_before_r_is_an_identifier_continuation() {
        let accented = "ér#\"\"\"";
        let r = accented.find('r').expect("contains r");
        assert!(prev_char_is_ident(accented, r));

        let combining = "e\u{301}r#\"\"\"";
        let r = combining.find('r').expect("contains r");
        assert!(prev_char_is_ident(combining, r));
    }

    #[test]
    fn unicode_whitespace_before_r_is_not_an_identifier_continuation() {
        for src in ["\u{00a0}r#\"\"\"", "\u{2003}r#\"\"\""] {
            let r = src.find('r').expect("contains r");
            assert!(!prev_char_is_ident(src, r));
        }
    }
}
