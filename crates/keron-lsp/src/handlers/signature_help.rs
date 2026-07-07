//! `textDocument/signatureHelp` — parameter hints inside a call's
//! argument list, with named arguments mapped to their parameter.

use keron_lang::{FnSig, ParamSig, Program};
use lsp_types::{
    Documentation, ParameterInformation, ParameterLabel, SignatureHelp, SignatureHelpParams,
    SignatureInformation,
};

use crate::analysis::node_at::{CallCtx, enclosing_call};
use crate::analysis::symbols::{LocalDef, find_local_def};
use crate::handlers::render;
use crate::handlers::snapshot_at;
use crate::state::ServerState;

pub fn handle(state: &ServerState, params: &SignatureHelpParams) -> Option<SignatureHelp> {
    let pos = &params.text_document_position_params;
    let snap = snapshot_at(state, &pos.text_document.uri)?;
    let offset = snap.index.offset(snap.text, pos.position, snap.enc)?;
    let ctx = enclosing_call(snap.program, offset)?;
    let sig = resolve_sig(
        snap.program,
        &ctx.callee.node,
        offset,
        &snap.imported_symbols(),
    )?;

    let label = render::fn_sig_signature(&ctx.callee.node, &sig);
    let parameters: Vec<ParameterInformation> = sig
        .params
        .iter()
        .map(|p| ParameterInformation {
            label: ParameterLabel::Simple(render::param_label(p)),
            documentation: None,
        })
        .collect();
    let active_parameter = active_param(&ctx, &sig.params);
    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: None::<Documentation>,
            parameters: Some(parameters),
            active_parameter,
        }],
        active_signature: Some(0),
        active_parameter,
    })
}

/// The callee's signature: a local fn/struct converted on the fly, or
/// an imported/builtin [`FnSig`].
fn resolve_sig(
    program: &Program,
    name: &str,
    offset: usize,
    imported: &keron_lang::ImportedSymbols,
) -> Option<FnSig> {
    match find_local_def(program, name, offset) {
        Some(LocalDef::Fn(f)) => {
            return Some(FnSig {
                struct_name: None,
                params: f
                    .params
                    .iter()
                    .map(|p| ParamSig {
                        name: p.name.node.clone(),
                        ty: p.ty.node.clone(),
                        has_default: p.default.is_some(),
                    })
                    .collect(),
                return_type: f.return_type.node.clone(),
            });
        }
        Some(LocalDef::Struct(_)) | None => {}
        _ => return None,
    }
    imported.fns.get(name).cloned()
}

/// Named arguments select their parameter by name; positional ones by
/// position. Past-the-end positions clamp to the last parameter.
fn active_param(ctx: &CallCtx<'_>, params: &[ParamSig]) -> Option<u32> {
    if params.is_empty() {
        return None;
    }
    let index = ctx
        .args
        .get(ctx.active)
        .and_then(|arg| arg.name.as_ref())
        .and_then(|n| params.iter().position(|p| p.name == n.node))
        .unwrap_or_else(|| ctx.active.min(params.len() - 1));
    u32::try_from(index).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        Position, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
    };

    use crate::handlers::test_support::{pos_of, state_with_doc};
    use crate::state::ServerState;

    fn help_at(state: &ServerState, uri: &lsp_types::Uri, position: Position) -> SignatureHelp {
        let params = SignatureHelpParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            context: None,
        };
        handle(state, &params).expect("signature help available")
    }

    #[test]
    fn positional_args_track_the_cursor() {
        let src = "fn two(a: Int, b: Int): Int { a + b }\nval n: Int = two(1, 2)\n";
        let (state, uri) = state_with_doc(src, src);
        let help = help_at(&state, &uri, pos_of(src, "two(1", 4));
        assert_eq!(help.signatures.len(), 1);
        assert_eq!(help.signatures[0].label, "fn two(a: Int, b: Int): Int");
        assert_eq!(help.active_parameter, Some(0));
        let help = help_at(&state, &uri, pos_of(src, ", 2", 2));
        assert_eq!(help.active_parameter, Some(1));
    }

    #[test]
    fn named_args_map_to_their_parameter() {
        // `target` is the *second* declared param of symlink; naming it
        // first must highlight index 1, not 0.
        let src = "val s: Symlink = symlink(target = \"b\", source = \"a\")\nreconcile s\n";
        let (state, uri) = state_with_doc(src, src);
        let help = help_at(&state, &uri, pos_of(src, "target", 3));
        assert!(help.signatures[0].label.starts_with("fn symlink("));
        assert_eq!(help.active_parameter, Some(1));
        let help = help_at(&state, &uri, pos_of(src, "source", 3));
        assert_eq!(help.active_parameter, Some(0));
    }
}
