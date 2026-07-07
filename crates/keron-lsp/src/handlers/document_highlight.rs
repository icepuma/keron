//! `textDocument/documentHighlight` — same-document occurrences of
//! the symbol under the cursor. Reuses the reference collector,
//! scoped to the current document only.

use lsp_types::{DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams};

use crate::analysis::node_at::{NodeRef, node_at};
use crate::analysis::refs::{HitKind, collect_name_hits, resolves_to};
use crate::analysis::symbols::find_local_def;
use crate::handlers::snapshot_at;
use crate::state::ServerState;

pub fn handle(
    state: &ServerState,
    params: &DocumentHighlightParams,
) -> Option<Vec<DocumentHighlight>> {
    let pos = &params.text_document_position_params;
    let snap = snapshot_at(state, &pos.text_document.uri)?;
    let offset = snap.index.offset(snap.text, pos.position, snap.enc)?;
    let name = match node_at(snap.program, offset)? {
        NodeRef::Callee(n) => n.node.clone(),
        NodeRef::Var { name, .. } | NodeRef::TypeName { name, .. } => name.to_string(),
        NodeRef::UseName { name } => name.node.clone(),
        NodeRef::FnName(f) => f.name.node.clone(),
        NodeRef::ValName(v) => v.name.node.clone(),
        NodeRef::StructName(s) => s.name.node.clone(),
        NodeRef::TypeAliasName(t) => t.name.node.clone(),
        NodeRef::ParamName(p) => p.name.node.clone(),
        _ => return None,
    };
    // Value occurrences respect shadowing when the cursor's own
    // occurrence resolves locally; builtins and imports fall back to
    // plain name matching, which is exact for the unshadowed case.
    let def_span = find_local_def(snap.program, &name, offset).map(|d| d.name_span());
    let highlights = collect_name_hits(snap.program, snap.text, &name)
        .into_iter()
        .filter(|hit| match (&def_span, hit.kind) {
            (Some(def), HitKind::VarRef | HitKind::Shorthand) => {
                resolves_to(snap.program, &name, hit.span.start, def)
            }
            (Some(def), HitKind::Decl) => hit.span == *def,
            _ => true,
        })
        .map(|hit| DocumentHighlight {
            range: snap.index.range(snap.text, &hit.span, snap.enc),
            kind: Some(if hit.kind == HitKind::Decl {
                DocumentHighlightKind::WRITE
            } else {
                DocumentHighlightKind::READ
            }),
        })
        .collect();
    Some(highlights)
}
