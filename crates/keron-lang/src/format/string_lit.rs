//! Canonical rendering of string-literal contents.
//!
//! Cooked single-line strings (`"..."`) get their special characters
//! re-escaped to canonical short forms (`\n`, `\t`, `\"`, `\\`, etc.)
//! so a formatter pass over `"a\u{000A}b"` produces `"a\nb"`. Raw
//! and multi-line strings are preserved verbatim and never re-emitted
//! through this module — the emitter walks the original source for
//! their bytes instead.

use std::fmt::Write;

/// Render `content` as the body of a cooked single-line string,
/// without the surrounding `"`. Each non-printable / non-ASCII byte
/// gets the shortest valid keron escape; printable ASCII passes
/// through untouched except for `"`, `\`, and the dollar-brace
/// sequence (`${`) — `$` is escaped only when immediately followed
/// by `{`, since the parser uses `${...}` as the interpolation
/// trigger and a bare `$` elsewhere is plain content.
#[must_use]
pub fn render_cooked_inner(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            c => {
                // Anything else (control chars, non-ASCII letters) is
                // emitted as a `\u{HHHH}` unicode escape. This keeps
                // the source 7-bit clean and survives clipboards and
                // terminals that fight non-ASCII content. Users who
                // want literal unicode can use multi-line strings
                // which don't go through this path.
                write!(out, "\\u{{{:04x}}}", c as u32).unwrap();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passes_through_unchanged() {
        assert_eq!(render_cooked_inner("hello"), "hello");
    }

    #[test]
    fn backslash_and_quote_escape() {
        assert_eq!(render_cooked_inner("a\\b\"c"), "a\\\\b\\\"c");
    }

    #[test]
    fn whitespace_specials_get_short_escapes() {
        assert_eq!(render_cooked_inner("a\nb\tc\rd"), "a\\nb\\tc\\rd");
    }

    #[test]
    fn null_byte_uses_short_escape() {
        assert_eq!(render_cooked_inner("\0"), "\\0");
    }

    #[test]
    fn non_ascii_uses_unicode_escape() {
        assert_eq!(render_cooked_inner("é"), "\\u{00e9}");
    }

    #[test]
    fn ascii_control_chars_use_unicode_escape() {
        // U+0007 (bell) has no short form; should become \u{0007}.
        assert_eq!(render_cooked_inner("\u{0007}"), "\\u{0007}");
    }

    #[test]
    fn space_is_preserved_verbatim() {
        assert_eq!(render_cooked_inner("a b c"), "a b c");
    }
}
