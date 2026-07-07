//! Request/notification dispatch. Every handler is a pure function
//! over [`ServerState`]; the main loop owns all channel IO.

pub mod completion;
pub mod definition;
pub mod diagnostics;
pub mod document_symbol;
pub mod formatting;
pub mod hover;
pub mod render;
pub mod semantic_tokens;
pub mod signature_help;

use std::fs;
use std::path::PathBuf;

use keron_lang::{ImportedSymbols, Program};
use keron_modules::{CheckedModule, Resolution};
use lsp_server::{ErrorCode, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{
    Completion, DocumentSymbolRequest, Formatting, GotoDefinition, HoverRequest, Request as _,
    SemanticTokensFullRequest, SignatureHelpRequest,
};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    PublishDiagnosticsParams, Uri,
};

use crate::analysis::analyze;
use crate::analysis::symbols::module_for;
use crate::line_index::{LineIndex, PositionEncoding};
use crate::state::{Document, Parsed, ServerState};
use crate::uri::uri_to_path;

/// The read view feature handlers work against: the latest (possibly
/// partial) parse of one document plus the workspace resolution.
/// Spans in `program` are valid against `text`/`index`.
pub struct Snapshot<'s> {
    pub program: &'s Program,
    pub text: &'s str,
    pub index: &'s LineIndex,
    pub doc: &'s Document,
    pub path: &'s PathBuf,
    pub resolution: Option<&'s Resolution>,
    pub enc: PositionEncoding,
}

impl Snapshot<'_> {
    /// This document's module in the latest resolution, if it checked.
    #[must_use]
    pub fn module(&self) -> Option<&CheckedModule> {
        self.resolution.and_then(|r| module_for(r, self.path))
    }

    /// Everything in scope: stdlib builtins plus resolved imports
    /// (stdlib only when the module didn't resolve).
    #[must_use]
    pub fn imported_symbols(&self) -> ImportedSymbols {
        self.resolution
            .and_then(|r| {
                module_for(r, self.path).map(|m| keron_modules::imported_symbols(m, &r.graph))
            })
            .unwrap_or_else(keron_modules::stdlib_symbols)
    }
}

/// Build the feature-request view for `uri`. `None` when the document
/// isn't open.
#[must_use]
pub fn snapshot_at<'s>(state: &'s ServerState, uri: &Uri) -> Option<Snapshot<'s>> {
    let (path, doc) = state.doc_by_uri(uri)?;
    let parsed = doc.parsed.as_ref()?;
    Some(Snapshot {
        program: &parsed.program,
        text: &parsed.text,
        index: &parsed.line_index,
        doc,
        path,
        resolution: state.resolution.as_ref(),
        enc: state.encoding,
    })
}

pub fn handle_request(state: &ServerState, req: Request) -> Response {
    match req.method.as_str() {
        HoverRequest::METHOD => respond(req.id, req.params, |p| hover::handle(state, &p)),
        GotoDefinition::METHOD => respond(req.id, req.params, |p| definition::handle(state, &p)),
        Completion::METHOD => respond(req.id, req.params, |p| completion::handle(state, &p)),
        DocumentSymbolRequest::METHOD => {
            respond(req.id, req.params, |p| document_symbol::handle(state, &p))
        }
        SignatureHelpRequest::METHOD => {
            respond(req.id, req.params, |p| signature_help::handle(state, &p))
        }
        Formatting::METHOD => respond(req.id, req.params, |p| formatting::handle(state, &p)),
        SemanticTokensFullRequest::METHOD => {
            respond(req.id, req.params, |p| semantic_tokens::handle(state, &p))
        }
        _ => Response::new_err(
            req.id,
            ErrorCode::MethodNotFound as i32,
            format!("unhandled method `{}`", req.method),
        ),
    }
}

/// Deserialize params, run the handler, serialize the (optional)
/// result. `None` results serialize to JSON `null` — the protocol's
/// "no answer" for every request this server implements.
fn respond<P, R>(
    id: RequestId,
    params: serde_json::Value,
    f: impl FnOnce(P) -> Option<R>,
) -> Response
where
    P: serde::de::DeserializeOwned,
    R: serde::Serialize,
{
    let Ok(params) = serde_json::from_value::<P>(params) else {
        return Response::new_err(
            id,
            ErrorCode::InvalidParams as i32,
            "malformed params".to_string(),
        );
    };
    match serde_json::to_value(f(params)) {
        Ok(value) => Response::new_ok(id, value),
        Err(e) => Response::new_err(id, ErrorCode::InternalError as i32, e.to_string()),
    }
}

/// Handle one client notification; returns the notifications (fresh
/// diagnostics) the server should send back.
pub fn handle_notification(state: &mut ServerState, not: Notification) -> Vec<Notification> {
    match not.method.as_str() {
        DidOpenTextDocument::METHOD => parsed::<DidOpenTextDocumentParams>(not.params)
            .map_or_else(Vec::new, |p| did_open(state, p)),
        DidChangeTextDocument::METHOD => parsed::<DidChangeTextDocumentParams>(not.params)
            .map_or_else(Vec::new, |p| did_change(state, p)),
        DidCloseTextDocument::METHOD => parsed::<DidCloseTextDocumentParams>(not.params)
            .map_or_else(Vec::new, |p| did_close(state, &p)),
        _ => Vec::new(),
    }
}

/// Malformed params are a client bug; drop the message rather than
/// crash the server over it.
fn parsed<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Option<T> {
    serde_json::from_value(params).ok()
}

/// Canonical filesystem key for a document URI — must match the
/// canonical paths the module resolver produces for `use` targets so
/// the overlay loader finds open buffers. Falls back to the raw path
/// for files that don't exist on disk yet.
fn doc_key(uri: &Uri) -> Option<PathBuf> {
    let path = uri_to_path(uri)?;
    Some(fs::canonicalize(&path).unwrap_or(path))
}

/// Re-parse with recovery: even a broken buffer yields a partial AST
/// (broken items absent), so features track the live text instead of
/// serving stale spans. The diagnostics are dropped here — the
/// resolver re-reports them with module context during [`analyze`].
fn refresh_parsed(doc: &mut Document) {
    let (program, _) = keron_lang::parse_recovering(&doc.text);
    doc.parsed = Some(Parsed {
        program,
        text: doc.text.clone(),
        line_index: doc.line_index.clone(),
    });
}

fn did_open(state: &mut ServerState, params: DidOpenTextDocumentParams) -> Vec<Notification> {
    let Some(key) = doc_key(&params.text_document.uri) else {
        return Vec::new();
    };
    let text = params.text_document.text;
    let mut doc = Document {
        uri: params.text_document.uri,
        version: params.text_document.version,
        line_index: LineIndex::new(&text),
        text,
        parsed: None,
    };
    refresh_parsed(&mut doc);
    state.docs.insert(key, doc);
    publish_notifications(analyze(state))
}

fn did_change(state: &mut ServerState, params: DidChangeTextDocumentParams) -> Vec<Notification> {
    let Some(key) = doc_key(&params.text_document.uri) else {
        return Vec::new();
    };
    let Some(doc) = state.docs.get_mut(&key) else {
        return Vec::new();
    };
    // Full-text sync: the last change event carries the whole buffer.
    let Some(change) = params.content_changes.into_iter().next_back() else {
        return Vec::new();
    };
    doc.text = change.text;
    doc.version = params.text_document.version;
    doc.line_index = LineIndex::new(&doc.text);
    refresh_parsed(doc);
    publish_notifications(analyze(state))
}

fn did_close(state: &mut ServerState, params: &DidCloseTextDocumentParams) -> Vec<Notification> {
    let Some(key) = doc_key(&params.text_document.uri) else {
        return Vec::new();
    };
    if state.docs.remove(&key).is_none() {
        return Vec::new();
    }
    publish_notifications(analyze(state))
}

fn publish_notifications(payloads: Vec<PublishDiagnosticsParams>) -> Vec<Notification> {
    payloads
        .into_iter()
        .map(|p| Notification::new(PublishDiagnostics::METHOD.to_string(), p))
        .collect()
}

#[cfg(test)]
pub mod test_support {
    use std::path::PathBuf;
    use std::str::FromStr;

    use lsp_types::Uri;

    use crate::line_index::LineIndex;
    use crate::state::{Document, Parsed, ServerState};

    /// One open in-memory document at `file:///test/main.keron` whose
    /// last-good snapshot is parsed from `good` while the live buffer
    /// holds `live` — pass the same string twice for a buffer that
    /// parses. No resolution is attached, so scope falls back to
    /// stdlib builtins plus the module's own declarations.
    pub fn state_with_doc(live: &str, good: &str) -> (ServerState, Uri) {
        let uri = Uri::from_str("file:///test/main.keron").expect("static uri");
        let program = keron_lang::parse(good).expect("good text parses");
        let doc = Document {
            uri: uri.clone(),
            version: 1,
            text: live.to_string(),
            line_index: LineIndex::new(live),
            parsed: Some(Parsed {
                program,
                text: good.to_string(),
                line_index: LineIndex::new(good),
            }),
        };
        let mut state = ServerState::default();
        state.docs.insert(PathBuf::from("/test/main.keron"), doc);
        (state, uri)
    }

    /// Zero-based LSP position of `needle`'s first byte plus `add`
    /// columns, assuming single-byte characters around the needle.
    pub fn pos_of(src: &str, needle: &str, add: u32) -> lsp_types::Position {
        let offset = src.find(needle).expect("needle present");
        let line = src[..offset].matches('\n').count();
        let line_start = src[..offset].rfind('\n').map_or(0, |i| i + 1);
        let character = u32::try_from(offset - line_start).expect("short lines") + add;
        lsp_types::Position::new(u32::try_from(line).expect("short files"), character)
    }
}
