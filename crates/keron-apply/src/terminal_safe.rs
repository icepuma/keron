//! Sanitization for strings rendered to a terminal.
//!
//! A `.keron` manifest can carry arbitrary bytes inside string
//! literals (paths, package names, template content, addresses).
//! When those land in the diff renderer's plan, the elevated
//! child's warning stream, or an anyhow error chain printed by
//! `main`, the visible output is at the mercy of the terminal:
//! a hostile manifest with `\r`, `\x1b[A`, or U+202E can rewrite
//! the rendered output and forge a benign-looking plan / error.
//!
//! This module centralizes the escape policy so every user-visible
//! sink uses the same rules:
//!
//! - Backslash, double-quote, newline, tab, carriage return — the
//!   familiar string-literal escapes.
//! - Every other ASCII control byte and DEL (`\x00`..`\x1F`, `\x7F`)
//!   — rendered as `\u{HHHH}`.
//! - Unicode directional-isolate / bidi-override controls — same.
//!
//! Paths arrive as `Path::display()` (which preserves control
//! bytes); we run the resulting string through the same escape so a
//! hostile filename can't leak through.

use std::fmt::Write as _;
use std::path::Path;

/// Escape a free-form string so it cannot inject ANSI / control /
/// bidi sequences into a terminal.
#[must_use]
pub fn escape_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || is_bidi_control(c) => {
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Sanitize a path for display. `Path::display()` preserves control
/// bytes verbatim; route them through [`escape_inline`].
#[must_use]
pub fn show_path(path: &Path) -> String {
    escape_inline(&path.display().to_string())
}

/// Sanitize a free-form `String` (resource address, package name,
/// any user-supplied text) for terminal display.
#[must_use]
pub fn show_str(s: &str) -> String {
    escape_inline(s)
}

/// Strip control / bidi sequences from an already-formatted
/// terminal message.
///
/// (e.g., an `anyhow` chain via `{e:?}`). The formatter chooses
/// where to put line breaks; this only rewrites inner control
/// bytes that would change the apparent contents of each line.
/// Newlines and tabs are kept as the formatter emits them, but
/// inline ANSI / `\r` / bidi overrides become escapes so a
/// hostile string inside the chain can't redraw the message.
#[must_use]
pub fn sanitize_terminal_message(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            // Preserve formatter-emitted whitespace.
            '\n' | '\t' => out.push(ch),
            // `\r` on its own is a cursor-return; treat as control.
            c if c.is_control() || is_bidi_control(c) => {
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

const fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{200E}' | '\u{200F}' // LRM, RLM
        | '\u{202A}'..='\u{202E}' // LRE, RLE, PDF, LRO, RLO
        | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_inline_preserves_safe_text() {
        assert_eq!(escape_inline("hello world"), "hello world");
    }

    #[test]
    fn escape_inline_handles_common_escapes() {
        assert_eq!(escape_inline("a\\b"), "a\\\\b");
        assert_eq!(escape_inline("\"x\""), "\\\"x\\\"");
        assert_eq!(escape_inline("\nline"), "\\nline");
        assert_eq!(escape_inline("\rback"), "\\rback");
        assert_eq!(escape_inline("\ttab"), "\\ttab");
    }

    #[test]
    fn escape_inline_neutralizes_ansi_and_nul() {
        assert_eq!(escape_inline("\x1b[2K"), "\\u{001b}[2K");
        assert_eq!(escape_inline("\0null"), "\\u{0000}null");
    }

    #[test]
    fn escape_inline_neutralizes_bidi_override() {
        assert_eq!(escape_inline("good\u{202e}lave"), "good\\u{202e}lave");
    }

    #[test]
    fn sanitize_terminal_message_keeps_newlines_and_tabs() {
        let raw = "line1\nline2\tcol";
        assert_eq!(sanitize_terminal_message(raw), raw);
    }

    #[test]
    fn sanitize_terminal_message_neutralizes_cr_and_ansi() {
        let raw = "Error: bad path \"/etc/passwd\rmalicious\" \x1b[2K";
        let out = sanitize_terminal_message(raw);
        assert!(
            !out.contains('\r'),
            "carriage return must be escaped: {out:?}"
        );
        assert!(!out.contains('\x1b'), "ESC must be escaped: {out:?}");
        assert!(out.contains("\\u{000d}"));
        assert!(out.contains("\\u{001b}"));
    }
}
