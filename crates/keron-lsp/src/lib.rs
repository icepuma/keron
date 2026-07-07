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
//! - [`state`]: open documents + last-good parse snapshots + the most
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
    let capabilities = serde_json::to_value(server_capabilities())?;
    connection.initialize(capabilities)?;
    main_loop(connection)
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions::default()),
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

fn main_loop(connection: &Connection) -> Result<()> {
    let mut state = ServerState::default();
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
