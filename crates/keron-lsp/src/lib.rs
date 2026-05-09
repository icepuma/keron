//! keron-lsp: stdio-based language server.
//!
//! Drives the `keron-modules` resolver on every edit (treating the
//! changed document as its own entry) and publishes the resulting
//! diagnostics for that document. Imports of other `.keron` files
//! and of the stdlib are resolved live from disk on each edit.
//!
//! Capabilities exposed today:
//!
//! - `textDocument/didOpen` / `didChange` / `didClose` (full sync)
//! - `textDocument/publishDiagnostics`
//!
//! Anything else replies `MethodNotFound`. Hover, completion, and
//! goto-definition are deliberately out of scope until the parse +
//! check loop is solid in editors. Cross-module diagnostics for
//! imported files are dropped today; a follow-up will publish those
//! to their own URIs.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use keron_modules::{EntrySource, ModuleId, ResolveError, resolve};
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
    let diagnostics = compute_diagnostics(uri, text);
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

fn compute_diagnostics(uri: &Uri, text: &str) -> Vec<Diagnostic> {
    let lines = span::LineIndex::new(text);
    let Some(path) = uri_to_path(uri) else {
        // Untitled / non-file URIs: fall back to the standalone
        // checker (no imports). Programs containing `use` items will
        // surface "unknown function" errors via the type checker.
        let keron_diags = standalone_diagnostics(text);
        return keron_diags
            .into_iter()
            .map(|d| keron_to_lsp(&d, &lines))
            .collect();
    };
    let base_dir = path.parent().unwrap_or(&path).to_path_buf();
    let entry_id = ModuleId::File(path);
    let Err(errors) = resolve(EntrySource {
        text: text.to_string(),
        base_dir,
        id: entry_id.clone(),
    }) else {
        return Vec::new();
    };
    diagnostics_for_entry(&errors, &entry_id, &lines)
}

fn standalone_diagnostics(text: &str) -> Vec<keron_lang::Diagnostic> {
    match keron_lang::parse(text) {
        Ok(prog) => match keron_lang::check(&prog) {
            Ok(()) => Vec::new(),
            Err(diags) => diags,
        },
        Err(diags) => diags,
    }
}

fn diagnostics_for_entry(
    errors: &[ResolveError],
    entry: &ModuleId,
    lines: &span::LineIndex,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for err in errors {
        if &err.module != entry {
            // Diagnostics belonging to imported files would need to
            // publish to *their* URIs; today we drop them rather
            // than misattribute them to the entry document.
            continue;
        }
        for d in &err.diagnostics {
            out.push(keron_to_lsp(d, lines));
        }
    }
    out
}

fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    // `lsp_types::Uri` re-exports `fluent_uri::Uri`. We only handle
    // `file:` schemes; anything else is a non-filesystem buffer.
    let s = uri.as_str();
    let rest = s.strip_prefix("file://")?;
    // Strip a leading host (none, in practice) up to the next `/`.
    let path_start = rest.find('/').unwrap_or(0);
    let path_part = &rest[path_start..];
    let decoded = percent_decode(path_part);
    Some(PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                #[allow(clippy::cast_possible_truncation)]
                {
                    out.push((h * 16 + l) as u8);
                }
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn percent_decode_passes_plain_ascii_through() {
        assert_eq!(percent_decode("/abs/path.keron"), "/abs/path.keron");
    }

    #[test]
    fn percent_decode_handles_space_escape() {
        assert_eq!(percent_decode("/has%20space.keron"), "/has space.keron");
    }

    #[test]
    fn percent_decode_handles_high_byte() {
        // 0xFF is valid as a single byte but not valid UTF-8 on its
        // own; `from_utf8_lossy` substitutes the replacement char.
        let got = percent_decode("a%FFb");
        assert!(got.starts_with('a') && got.ends_with('b'));
        assert_ne!(got, "a%FFb"); // the escape was decoded
    }

    #[test]
    fn percent_decode_handles_two_escapes_in_sequence() {
        assert_eq!(percent_decode("%2F%2F"), "//");
    }

    #[test]
    fn percent_decode_skips_truncated_escape_at_end() {
        // `%2` has no third byte; treat as literal.
        assert_eq!(percent_decode("/foo%2"), "/foo%2");
    }

    #[test]
    fn percent_decode_passes_invalid_hex_through() {
        // `%ZZ` is not valid hex — falls through to literal copy.
        assert_eq!(percent_decode("a%ZZb"), "a%ZZb");
    }

    #[test]
    fn percent_decode_empty_input() {
        assert_eq!(percent_decode(""), "");
    }

    fn make_uri(s: &str) -> Uri {
        Uri::from_str(s).expect("valid URI")
    }

    #[test]
    fn uri_to_path_decodes_file_scheme() {
        let uri = make_uri("file:///abs/path.keron");
        assert_eq!(uri_to_path(&uri), Some(PathBuf::from("/abs/path.keron")));
    }

    #[test]
    fn uri_to_path_decodes_percent_escapes() {
        let uri = make_uri("file:///has%20space.keron");
        assert_eq!(uri_to_path(&uri), Some(PathBuf::from("/has space.keron")));
    }

    #[test]
    fn uri_to_path_rejects_non_file_scheme() {
        let uri = make_uri("untitled:Untitled-1");
        assert_eq!(uri_to_path(&uri), None);
    }

    #[test]
    fn standalone_diagnostics_returns_empty_for_well_typed_source() {
        let diags = standalone_diagnostics("val n: Int = 1\n");
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn standalone_diagnostics_returns_parse_errors() {
        let diags = standalone_diagnostics("val n: Int = !!!");
        assert!(!diags.is_empty());
    }

    #[test]
    fn standalone_diagnostics_returns_check_errors() {
        // Type mismatch — parses fine, fails check.
        let diags = standalone_diagnostics("val n: Int = \"hi\"\n");
        assert!(!diags.is_empty());
    }

    #[test]
    fn keron_to_lsp_sets_metadata_fields() {
        let lines = span::LineIndex::new("hello\nworld\n");
        let kd = keron_lang::Diagnostic {
            span: 0..5,
            message: "boom".into(),
        };
        let lsp = keron_to_lsp(&kd, &lines);
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp.source.as_deref(), Some("keron"));
        assert_eq!(lsp.message, "boom");
    }

    #[test]
    fn keron_to_lsp_maps_span_to_range() {
        // Span "world" on the second line — distinct end vs start
        // makes the range mapping observable.
        let text = "hello\nworld\n";
        let lines = span::LineIndex::new(text);
        let kd = keron_lang::Diagnostic {
            span: 6..11,
            message: "boom".into(),
        };
        let lsp = keron_to_lsp(&kd, &lines);
        assert_eq!(lsp.range.start.line, 1);
        assert_eq!(lsp.range.start.character, 0);
        assert_eq!(lsp.range.end.line, 1);
        assert_eq!(lsp.range.end.character, 5);
    }

    #[test]
    fn server_capabilities_advertises_full_text_sync() {
        let caps = server_capabilities();
        match caps.text_document_sync {
            Some(TextDocumentSyncCapability::Kind(kind)) => {
                assert_eq!(kind, TextDocumentSyncKind::FULL);
            }
            other => panic!("expected FULL text sync, got {other:?}"),
        }
    }

    fn untitled_uri(name: &str) -> Uri {
        Uri::from_str(&format!("untitled:{name}")).unwrap()
    }

    fn notif(method: &str, params: serde_json::Value) -> Notification {
        Notification {
            method: method.to_string(),
            params,
        }
    }

    fn drain_notifications(conn: &Connection) -> Vec<Notification> {
        let mut out = Vec::new();
        // Non-blocking drain: when the connection has no more buffered
        // messages, `try_recv` returns Empty.
        while let Ok(msg) = conn.receiver.try_recv() {
            if let Message::Notification(n) = msg {
                out.push(n);
            }
        }
        out
    }

    #[test]
    fn did_change_updates_docs_and_publishes_diagnostics() {
        // Pin the `DidChangeTextDocument::METHOD` match arm. Deleting
        // it would skip both the `docs` update and the diagnostics
        // publish — so we assert both observable effects.
        let (server, client) = Connection::memory();
        let mut docs: HashMap<String, String> = HashMap::new();
        let uri = untitled_uri("a");
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri.as_str(),
                "version": 1,
            },
            "contentChanges": [
                { "text": "val n: Int = \"oops\"\n" }
            ],
        });
        handle_notification(
            &server,
            &mut docs,
            notif(DidChangeTextDocument::METHOD, params),
        )
        .unwrap();
        assert_eq!(
            docs.get(uri.as_str()).map(String::as_str),
            Some("val n: Int = \"oops\"\n"),
        );
        let pubs = drain_notifications(&client);
        let diag = pubs
            .iter()
            .find(|n| n.method == PublishDiagnostics::METHOD)
            .expect("publishDiagnostics emitted");
        let payload: PublishDiagnosticsParams =
            serde_json::from_value(diag.params.clone()).unwrap();
        assert!(
            !payload.diagnostics.is_empty(),
            "type error should produce a diagnostic"
        );
    }

    #[test]
    fn did_close_removes_doc_from_state() {
        // Pin the `DidCloseTextDocument::METHOD` arm. Deleting it
        // would leave the doc in `docs`.
        let (server, _client) = Connection::memory();
        let mut docs: HashMap<String, String> = HashMap::new();
        let uri = untitled_uri("b");
        docs.insert(uri.as_str().to_string(), "stale".into());
        let params = serde_json::json!({
            "textDocument": { "uri": uri.as_str() },
        });
        handle_notification(
            &server,
            &mut docs,
            notif(DidCloseTextDocument::METHOD, params),
        )
        .unwrap();
        assert!(
            !docs.contains_key(uri.as_str()),
            "didClose should drop the doc"
        );
    }

    #[test]
    fn did_open_inserts_doc_and_publishes_diagnostics() {
        // Symmetric coverage for DidOpen so future mutation runs that
        // re-target it find a test pinning the behavior.
        let (server, client) = Connection::memory();
        let mut docs: HashMap<String, String> = HashMap::new();
        let uri = untitled_uri("c");
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri.as_str(),
                "languageId": "keron",
                "version": 1,
                "text": "val n: Int = 1\n",
            },
        });
        handle_notification(
            &server,
            &mut docs,
            notif(DidOpenTextDocument::METHOD, params),
        )
        .unwrap();
        assert_eq!(
            docs.get(uri.as_str()).map(String::as_str),
            Some("val n: Int = 1\n"),
        );
        let pubs = drain_notifications(&client);
        assert!(
            pubs.iter().any(|n| n.method == PublishDiagnostics::METHOD),
            "didOpen must publish diagnostics"
        );
    }
}
