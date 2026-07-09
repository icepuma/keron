//! Whole-workspace analysis: run the module resolver over every open
//! document (overlaying unsaved buffer text over disk) and turn the
//! per-module errors into `publishDiagnostics` payloads.

pub mod node_at;
pub mod refs;
pub mod symbols;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use keron_modules::{DiskLoader, EntrySource, FileLoader, ModuleId, resolve_with_loader};
use lsp_types::{PublishDiagnosticsParams, Uri};

use crate::handlers::diagnostics::to_lsp;
use crate::line_index::LineIndex;
use crate::state::{Document, PublishedDiagnostics, ServerState};
use crate::uri::path_to_uri;

/// [`FileLoader`] that prefers open-buffer text and falls back to
/// disk, so `use`-imported modules see unsaved edits.
struct OverlayLoader<'a> {
    docs: &'a HashMap<PathBuf, Document>,
}

impl FileLoader for OverlayLoader<'_> {
    fn read_to_string(&self, path: &Path) -> Result<String, String> {
        if let Some(doc) = self.docs.get(path) {
            return Ok(doc.text.clone());
        }
        DiskLoader.read_to_string(path)
    }
}

/// Re-resolve all open documents, store the new [`keron_modules::Resolution`]
/// on `state`, and return only the `publishDiagnostics` payloads that
/// changed since the previous round (including empty payloads for
/// URIs whose diagnostics disappeared).
pub fn analyze(state: &mut ServerState) -> Vec<PublishDiagnosticsParams> {
    let roots: Vec<EntrySource> = state
        .docs
        .iter()
        .map(|(path, doc)| EntrySource {
            text: doc.text.clone(),
            base_dir: path.parent().map_or_else(PathBuf::new, Path::to_path_buf),
            id: ModuleId(path.clone()),
        })
        .collect();
    let resolution = resolve_with_loader(roots, &OverlayLoader { docs: &state.docs });

    let mut fresh: HashMap<String, (Uri, PublishedDiagnostics)> = HashMap::new();
    for err in &resolution.errors {
        let open_doc = state.docs.get(&err.module.0);
        let Some(uri) = open_doc
            .map(|doc| doc.uri.clone())
            .or_else(|| path_to_uri(&err.module.0))
        else {
            continue;
        };
        let source = resolution
            .sources
            .get(&err.module)
            .map_or("", String::as_str);
        let index = LineIndex::new(source);
        let entry = fresh.entry(uri.as_str().to_string()).or_insert_with(|| {
            (
                uri,
                PublishedDiagnostics {
                    version: open_doc.map(|doc| doc.version),
                    diagnostics: Vec::new(),
                },
            )
        });
        for diag in &err.diagnostics {
            entry
                .1
                .diagnostics
                .push(to_lsp(diag, source, &index, state.encoding));
        }
    }
    for (_, published) in fresh.values_mut() {
        published
            .diagnostics
            .sort_by_key(|d| (d.range.start, d.range.end));
        published.diagnostics.dedup();
    }
    state.resolution = Some(resolution);

    let mut out = Vec::new();
    for (uri_str, (uri, published)) in &fresh {
        if state.published.get(uri_str) != Some(published) {
            out.push(PublishDiagnosticsParams {
                uri: uri.clone(),
                diagnostics: published.diagnostics.clone(),
                version: published.version,
            });
        }
    }
    for uri_str in state.published.keys() {
        if !fresh.contains_key(uri_str)
            && let Ok(uri) = Uri::from_str(uri_str)
        {
            let version = state
                .docs
                .values()
                .find(|doc| doc.uri.as_str() == uri_str)
                .map(|doc| doc.version);
            out.push(PublishDiagnosticsParams {
                uri,
                diagnostics: Vec::new(),
                version,
            });
        }
    }
    state.published = fresh
        .into_iter()
        .map(|(key, (_, published))| (key, published))
        .collect();
    out
}
