//! Byte-offset ⇄ LSP position conversion.
//!
//! keron spans are byte ranges into UTF-8 source; the LSP wire format
//! speaks zero-based line numbers plus code-unit columns in the
//! *negotiated* encoding — UTF-16 by default (the only one every
//! client must support), UTF-8 when the client offered it at
//! initialize (cheaper: columns are plain byte offsets).
//! [`LineIndex`] precomputes line starts once per document version so
//! each conversion only scans a single line.

use lsp_types::Position;

use keron_lang::Span;

/// The position encoding negotiated at initialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PositionEncoding {
    /// Columns are UTF-16 code units (the mandatory LSP default).
    #[default]
    Utf16,
    /// Columns are UTF-8 bytes.
    Utf8,
}

/// Byte offsets of every line start in one document snapshot. Valid
/// only for the exact text it was built from — rebuild on every edit.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// `line_starts[i]` is the byte offset where line `i` begins;
    /// `line_starts[0] == 0` always, even for empty text.
    line_starts: Vec<usize>,
}

impl LineIndex {
    #[must_use]
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        let bytes = text.as_bytes();
        for (index, byte) in bytes.iter().copied().enumerate() {
            if byte == b'\n' || (byte == b'\r' && bytes.get(index + 1) != Some(&b'\n')) {
                line_starts.push(index + 1);
            }
        }
        Self { line_starts }
    }

    /// Convert a byte offset into `text` (the text this index was
    /// built from) to an LSP position. Offsets past the end of the
    /// text clamp to the final position; offsets inside a multi-byte
    /// character round down to that character's start.
    #[must_use]
    pub fn position(&self, text: &str, offset: usize, enc: PositionEncoding) -> Position {
        let offset = clamp_to_char_boundary(text, offset);
        let line = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let line_start = self.line_starts[line];
        let offset = offset.min(self.line_content_end(text, line));
        let character = match enc {
            PositionEncoding::Utf16 => text[line_start..offset]
                .chars()
                .map(char::len_utf16)
                .sum::<usize>(),
            PositionEncoding::Utf8 => offset - line_start,
        };
        Position {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            character: u32::try_from(character).unwrap_or(u32::MAX),
        }
    }

    /// Convert a keron span into an `lsp_types::Range`.
    #[must_use]
    pub fn range(&self, text: &str, span: &Span, enc: PositionEncoding) -> lsp_types::Range {
        lsp_types::Range {
            start: self.position(text, span.start, enc),
            end: self.position(text, span.end, enc),
        }
    }

    /// Convert an LSP position back to a byte offset into `text`. A
    /// `character` beyond the end of its line clamps to the line end
    /// (LSP-specified behavior); a `line` beyond the last line
    /// returns `None`.
    #[must_use]
    pub fn offset(&self, text: &str, position: Position, enc: PositionEncoding) -> Option<usize> {
        let line_start = *self.line_starts.get(position.line as usize)?;
        let line_end = self.line_content_end(text, position.line as usize);
        let line_text = &text[line_start..line_end];
        match enc {
            PositionEncoding::Utf16 => {
                let mut units_left = position.character as usize;
                for (i, c) in line_text.char_indices() {
                    if units_left < c.len_utf16() {
                        return Some(line_start + i);
                    }
                    units_left -= c.len_utf16();
                }
                Some(line_end)
            }
            PositionEncoding::Utf8 => {
                let byte = (position.character as usize).min(line_text.len());
                Some(clamp_to_char_boundary(text, line_start + byte))
            }
        }
    }

    /// The position one past the last character — the exclusive end of
    /// a whole-document range (used by full-text formatting edits).
    #[must_use]
    pub fn end_position(&self, text: &str, enc: PositionEncoding) -> Position {
        self.position(text, text.len(), enc)
    }

    fn line_content_end(&self, text: &str, line: usize) -> usize {
        let Some(&next) = self.line_starts.get(line + 1) else {
            return text.len();
        };
        let newline = next - 1;
        if newline > 0 && text.as_bytes()[newline - 1] == b'\r' {
            newline - 1
        } else {
            newline
        }
    }
}

fn clamp_to_char_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    use PositionEncoding::{Utf8, Utf16};

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn ascii_positions() {
        let text = "ab\ncd\n";
        let idx = LineIndex::new(text);
        for enc in [Utf16, Utf8] {
            assert_eq!(idx.position(text, 0, enc), pos(0, 0));
            assert_eq!(idx.position(text, 2, enc), pos(0, 2));
            assert_eq!(idx.position(text, 3, enc), pos(1, 0));
            assert_eq!(idx.position(text, 5, enc), pos(1, 2));
            assert_eq!(idx.position(text, 6, enc), pos(2, 0));
        }
    }

    #[test]
    fn multibyte_counts_utf16_units() {
        // 'é' = 2 bytes / 1 UTF-16 unit; '🎉' = 4 bytes / 2 units.
        let text = "é🎉x";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 2, Utf16), pos(0, 1));
        assert_eq!(idx.position(text, 6, Utf16), pos(0, 3));
        assert_eq!(idx.offset(text, pos(0, 1), Utf16), Some(2));
        assert_eq!(idx.offset(text, pos(0, 3), Utf16), Some(6));
    }

    #[test]
    fn multibyte_counts_utf8_bytes() {
        let text = "é🎉x";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 2, Utf8), pos(0, 2));
        assert_eq!(idx.position(text, 6, Utf8), pos(0, 6));
        assert_eq!(idx.offset(text, pos(0, 2), Utf8), Some(2));
        assert_eq!(idx.offset(text, pos(0, 6), Utf8), Some(6));
        // A byte column inside a char rounds down to its start.
        assert_eq!(idx.offset(text, pos(0, 3), Utf8), Some(2));
    }

    #[test]
    fn offset_inside_multibyte_char_rounds_down() {
        let text = "🎉";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 2, Utf16), pos(0, 0));
        assert_eq!(idx.position(text, 2, Utf8), pos(0, 0));
    }

    #[test]
    fn crlf_terminator_is_excluded_from_line_columns() {
        let text = "ab\r\ncd";
        let idx = LineIndex::new(text);
        for enc in [Utf16, Utf8] {
            assert_eq!(idx.position(text, 4, enc), pos(1, 0));
            assert_eq!(idx.position(text, 2, enc), pos(0, 2));
            assert_eq!(idx.position(text, 3, enc), pos(0, 2));
            assert_eq!(idx.offset(text, pos(0, 99), enc), Some(2));
            assert_eq!(idx.offset(text, pos(1, 0), enc), Some(4));
        }
    }

    #[test]
    fn carriage_return_terminator_starts_a_new_line() {
        let text = "ab\rcd\r";
        let idx = LineIndex::new(text);
        for enc in [Utf16, Utf8] {
            assert_eq!(idx.position(text, 2, enc), pos(0, 2));
            assert_eq!(idx.position(text, 3, enc), pos(1, 0));
            assert_eq!(idx.position(text, 5, enc), pos(1, 2));
            assert_eq!(idx.position(text, 6, enc), pos(2, 0));
            assert_eq!(idx.offset(text, pos(0, 99), enc), Some(2));
            assert_eq!(idx.offset(text, pos(1, 0), enc), Some(3));
            assert_eq!(idx.offset(text, pos(2, 0), enc), Some(6));
        }
    }

    #[test]
    fn character_past_line_end_clamps_to_line_end() {
        let text = "ab\ncd";
        let idx = LineIndex::new(text);
        for enc in [Utf16, Utf8] {
            assert_eq!(idx.offset(text, pos(0, 99), enc), Some(2));
            assert_eq!(idx.offset(text, pos(1, 99), enc), Some(5));
        }
    }

    #[test]
    fn line_past_end_returns_none() {
        let text = "ab";
        let idx = LineIndex::new(text);
        assert_eq!(idx.offset(text, pos(5, 0), Utf16), None);
        assert_eq!(idx.offset(text, pos(5, 0), Utf8), None);
    }

    #[test]
    fn empty_text_has_one_line() {
        let text = "";
        let idx = LineIndex::new(text);
        for enc in [Utf16, Utf8] {
            assert_eq!(idx.position(text, 0, enc), pos(0, 0));
            assert_eq!(idx.offset(text, pos(0, 0), enc), Some(0));
            assert_eq!(idx.end_position(text, enc), pos(0, 0));
        }
    }

    proptest! {
        #[test]
        fn roundtrip_offset_position_offset(text in "\\PC*(\n\\PC*){0,5}", frac in 0usize..10_000, utf8 in proptest::bool::ANY) {
            let enc = if utf8 { Utf8 } else { Utf16 };
            // Pick an arbitrary char boundary and require an exact
            // roundtrip through Position.
            let mut offset = frac % (text.len() + 1);
            while offset > 0 && !text.is_char_boundary(offset) {
                offset -= 1;
            }
            let idx = LineIndex::new(&text);
            let position = idx.position(&text, offset, enc);
            prop_assert_eq!(idx.offset(&text, position, enc), Some(offset));
        }

        #[test]
        fn position_is_total_and_offset_never_panics(text in ".*", offset in 0usize..200, utf8 in proptest::bool::ANY) {
            let enc = if utf8 { Utf8 } else { Utf16 };
            let idx = LineIndex::new(&text);
            let position = idx.position(&text, offset, enc);
            let _ = idx.offset(&text, position, enc);
        }
    }
}
