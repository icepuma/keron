//! keron-lsp: stdio-based language server.
//!
//! Drives `keron-lang`'s parse + check pipeline on every edit and
//! publishes the resulting diagnostics. Capabilities exposed today:
//!
//! - `textDocument/didOpen` / `didChange` / `didClose` (full sync)
//! - `textDocument/publishDiagnostics`
//!
//! Anything else replies `MethodNotFound`. Hover, completion, and
//! goto-definition are deliberately out of scope until the parse +
//! check loop is solid in editors.

use std::collections::HashMap;

use anyhow::{Context, Result};
use lsp_server::{Connection, ExtractError, Message, Notification, Response, ResponseError};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, PublishDiagnosticsParams, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
    notification::{
        DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
        PublishDiagnostics,
    },
};

mod span;

/// Entry point: connect over stdio, perform the LSP handshake, then
/// run the message loop until the client requests shutdown.
///
/// # Errors
/// Returns the underlying `anyhow::Error` if any I/O, JSON, or
/// internal LSP step fails.
pub fn run() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();
    let server_capabilities =
        serde_json::to_value(server_capabilities()).context("serializing server capabilities")?;
    let _initialize_params = connection
        .initialize(server_capabilities)
        .context("LSP initialize handshake")?;
    main_loop(&connection)?;
    // The writer IO thread parks on `connection.sender`; dropping the
    // connection releases the channel so the thread can exit, which
    // is what `io_threads.join()` is waiting on.
    drop(connection);
    io_threads.join().context("LSP IO threads")?;
    Ok(())
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        ..Default::default()
    }
}

fn main_loop(connection: &Connection) -> Result<()> {
    // `Uri` carries internal interior mutability (a fluent-uri parse
    // cache), so clippy refuses it as a `HashMap` key. We key by the
    // URI's string form instead.
    let mut docs: HashMap<String, String> = HashMap::new();
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                let resp = Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: lsp_server::ErrorCode::MethodNotFound as i32,
                        message: format!("method `{}` not implemented", req.method),
                        data: None,
                    }),
                };
                connection.sender.send(Message::Response(resp))?;
            }
            Message::Notification(notif) => {
                handle_notification(connection, &mut docs, notif)?;
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

fn handle_notification(
    connection: &Connection,
    docs: &mut HashMap<String, String>,
    notif: Notification,
) -> Result<()> {
    let method = notif.method.clone();
    match method.as_str() {
        DidOpenTextDocument::METHOD => {
            let params: DidOpenTextDocumentParams = notif.extract(&method).map_err(extract_err)?;
            let uri = params.text_document.uri;
            let text = params.text_document.text;
            docs.insert(uri.as_str().to_string(), text.clone());
            publish_diagnostics(connection, &uri, &text)?;
        }
        DidChangeTextDocument::METHOD => {
            let params: DidChangeTextDocumentParams =
                notif.extract(&method).map_err(extract_err)?;
            let uri = params.text_document.uri;
            // FULL sync: each change carries the entire document; the
            // last entry wins if a client batches multiple.
            if let Some(change) = params.content_changes.into_iter().next_back() {
                docs.insert(uri.as_str().to_string(), change.text.clone());
                publish_diagnostics(connection, &uri, &change.text)?;
            }
        }
        DidCloseTextDocument::METHOD => {
            let params: DidCloseTextDocumentParams = notif.extract(&method).map_err(extract_err)?;
            docs.remove(params.text_document.uri.as_str());
        }
        _ => {}
    }
    Ok(())
}

fn publish_diagnostics(connection: &Connection, uri: &Uri, text: &str) -> Result<()> {
    let diagnostics = compute_diagnostics(text);
    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics,
        version: None,
    };
    let notif = Notification {
        method: PublishDiagnostics::METHOD.to_string(),
        params: serde_json::to_value(params).context("serializing PublishDiagnosticsParams")?,
    };
    connection.sender.send(Message::Notification(notif))?;
    Ok(())
}

fn compute_diagnostics(text: &str) -> Vec<Diagnostic> {
    let lines = span::LineIndex::new(text);
    let keron_diags = match keron_lang::parse(text) {
        Ok(prog) => match keron_lang::check(&prog) {
            Ok(()) => return Vec::new(),
            Err(diags) => diags,
        },
        Err(diags) => diags,
    };
    keron_diags
        .into_iter()
        .map(|d| keron_to_lsp(&d, &lines))
        .collect()
}

fn keron_to_lsp(d: &keron_lang::Diagnostic, lines: &span::LineIndex) -> Diagnostic {
    Diagnostic {
        range: lines.span_to_range(&d.span),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("keron".into()),
        message: d.message.clone(),
        ..Default::default()
    }
}

fn extract_err(err: ExtractError<Notification>) -> anyhow::Error {
    match err {
        ExtractError::JsonError { method, error } => {
            anyhow::anyhow!("decoding `{method}` params failed: {error}")
        }
        ExtractError::MethodMismatch(notif) => {
            anyhow::anyhow!("method mismatch: got `{}`", notif.method)
        }
    }
}
