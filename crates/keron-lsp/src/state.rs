//! In-memory server state: open documents, last-good parse snapshots,
//! the latest module-graph resolution, and publish bookkeeping.

use std::collections::HashMap;
use std::path::PathBuf;

use keron_lang::Program;
use keron_modules::Resolution;
use lsp_types::Uri;

use crate::line_index::{LineIndex, PositionEncoding};

/// One open editor document. Keyed in [`ServerState::docs`] by its
/// canonicalized filesystem path (the same key the module resolver
/// uses), so overlay lookups and diagnostic fan-out agree on identity.
#[derive(Debug)]
pub struct Document {
    pub uri: Uri,
    pub version: i32,
    pub text: String,
    pub line_index: LineIndex,
    /// The latest parse of this document — with parser recovery, a
    /// broken buffer still yields a partial AST whose spans are valid
    /// against the *current* text (broken items are simply absent).
    /// `None` only before the first didOpen refresh.
    pub parsed: Option<Parsed>,
}

/// A (possibly partial) parse of a document, kept alongside the exact
/// text and line index its spans refer to.
#[derive(Debug)]
pub struct Parsed {
    pub program: Program,
    pub text: String,
    pub line_index: LineIndex,
}

/// The last diagnostics payload sent for one URI. The document
/// version is part of its identity: unchanged text ranges on a newer
/// buffer still need a new publication so clients do not retain a
/// payload tied to an obsolete snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDiagnostics {
    pub version: Option<i32>,
    pub diagnostics: Vec<lsp_types::Diagnostic>,
}

/// Whole-server mutable state, owned by the single-threaded main loop.
#[derive(Debug, Default)]
pub struct ServerState {
    /// Negotiated at initialize; every position on the wire uses it.
    pub encoding: PositionEncoding,
    /// Whether the client renders `$1`-style snippet completions.
    pub snippet_support: bool,
    pub docs: HashMap<PathBuf, Document>,
    /// Result of the most recent [`keron_modules::resolve_with_loader`]
    /// run over all open documents. Feature handlers read the graph;
    /// `None` only before the first didOpen.
    pub resolution: Option<Resolution>,
    /// Diagnostics published in the previous round, keyed by URI
    /// string. Lets the next round skip unchanged sets and push empty
    /// arrays to URIs whose diagnostics disappeared.
    pub published: HashMap<String, PublishedDiagnostics>,
}

impl ServerState {
    /// Look up the open document backing `uri`, together with its
    /// canonical path key.
    #[must_use]
    pub fn doc_by_uri(&self, uri: &Uri) -> Option<(&PathBuf, &Document)> {
        self.docs.iter().find(|(_, d)| d.uri == *uri)
    }
}
