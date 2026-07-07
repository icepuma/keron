//! `textDocument/definition` — local declarations, cross-file imports,
//! and `use`-path targets.

use std::fs;

use keron_modules::CheckedModule;
use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, Location, Position, Range};

use crate::analysis::node_at::{NodeRef, node_at};
use crate::analysis::symbols::{find_local_def, top_level_decl_span};
use crate::handlers::{Snapshot, snapshot_at};
use crate::line_index::LineIndex;
use crate::state::ServerState;
use crate::uri::path_to_uri;

pub fn handle(
    state: &ServerState,
    params: &GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let pos = &params.text_document_position_params;
    let snap = snapshot_at(state, &pos.text_document.uri)?;
    let offset = snap.index.offset(snap.text, pos.position)?;
    let location = match node_at(snap.program, offset)? {
        NodeRef::Callee(name) => named_def(&snap, &name.node, offset),
        NodeRef::Var { name, .. } | NodeRef::TypeName { name, .. } => {
            named_def(&snap, name, offset)
        }
        NodeRef::UseName { name, .. } => imported_def(&snap, &name.node),
        NodeRef::UsePath(u) => use_path_target(&snap, &u.source.node),
        // Already at the definition.
        _ => None,
    }?;
    Some(GotoDefinitionResponse::Scalar(location))
}

fn named_def(snap: &Snapshot<'_>, name: &str, offset: usize) -> Option<Location> {
    if let Some(def) = find_local_def(snap.program, name, offset) {
        return Some(Location {
            uri: snap.doc.uri.clone(),
            range: snap.index.range(snap.text, &def.name_span()),
        });
    }
    imported_def(snap, name)
}

/// Follow `name` through this module's resolved imports to the
/// declaration in its origin module. Builtins have no source to jump
/// to and return `None`.
fn imported_def(snap: &Snapshot<'_>, name: &str) -> Option<Location> {
    let resolution = snap.resolution?;
    let module = snap.module()?;
    let (origin_id, original_name) = module.imports.get(name)?;
    let origin: &CheckedModule = resolution.graph.modules.get(origin_id)?;
    let span = top_level_decl_span(&origin.program, original_name)?;
    let index = LineIndex::new(&origin.source);
    Some(Location {
        uri: path_to_uri(&origin_id.0)?,
        range: index.range(&origin.source, &span),
    })
}

/// `from "./x.keron" …` with the cursor on the path: jump to the top
/// of the target file.
fn use_path_target(snap: &Snapshot<'_>, raw_path: &str) -> Option<Location> {
    let base = snap.path.parent()?;
    let target = fs::canonicalize(base.join(raw_path)).ok()?;
    Some(Location {
        uri: path_to_uri(&target)?,
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
    })
}
