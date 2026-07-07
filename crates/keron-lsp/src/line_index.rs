//! Byte-offset ⇄ LSP position conversion.
//!
//! keron spans are byte ranges into UTF-8 source; the LSP wire format
//! speaks zero-based line numbers plus UTF-16 code-unit columns (the
//! only encoding every client must support — the server advertises
//! `PositionEncodingKind::UTF16`). [`LineIndex`] precomputes line
//! starts once per document version so each conversion only scans a
//! single line.

use lsp_types::Position;

use keron_lang::Span;

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
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter(|&(_, b)| b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { line_starts }
    }

    /// Convert a byte offset into `text` (the text this index was
    /// built from) to an LSP UTF-16 position. Offsets past the end of
    /// the text clamp to the final position; offsets inside a
    /// multi-byte character round down to that character's start.
    #[must_use]
    pub fn position(&self, text: &str, offset: usize) -> Position {
        let offset = clamp_to_char_boundary(text, offset);
        let line = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let line_start = self.line_starts[line];
        let character = text[line_start..offset]
            .chars()
            .map(char::len_utf16)
            .sum::<usize>();
        Position {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            character: u32::try_from(character).unwrap_or(u32::MAX),
        }
    }

    /// Convert an LSP span into an `lsp_types::Range`.
    #[must_use]
    pub fn range(&self, text: &str, span: &Span) -> lsp_types::Range {
        lsp_types::Range {
            start: self.position(text, span.start),
            end: self.position(text, span.end),
        }
    }

    /// Convert an LSP UTF-16 position back to a byte offset into
    /// `text`. A `character` beyond the end of its line clamps to the
    /// line end (LSP-specified behavior); a `line` beyond the last
    /// line returns `None`.
    #[must_use]
    pub fn offset(&self, text: &str, position: Position) -> Option<usize> {
        let line_start = *self.line_starts.get(position.line as usize)?;
        let line_end = self
            .line_starts
            .get(position.line as usize + 1)
            .map_or(text.len(), |&next| next - 1);
        let line_text = &text[line_start..line_end];
        let mut units_left = position.character as usize;
        for (i, c) in line_text.char_indices() {
            if units_left < c.len_utf16() {
                return Some(line_start + i);
            }
            units_left -= c.len_utf16();
        }
        Some(line_end)
    }

    /// The position one past the last character — the exclusive end of
    /// a whole-document range (used by full-text formatting edits).
    #[must_use]
    pub fn end_position(&self, text: &str) -> Position {
        self.position(text, text.len())
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

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn ascii_positions() {
        let text = "ab\ncd\n";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 0), pos(0, 0));
        assert_eq!(idx.position(text, 2), pos(0, 2));
        assert_eq!(idx.position(text, 3), pos(1, 0));
        assert_eq!(idx.position(text, 5), pos(1, 2));
        assert_eq!(idx.position(text, 6), pos(2, 0));
    }

    #[test]
    fn multibyte_counts_utf16_units() {
        // 'é' = 2 bytes / 1 UTF-16 unit; '🎉' = 4 bytes / 2 units.
        let text = "é🎉x";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 2), pos(0, 1));
        assert_eq!(idx.position(text, 6), pos(0, 3));
        assert_eq!(idx.offset(text, pos(0, 1)), Some(2));
        assert_eq!(idx.offset(text, pos(0, 3)), Some(6));
    }

    #[test]
    fn offset_inside_multibyte_char_rounds_down() {
        let text = "🎉";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 2), pos(0, 0));
    }

    #[test]
    fn crlf_newline_is_part_of_previous_line() {
        let text = "ab\r\ncd";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 4), pos(1, 0));
        assert_eq!(idx.position(text, 2), pos(0, 2));
        assert_eq!(idx.offset(text, pos(1, 0)), Some(4));
    }

    #[test]
    fn character_past_line_end_clamps_to_line_end() {
        let text = "ab\ncd";
        let idx = LineIndex::new(text);
        assert_eq!(idx.offset(text, pos(0, 99)), Some(2));
        assert_eq!(idx.offset(text, pos(1, 99)), Some(5));
    }

    #[test]
    fn line_past_end_returns_none() {
        let text = "ab";
        let idx = LineIndex::new(text);
        assert_eq!(idx.offset(text, pos(5, 0)), None);
    }

    #[test]
    fn empty_text_has_one_line() {
        let text = "";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 0), pos(0, 0));
        assert_eq!(idx.offset(text, pos(0, 0)), Some(0));
        assert_eq!(idx.end_position(text), pos(0, 0));
    }

    proptest! {
        #[test]
        fn roundtrip_offset_position_offset(text in "\\PC*(\n\\PC*){0,5}", frac in 0usize..10_000) {
            // Pick an arbitrary char boundary and require an exact
            // roundtrip through Position.
            let mut offset = frac % (text.len() + 1);
            while offset > 0 && !text.is_char_boundary(offset) {
                offset -= 1;
            }
            let idx = LineIndex::new(&text);
            let position = idx.position(&text, offset);
            prop_assert_eq!(idx.offset(&text, position), Some(offset));
        }

        #[test]
        fn position_is_total_and_offset_never_panics(text in ".*", offset in 0usize..200) {
            let idx = LineIndex::new(&text);
            let position = idx.position(&text, offset);
            let _ = idx.offset(&text, position);
        }
    }
}
