//! User-facing diagnostics produced by the parser and checker.

use crate::ast::Span;

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct Diagnostic {
    pub span: Span,
    pub message: String,
    /// Supplementary context the renderer prints as an ariadne `note:`.
    /// Use this to surface a static fact about the construct (e.g.
    /// "`val` bindings always require a value") that helps the user
    /// understand *why* the message fired.
    pub note: Option<String>,
    /// A user-actionable suggestion the renderer prints as ariadne's
    /// `help:`. Prefer this for "write X here" or "rename to Y"
    /// guidance — concrete next steps the reader can act on.
    pub help: Option<String>,
}

impl Diagnostic {
    #[must_use]
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            note: None,
            help: None,
        }
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}
