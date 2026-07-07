//! Canonical rendering of string-literal contents.
//!
//! Cooked single-line strings (`"..."`) get their special characters
//! re-escaped to canonical short forms (`\n`, `\t`, `\"`, `\\`, etc.)
//! so a formatter pass over `"a\u{000A}b"` produces `"a\nb"`. Raw
//! and multi-line strings are preserved verbatim and never re-emitted
//! through this module — the emitter walks the original source for
//! their bytes instead.

/// Render `content` as the body of a cooked single-line string,
/// without the surrounding `"`. Only the escapes the keron parser
/// actually understands are emitted — `\"`, `\\`, `\n`, `\r`, `\t`,
/// and the dollar-brace sequence (`\$`, escaped only when `$` is
/// immediately followed by `{`, since the parser uses `${...}` as
/// the interpolation trigger and a bare `$` elsewhere is plain
/// content). Every other character passes through verbatim.
///
/// The verbatim path is what keeps the formatter's output
/// re-parseable. The keron string grammar has no `\u{...}` and no
/// `\0` escape (see `parser/string.rs::parse_escape`, which accepts
/// exactly `" \ n r t $`); non-ASCII letters and rare control
/// characters can only ever enter a cooked literal as raw bytes, so
/// they must leave it the same way. Escaping them — as an earlier
/// "keep the source 7-bit clean" version did — produced files that
/// no longer parsed, silently corrupting any manifest containing an
/// accented character the moment `keron format` rewrote it in place.
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
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            c => out.push(c),
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
    fn null_byte_passes_through_verbatim() {
        // The parser has no `\0` escape, so a NUL can only round-trip
        // as a raw byte.
        assert_eq!(render_cooked_inner("\0"), "\0");
    }

    #[test]
    fn non_ascii_passes_through_verbatim() {
        // Non-ASCII letters are valid raw string content; escaping
        // them to `\u{...}` would produce output the parser rejects.
        assert_eq!(render_cooked_inner("é"), "é");
    }

    #[test]
    fn ascii_control_chars_pass_through_verbatim() {
        // U+0007 (bell) has no keron escape; it round-trips raw.
        assert_eq!(render_cooked_inner("\u{0007}"), "\u{0007}");
    }

    #[test]
    fn space_is_preserved_verbatim() {
        assert_eq!(render_cooked_inner("a b c"), "a b c");
    }

    #[test]
    fn only_parser_supported_escapes_are_emitted() {
        // Guard against regressing to an escape the parser can't read.
        // The output must contain no `\u` or `\0` sequences.
        let rendered = render_cooked_inner("mixed: café \u{0007} \0 tab\tnewline\n");
        assert!(!rendered.contains("\\u"), "must not emit \\u escapes");
        assert!(!rendered.contains("\\0"), "must not emit \\0 escapes");
    }
}
