//! Map keron `Span`s (byte offsets in the source text) to LSP
//! `Range`s (line + UTF-16 code unit offset).
//!
//! The LSP default position encoding is UTF-16. Translating from byte
//! offsets requires re-walking the prefix of the line and counting
//! UTF-16 code units; we precompute the line-start offsets to keep
//! per-diagnostic work bounded by the length of the offending line.

use keron_lang::Span;
use lsp_types::{Position, Range};

// `pub(super)` is the right visibility (the type is internal to this
// crate's `lib.rs`), but clippy's `redundant_pub_crate` reads it as
// crate-visible because the module sits at the crate root. Suppress
// rather than promote to `pub`, which would trip `unreachable_pub`.
#[allow(clippy::redundant_pub_crate)]
pub(super) struct LineIndex {
    /// Byte offset of the start of each line, sorted ascending.
    /// Always begins with `0`; one entry per `\n` plus the implicit
    /// first line.
    line_starts: Vec<usize>,
    source: String,
}

#[allow(clippy::redundant_pub_crate)]
impl LineIndex {
    pub(super) fn new(source: &str) -> Self {
        let mut line_starts = vec![0usize];
        for (offset, ch) in source.char_indices() {
            if ch == '\n' {
                line_starts.push(offset + 1);
            }
        }
        Self {
            line_starts,
            source: source.to_string(),
        }
    }

    pub(super) fn span_to_range(&self, span: &Span) -> Range {
        Range {
            start: self.offset_to_position(span.start),
            end: self.offset_to_position(span.end),
        }
    }

    fn offset_to_position(&self, offset: usize) -> Position {
        let clamped = offset.min(self.source.len());
        let line = self
            .line_starts
            .partition_point(|&start| start <= clamped)
            .saturating_sub(1);
        let line_start = self.line_starts[line];
        let line_text = &self.source[line_start..clamped];
        let character = line_text.encode_utf16().count();
        Position {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            character: u32::try_from(character).unwrap_or(u32::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn first_line_offsets() {
        let idx = LineIndex::new("hello world");
        assert_eq!(idx.offset_to_position(0), pos(0, 0));
        assert_eq!(idx.offset_to_position(5), pos(0, 5));
        assert_eq!(idx.offset_to_position(11), pos(0, 11));
    }

    #[test]
    fn multiline_offsets() {
        let idx = LineIndex::new("ab\ncd\nef");
        assert_eq!(idx.offset_to_position(0), pos(0, 0));
        assert_eq!(idx.offset_to_position(2), pos(0, 2)); // end of first line
        assert_eq!(idx.offset_to_position(3), pos(1, 0)); // start of second
        assert_eq!(idx.offset_to_position(5), pos(1, 2));
        assert_eq!(idx.offset_to_position(6), pos(2, 0));
    }

    #[test]
    fn out_of_bounds_clamps_to_end() {
        let idx = LineIndex::new("ab");
        assert_eq!(idx.offset_to_position(100), pos(0, 2));
    }

    #[test]
    fn utf16_counts_characters_not_bytes() {
        // `é` is two UTF-8 bytes but one UTF-16 code unit.
        let idx = LineIndex::new("é-x");
        // Byte 3 lands just past `é-`; that's two UTF-16 units.
        assert_eq!(idx.offset_to_position(3), pos(0, 2));
    }

    #[test]
    fn surrogate_pair_counts_two_units() {
        // `🦀` (U+1F980) is one Unicode scalar but encodes as a
        // surrogate pair in UTF-16 — i.e. two units.
        let idx = LineIndex::new("🦀x");
        assert_eq!(idx.offset_to_position("🦀".len()), pos(0, 2));
    }

    #[test]
    fn span_to_range_spans_two_lines() {
        let idx = LineIndex::new("foo\nbar");
        let range = idx.span_to_range(&(2..6));
        assert_eq!(range.start, pos(0, 2));
        assert_eq!(range.end, pos(1, 2));
    }
}
