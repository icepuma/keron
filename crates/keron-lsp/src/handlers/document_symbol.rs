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
        .filter_map(|item| item_symbol(item, snap.text, snap.index))
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
) -> Option<DocumentSymbol> {
    let range = index.range(text, &item.span());
    match item {
        Item::Fn(f) => Some(symbol(
            f.name.node.clone(),
            Some(render::fn_decl_signature(f)),
            SymbolKind::FUNCTION,
            range,
            index.range(text, &f.name.span),
            None,
        )),
        Item::Val(v) => Some(symbol(
            v.name.node.clone(),
            Some(render::val_decl_signature(v)),
            SymbolKind::CONSTANT,
            range,
            index.range(text, &v.name.span),
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
                        index.range(text, &f.span),
                        index.range(text, &f.name.span),
                        None,
                    )
                })
                .collect();
            Some(symbol(
                s.name.node.clone(),
                Some(render::struct_decl_signature(s)),
                SymbolKind::STRUCT,
                range,
                index.range(text, &s.name.span),
                Some(children),
            ))
        }
        Item::TypeAlias(t) => Some(symbol(
            t.name.node.clone(),
            Some(render::type_alias_signature(t)),
            SymbolKind::ENUM,
            range,
            index.range(text, &t.name.span),
            None,
        )),
        Item::Reconcile(r) => Some(symbol(
            "reconcile".to_string(),
            None,
            SymbolKind::EVENT,
            range,
            index.range(text, &r.span),
            None,
        )),
        Item::Use(_) | Item::ExprStmt(_) => None,
    }
}
