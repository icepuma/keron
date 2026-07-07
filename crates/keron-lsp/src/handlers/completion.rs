//! `textDocument/completion` — a flat identifier list: keywords,
//! stdlib builtins, module-local declarations, and imports. One
//! context refinement: inside a `from "…" use …` names list only the
//! target module's exports are offered.

use std::fs;

use keron_lang::{Item, Type};
use lsp_types::{CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse};

use crate::handlers::{Snapshot, render, snapshot_at};
use crate::state::ServerState;

const KEYWORDS: &[&str] = &[
    "val",
    "fn",
    "struct",
    "type",
    "reconcile",
    "from",
    "use",
    "if",
    "else",
    "for",
    "in",
    "match",
    "true",
    "false",
    "null",
];

pub fn handle(state: &ServerState, params: &CompletionParams) -> Option<CompletionResponse> {
    let pos = &params.text_document_position;
    let snap = snapshot_at(state, &pos.text_document.uri)?;
    let offset = snap.index.offset(snap.text, pos.position)?;
    if let Some(items) = use_names_completion(&snap, offset) {
        return Some(CompletionResponse::Array(items));
    }
    Some(CompletionResponse::Array(scope_completion(&snap)))
}

/// When the cursor sits in the names list of a `use` item, offer only
/// what the target module actually exports.
fn use_names_completion(snap: &Snapshot<'_>, offset: usize) -> Option<Vec<CompletionItem>> {
    let use_decl = snap.program.items.iter().find_map(|item| match item {
        Item::Use(u)
            if u.span.start <= offset && offset <= u.span.end && offset > u.source.span.end =>
        {
            Some(u)
        }
        _ => None,
    })?;
    let resolution = snap.resolution?;
    let base = snap.path.parent()?;
    let target = fs::canonicalize(base.join(&use_decl.source.node)).ok()?;
    let module = resolution
        .graph
        .modules
        .get(&keron_modules::ModuleId(target))?;
    let mut items = Vec::new();
    for name in &module.exported_fns {
        items.push(item(name, CompletionItemKind::FUNCTION, None));
    }
    for name in &module.exported_vals {
        items.push(item(name, CompletionItemKind::VARIABLE, None));
    }
    for (name, ty) in &module.exported_types {
        items.push(item(name, type_kind(ty), None));
    }
    dedup(&mut items);
    Some(items)
}

fn scope_completion(snap: &Snapshot<'_>) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = KEYWORDS
        .iter()
        .map(|kw| item(kw, CompletionItemKind::KEYWORD, None))
        .collect();

    // Everything imported into scope: stdlib builtins plus resolved
    // `use` items, with real signatures as detail text.
    let imported = snap.imported_symbols();
    for (name, sig) in &imported.fns {
        items.push(item(
            name,
            if sig.struct_name.is_some() {
                CompletionItemKind::STRUCT
            } else {
                CompletionItemKind::FUNCTION
            },
            Some(render::fn_sig_signature(name, sig)),
        ));
    }
    for (name, ty) in &imported.types {
        items.push(item(name, type_kind(ty), Some(ty.to_string())));
    }
    for (name, ty) in &imported.vals {
        items.push(item(
            name,
            CompletionItemKind::VARIABLE,
            Some(format!("val {name}: {ty}")),
        ));
    }

    // Module-local declarations.
    for decl in &snap.program.items {
        match decl {
            Item::Fn(f) => items.push(item(
                &f.name.node,
                CompletionItemKind::FUNCTION,
                Some(render::fn_decl_signature(f)),
            )),
            Item::Val(v) => items.push(item(
                &v.name.node,
                CompletionItemKind::VARIABLE,
                Some(render::val_decl_signature(v)),
            )),
            Item::Struct(s) => items.push(item(
                &s.name.node,
                CompletionItemKind::STRUCT,
                Some(render::struct_decl_signature(s)),
            )),
            Item::TypeAlias(t) => items.push(item(
                &t.name.node,
                CompletionItemKind::ENUM,
                Some(render::type_alias_signature(t)),
            )),
            _ => {}
        }
    }

    dedup(&mut items);
    items
}

const fn type_kind(ty: &Type) -> CompletionItemKind {
    match ty {
        Type::StringUnion { .. } => CompletionItemKind::ENUM,
        Type::Struct { .. } => CompletionItemKind::STRUCT,
        _ => CompletionItemKind::CLASS,
    }
}

fn item(label: &str, kind: CompletionItemKind, detail: Option<String>) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        detail,
        ..Default::default()
    }
}

/// Local declarations also appear via `imported_symbols` only when
/// imported elsewhere, but a name can still show up twice (e.g. local
/// decl + stale import). Keep the first occurrence, which is the one
/// with the most specific detail.
fn dedup(items: &mut Vec<CompletionItem>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert(i.label.clone()));
    items.sort_by(|a, b| a.label.cmp(&b.label));
}
