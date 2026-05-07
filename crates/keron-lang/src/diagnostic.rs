//! User-facing diagnostics produced by the parser and checker.

use crate::ast::Span;

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct Diagnostic {
    pub span: Span,
    pub message: String,
}

impl Diagnostic {
    #[must_use]
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}
