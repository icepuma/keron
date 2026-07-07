//! keron-lsp: a synchronous Language Server for keron.
//!
//! Transport is `lsp-server` (the rust-analyzer stack) over stdio —
//! **stdout belongs to the protocol**; never print to it. Analysis is
//! fully synchronous per change: keron modules are dotfile-sized, so a
//! whole-workspace re-resolve (parse + import resolution + type check)
//! completes in well under a frame and the single-threaded main loop
//! needs no debounce, no snapshots, no cancellation.
//!
//! Layering:
//! - [`state`]: open documents + recovered parse snapshots + the most
//!   recent module-graph resolution.
//! - [`analysis`]: overlay file loader + `resolve_with_loader` +
//!   publish-delta computation.
//! - [`handlers`]: pure request/notification handlers over the state.

mod analysis;
mod handlers;
mod line_index;
mod state;
mod uri;

use anyhow::Result;
use lsp_server::{Connection, Message};
use lsp_types::{
    CompletionOptions, HoverProviderCapability, OneOf, PositionEncodingKind,
    SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions,
    SemanticTokensServerCapabilities, ServerCapabilities, SignatureHelpOptions,
    TextDocumentSyncCapability, TextDocumentSyncKind,
};

use line_index::PositionEncoding;
use state::ServerState;

/// Run the language server over stdio until the client disconnects or
/// sends `exit`. This call blocks; it is the whole body of
/// `keron lsp`.
///
/// # Errors
/// Returns transport-level failures (broken pipe, protocol violation
/// during the initialize handshake). A clean client shutdown is `Ok`.
pub fn run_stdio_server() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();
    let result = run_with_connection(&connection);
    // The writer thread only exits once every channel sender is gone;
    // dropping the connection before joining is what lets the process
    // terminate after the client's `exit` notification.
    drop(connection);
    io_threads.join()?;
    result
}

/// Run the server on an arbitrary connection — the in-memory seam the
/// end-to-end tests use via `Connection::memory()`.
///
/// # Errors
/// Same contract as [`run_stdio_server`].
pub fn run_with_connection(connection: &Connection) -> Result<()> {
    let (id, init_params) = connection.initialize_start()?;
    let encoding = serde_json::from_value::<lsp_types::InitializeParams>(init_params)
        .map_or(PositionEncoding::Utf16, |p| negotiate_encoding(&p));
    let result = lsp_types::InitializeResult {
        capabilities: server_capabilities(encoding),
        server_info: None,
    };
    connection.initialize_finish(id, serde_json::to_value(result)?)?;
    main_loop(connection, encoding)
}

/// Prefer UTF-8 columns (byte offsets — free for us) when the client
/// offered them; fall back to the mandatory UTF-16 otherwise.
fn negotiate_encoding(params: &lsp_types::InitializeParams) -> PositionEncoding {
    let offers_utf8 = params
        .capabilities
        .general
        .as_ref()
        .and_then(|g| g.position_encodings.as_ref())
        .is_some_and(|encodings| encodings.contains(&PositionEncodingKind::UTF8));
    if offers_utf8 {
        PositionEncoding::Utf8
    } else {
        PositionEncoding::Utf16
    }
}

fn server_capabilities(encoding: PositionEncoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(match encoding {
            PositionEncoding::Utf16 => PositionEncodingKind::UTF16,
            PositionEncoding::Utf8 => PositionEncodingKind::UTF8,
        }),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string()]),
            ..Default::default()
        }),
        definition_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: None,
            work_done_progress_options: lsp_types::WorkDoneProgressOptions::default(),
        }),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: handlers::semantic_tokens::TOKEN_TYPES.to_vec(),
                    token_modifiers: Vec::new(),
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: None,
                work_done_progress_options: lsp_types::WorkDoneProgressOptions::default(),
            },
        )),
        ..Default::default()
    }
}

fn main_loop(connection: &Connection, encoding: PositionEncoding) -> Result<()> {
    let mut state = ServerState {
        encoding,
        ..Default::default()
    };
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                let response = handlers::handle_request(&state, req);
                connection.sender.send(Message::Response(response))?;
            }
            Message::Notification(not) => {
                for out in handlers::handle_notification(&mut state, not) {
                    connection.sender.send(Message::Notification(out))?;
                }
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}
