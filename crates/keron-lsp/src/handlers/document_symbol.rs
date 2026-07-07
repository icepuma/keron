//! `textDocument/documentSymbol` — the outline view: top-level
//! declarations, with struct fields as children.

use keron_lang::Item;
use lsp_types::{DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, SymbolKind};

use crate::handlers::{render, snapshot_at};
use crate::state::ServerState;

pub fn handle(
    state: &ServerState,
    params: &DocumentSymbolParams,
) -> Option<DocumentSymbolResponse> {
    let snap = snapshot_at(state, &params.text_document.uri)?;
    let symbols: Vec<DocumentSymbol> = snap
        .program
        .items
        .iter()
        .filter_map(|item| item_symbol(item, snap.text, snap.index, snap.enc))
        .collect();
    Some(DocumentSymbolResponse::Nested(symbols))
}

// lsp-types deprecates `DocumentSymbol::deprecated` in favor of
// `tags`, but struct construction still requires the field.
#[allow(deprecated)]
const fn symbol(
    name: String,
    detail: Option<String>,
    kind: SymbolKind,
    range: lsp_types::Range,
    selection_range: lsp_types::Range,
    children: Option<Vec<DocumentSymbol>>,
) -> DocumentSymbol {
    DocumentSymbol {
        name,
        detail,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children,
    }
}

fn item_symbol(
    item: &Item,
    text: &str,
    index: &crate::line_index::LineIndex,
    enc: crate::line_index::PositionEncoding,
) -> Option<DocumentSymbol> {
    let range = index.range(text, &item.span(), enc);
    match item {
        Item::Fn(f) => Some(symbol(
            f.name.node.clone(),
            Some(render::fn_decl_signature(f)),
            SymbolKind::FUNCTION,
            range,
            index.range(text, &f.name.span, enc),
            None,
        )),
        Item::Val(v) => Some(symbol(
            v.name.node.clone(),
            Some(render::val_decl_signature(v)),
            SymbolKind::CONSTANT,
            range,
            index.range(text, &v.name.span, enc),
            None,
        )),
        Item::Struct(s) => {
            let children: Vec<DocumentSymbol> = s
                .fields
                .iter()
                .map(|f| {
                    symbol(
                        f.name.node.clone(),
                        Some(f.ty.node.to_string()),
                        SymbolKind::FIELD,
                        index.range(text, &f.span, enc),
                        index.range(text, &f.name.span, enc),
                        None,
                    )
                })
                .collect();
            Some(symbol(
                s.name.node.clone(),
                Some(render::struct_decl_signature(s)),
                SymbolKind::STRUCT,
                range,
                index.range(text, &s.name.span, enc),
                Some(children),
            ))
        }
        Item::TypeAlias(t) => Some(symbol(
            t.name.node.clone(),
            Some(render::type_alias_signature(t)),
            SymbolKind::ENUM,
            range,
            index.range(text, &t.name.span, enc),
            None,
        )),
        Item::Reconcile(r) => Some(symbol(
            "reconcile".to_string(),
            None,
            SymbolKind::EVENT,
            range,
            index.range(text, &r.span, enc),
            None,
        )),
        Item::Use(_) | Item::ExprStmt(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{PartialResultParams, TextDocumentIdentifier, WorkDoneProgressParams};

    use crate::handlers::test_support::state_with_doc;

    #[test]
    fn outline_covers_every_item_kind() {
        let src = "from \"./lib.keron\" use ignored\n\
                   fn f(a: Int): Int { a }\n\
                   val v: Int = 1\n\
                   struct P { x: Int, y: String }\n\
                   type C = \"red\" | \"blue\"\n\
                   reconcile symlink(source = \"a\", target = \"b\")\n";
        // The `use` line references a file that doesn't exist; symbols
        // come from the last-good parse alone, so that's fine.
        let (state, uri) = state_with_doc(src, src);
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let Some(DocumentSymbolResponse::Nested(symbols)) = handle(&state, &params) else {
            panic!("expected nested symbols");
        };
        let outline: Vec<String> = symbols
            .iter()
            .map(|s| {
                let children = s.children.as_ref().map_or(0, Vec::len);
                format!("{:?} {} ({} children)", s.kind, s.name, children)
            })
            .collect();
        insta::assert_snapshot!(outline.join("\n"), @r"
        Function f (0 children)
        Constant v (0 children)
        Struct P (2 children)
        Enum C (0 children)
        Event reconcile (0 children)
        ");
        let p = &symbols[2];
        assert_eq!(p.children.as_ref().unwrap()[0].name, "x");
        assert!(
            p.range.start <= p.selection_range.start && p.selection_range.end <= p.range.end,
            "selection_range must sit inside range"
        );
    }
}
