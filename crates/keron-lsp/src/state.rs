//! In-memory server state: open documents, last-good parse snapshots,
//! the latest module-graph resolution, and publish bookkeeping.

use std::collections::HashMap;
use std::path::PathBuf;

use keron_lang::Program;
use keron_modules::Resolution;
use lsp_types::Uri;

use crate::line_index::LineIndex;

/// One open editor document. Keyed in [`ServerState::docs`] by its
/// canonicalized filesystem path (the same key the module resolver
/// uses), so overlay lookups and diagnostic fan-out agree on identity.
#[derive(Debug)]
pub struct Document {
    pub uri: Uri,
    pub version: i32,
    pub text: String,
    pub line_index: LineIndex,
    /// The most recent snapshot whose parse succeeded. Feature
    /// requests (hover, completion, symbols) fall back to this while
    /// the live text is mid-edit and unparseable; spans in
    /// `last_good.program` are only valid against `last_good.text`.
    pub last_good: Option<LastGood>,
}

/// A parseable snapshot of a document, kept alongside the exact text
/// and line index its spans refer to.
#[derive(Debug)]
pub struct LastGood {
    pub program: Program,
    pub text: String,
    pub line_index: LineIndex,
}

/// Whole-server mutable state, owned by the single-threaded main loop.
#[derive(Debug, Default)]
pub struct ServerState {
    pub docs: HashMap<PathBuf, Document>,
    /// Result of the most recent [`keron_modules::resolve_with_loader`]
    /// run over all open documents. Feature handlers read the graph;
    /// `None` only before the first didOpen.
    pub resolution: Option<Resolution>,
    /// Diagnostics published in the previous round, keyed by URI
    /// string. Lets the next round skip unchanged sets and push empty
    /// arrays to URIs whose diagnostics disappeared.
    pub published: HashMap<String, Vec<lsp_types::Diagnostic>>,
}

impl ServerState {
    /// Look up the open document backing `uri`, together with its
    /// canonical path key.
    #[must_use]
    pub fn doc_by_uri(&self, uri: &Uri) -> Option<(&PathBuf, &Document)> {
        self.docs.iter().find(|(_, d)| d.uri == *uri)
    }
}
