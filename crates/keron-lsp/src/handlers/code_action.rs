//! `textDocument/codeAction` — quickfixes derived from the checker's
//! own suggestions.
//!
//! keron diagnostics carry a rustc-style ``help: did you mean `X`?``
//! line (appended to the message on the way out — see
//! [`super::diagnostics`]). The client echoes the diagnostics for the
//! requested range back in the params, so the fix is stateless: parse
//! the suggestion out of the message and offer a text edit replacing
//! the diagnostic's span with it.

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, TextEdit, WorkspaceEdit,
};

use crate::state::ServerState;

pub fn handle(state: &ServerState, params: &CodeActionParams) -> Vec<CodeActionOrCommand> {
    let _ = state;
    params
        .context
        .diagnostics
        .iter()
        .filter_map(|diag| {
            let suggestion = extract_suggestion(&diag.message)?;
            let edit = TextEdit {
                range: diag.range,
                new_text: suggestion.clone(),
            };
            // clippy's mutable-key-type fires on Uri's interior
            // cell; the map is write-once here, never rehashed after
            // mutation.
            #[allow(clippy::mutable_key_type)]
            let changes =
                std::collections::HashMap::from([(params.text_document.uri.clone(), vec![edit])]);
            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Replace with `{suggestion}`"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diag.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                is_preferred: Some(true),
                ..Default::default()
            }))
        })
        .collect()
}

/// Pull `X` out of a ``did you mean `X`?`` suggestion anywhere in the
/// diagnostic message.
fn extract_suggestion(message: &str) -> Option<String> {
    let idx = message.find("did you mean `")?;
    let rest = &message[idx + "did you mean `".len()..];
    let end = rest.find('`')?;
    let suggestion = &rest[..end];
    (!suggestion.is_empty()).then(|| suggestion.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_suggestion_from_help_line() {
        assert_eq!(
            extract_suggestion("unknown identifier `naem`\nhelp: did you mean `name`?"),
            Some("name".to_string())
        );
        assert_eq!(extract_suggestion("plain error"), None);
        assert_eq!(extract_suggestion("did you mean ``?"), None);
    }
}
