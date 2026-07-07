//! `textDocument/inlayHint` — inferred types on unannotated top-level
//! `val`s (from the checker's `val_types` byproduct) and parameter
//! names on positional call arguments.

use keron_lang::Expr;
use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, InlayHintParams, Position};

use crate::analysis::node_at::walk_exprs;
use crate::handlers::snapshot_at;
use crate::state::ServerState;

pub fn handle(state: &ServerState, params: &InlayHintParams) -> Option<Vec<InlayHint>> {
    let snap = snapshot_at(state, &params.text_document.uri)?;
    let range_start = snap.index.offset(snap.text, params.range.start, snap.enc)?;
    let range_end = snap.index.offset(snap.text, params.range.end, snap.enc)?;
    let in_range = |offset: usize| offset >= range_start && offset <= range_end;
    let mut hints = Vec::new();

    // Inferred val types come from the *checked* module, whose spans
    // are valid only when it was built from the same text the
    // snapshot holds (a mid-edit buffer has a partial parse and a
    // failed check — no hints then, which is fine).
    if let Some(module) = snap.module()
        && module.source == snap.text
    {
        for (&(_, end), ty) in &module.val_types {
            if !in_range(end) {
                continue;
            }
            hints.push(hint(
                snap.index.position(snap.text, end, snap.enc),
                format!(": {ty}"),
                InlayHintKind::TYPE,
                false,
            ));
        }
    }

    // Parameter names for positional arguments, from signatures we
    // can resolve without inference.
    let imported = snap.imported_symbols();
    walk_exprs(snap.program, &mut |e| {
        let Expr::Call { callee, args } = &e.node else {
            return;
        };
        let params_sig: Vec<String> =
            crate::analysis::symbols::find_local_def(snap.program, &callee.node, callee.span.start)
                .and_then(|def| match def {
                    crate::analysis::symbols::LocalDef::Fn(f) => {
                        Some(f.params.iter().map(|p| p.name.node.clone()).collect())
                    }
                    _ => None,
                })
                .or_else(|| {
                    imported
                        .fns
                        .get(&callee.node)
                        .map(|sig| sig.params.iter().map(|p| p.name.clone()).collect())
                })
                .unwrap_or_default();
        for (i, arg) in args.iter().enumerate() {
            if arg.name.is_some() || !in_range(arg.value.span.start) {
                continue;
            }
            if let Some(param_name) = params_sig.get(i) {
                hints.push(hint(
                    snap.index
                        .position(snap.text, arg.value.span.start, snap.enc),
                    format!("{param_name} = "),
                    InlayHintKind::PARAMETER,
                    true,
                ));
            }
        }
    });

    hints.sort_by_key(|h| (h.position.line, h.position.character));
    Some(hints)
}

const fn hint(
    position: Position,
    label: String,
    kind: InlayHintKind,
    pad_right: bool,
) -> InlayHint {
    InlayHint {
        position,
        label: InlayHintLabel::String(label),
        kind: Some(kind),
        text_edits: None,
        tooltip: None,
        padding_left: Some(false),
        padding_right: Some(pad_right),
        data: None,
    }
}
