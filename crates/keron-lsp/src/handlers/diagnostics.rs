//! `keron_lang::Diagnostic` → `lsp_types::Diagnostic` conversion.

use lsp_types::DiagnosticSeverity;

use crate::line_index::{LineIndex, PositionEncoding};

/// Convert one keron diagnostic against the source text its span
/// refers to. keron diagnostics carry no severity — everything the
/// frontend emits is an error. `note:` / `help:` have no spans of
/// their own, so they are appended to the message the way rustc-style
/// servers render span-less children.
pub fn to_lsp(
    diag: &keron_lang::Diagnostic,
    text: &str,
    index: &LineIndex,
    enc: PositionEncoding,
) -> lsp_types::Diagnostic {
    let mut message = diag.message.clone();
    if let Some(note) = &diag.note {
        message.push_str("\nnote: ");
        message.push_str(note);
    }
    if let Some(help) = &diag.help {
        message.push_str("\nhelp: ");
        message.push_str(help);
    }
    lsp_types::Diagnostic {
        range: index.range(text, &diag.span, enc),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("keron".to_string()),
        message,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keron_lang::Diagnostic;
    use lsp_types::Position;

    #[test]
    fn maps_span_and_appends_note_and_help() {
        let text = "val x: Int = \"hi\"\n";
        let index = LineIndex::new(text);
        let diag = Diagnostic::new(13..17, "expected `Int`, found `String`")
            .with_note("the annotation says `Int`")
            .with_help("change the annotation to `String`");
        let lsp = to_lsp(&diag, text, &index, PositionEncoding::Utf16);
        assert_eq!(lsp.range.start, Position::new(0, 13));
        assert_eq!(lsp.range.end, Position::new(0, 17));
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp.source.as_deref(), Some("keron"));
        assert_eq!(
            lsp.message,
            "expected `Int`, found `String`\nnote: the annotation says `Int`\nhelp: change the annotation to `String`"
        );
    }

    #[test]
    fn out_of_range_span_clamps_instead_of_panicking() {
        let text = "x";
        let index = LineIndex::new(text);
        let diag = Diagnostic::new(50..60, "boom");
        let lsp = to_lsp(&diag, text, &index, PositionEncoding::Utf16);
        assert_eq!(lsp.range.start, Position::new(0, 1));
        assert_eq!(lsp.range.end, Position::new(0, 1));
    }
}
