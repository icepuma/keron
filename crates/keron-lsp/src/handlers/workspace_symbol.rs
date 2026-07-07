//! `workspace/symbol` — top-level declarations across every module in
//! the latest resolution, filtered by a case-insensitive substring
//! query (the standard cheap fuzzy for small workspaces).

use keron_lang::Item;
use lsp_types::{Location, SymbolInformation, SymbolKind, WorkspaceSymbolParams};

use crate::line_index::LineIndex;
use crate::state::ServerState;
use crate::uri::path_to_uri;

pub fn handle(
    state: &ServerState,
    params: &WorkspaceSymbolParams,
) -> Option<Vec<SymbolInformation>> {
    let resolution = state.resolution.as_ref()?;
    let query = params.query.to_lowercase();
    let mut symbols = Vec::new();
    for (id, module) in &resolution.graph.modules {
        let Some(uri) = path_to_uri(&id.0) else {
            continue;
        };
        let index = LineIndex::new(&module.source);
        for item in &module.program.items {
            let (name, kind, span) = match item {
                Item::Fn(f) => (&f.name.node, SymbolKind::FUNCTION, &f.name.span),
                Item::Val(v) => (&v.name.node, SymbolKind::CONSTANT, &v.name.span),
                Item::Struct(s) => (&s.name.node, SymbolKind::STRUCT, &s.name.span),
                Item::TypeAlias(t) => (&t.name.node, SymbolKind::ENUM, &t.name.span),
                _ => continue,
            };
            if !query.is_empty() && !name.to_lowercase().contains(&query) {
                continue;
            }
            // lsp-types keeps the deprecated `deprecated` field on
            // SymbolInformation; construction still requires it.
            #[allow(deprecated)]
            symbols.push(SymbolInformation {
                name: name.clone(),
                kind,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri.clone(),
                    range: index.range(&module.source, span, state.encoding),
                },
                container_name: None,
            });
        }
    }
    symbols.sort_by(|a, b| a.name.cmp(&b.name));
    Some(symbols)
}
