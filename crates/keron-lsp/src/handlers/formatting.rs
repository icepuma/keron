//! `textDocument/formatting` — reuse the `keron format` engine as one
//! whole-document text edit.

use lsp_types::{DocumentFormattingParams, Position, Range, TextEdit};

use crate::state::ServerState;

/// Format the *current* buffer text (not the last-good snapshot — the
/// user asked to format what they see). Unparseable buffers return
/// `None`: diagnostics already explain the problem, and erroring the
/// request would just make editors pop noise.
pub fn handle(state: &ServerState, params: &DocumentFormattingParams) -> Option<Vec<TextEdit>> {
    let (_, doc) = state.doc_by_uri(&params.text_document.uri)?;
    let formatted = keron_lang::format(&doc.text).ok()?;
    if formatted == doc.text {
        return Some(Vec::new());
    }
    let end = doc.line_index.end_position(&doc.text, state.encoding);
    Some(vec![TextEdit {
        range: Range::new(Position::new(0, 0), end),
        new_text: formatted,
    }])
}
