//! `textDocument/completion` — context-aware where cheap, flat where
//! not: `receiver.` offers the receiver's struct fields, a struct
//! literal's braces offer its missing fields, a `from "…" use …`
//! names list offers the target module's exports, and everything else
//! gets the full scope (keywords, stdlib builtins with docs,
//! module-local declarations, imports).

use std::collections::HashSet;
use std::fs;

use keron_lang::{Item, Type};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse, Documentation,
    MarkupContent, MarkupKind,
};

use crate::analysis::node_at::enclosing_struct_literal;
use crate::analysis::symbols::var_struct_fields;
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
    // `receiver.` detection scans the *live* buffer text — a trailing
    // dot usually means the snapshot is one keystroke behind.
    if let Some(live_offset) = snap
        .doc
        .line_index
        .offset(&snap.doc.text, pos.position, snap.enc)
        && let Some(items) = dot_completion(&snap, live_offset)
    {
        return Some(CompletionResponse::Array(items));
    }
    let offset = snap.index.offset(snap.text, pos.position, snap.enc)?;
    if let Some(items) = struct_literal_completion(&snap, offset) {
        return Some(CompletionResponse::Array(items));
    }
    if let Some(items) = use_names_completion(&snap, offset) {
        return Some(CompletionResponse::Array(items));
    }
    Some(CompletionResponse::Array(scope_completion(&snap)))
}

/// `receiver.` (possibly with a partially-typed field after the dot):
/// offer the receiver's struct fields. Resolvable without inference
/// only when the receiver is an annotated val / struct-typed param /
/// imported val — same rule as hover on `receiver.field`.
fn dot_completion(snap: &Snapshot<'_>, live_offset: usize) -> Option<Vec<CompletionItem>> {
    let before = &snap.doc.text[..live_offset.min(snap.doc.text.len())];
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let rest = before.trim_end_matches(is_ident).strip_suffix('.')?;
    let receiver = &rest[rest.trim_end_matches(is_ident).len()..];
    if receiver.is_empty() || receiver.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    // Spans in the snapshot are only valid for its own text; when the
    // live buffer diverged, resolve the receiver as if referenced at
    // end-of-program (all top-level decls visible).
    let lookup_offset = if snap.doc.text == snap.text {
        live_offset
    } else {
        snap.text.len()
    };
    let fields = var_struct_fields(
        snap.program,
        receiver,
        lookup_offset,
        &snap.imported_symbols(),
    )?;
    Some(field_items(fields.iter().map(|(n, t)| (n.as_str(), t))))
}

/// Inside `Name { … }`: offer the struct's not-yet-present fields.
fn struct_literal_completion(snap: &Snapshot<'_>, offset: usize) -> Option<Vec<CompletionItem>> {
    let (name, present_fields) = enclosing_struct_literal(snap.program, offset)?;
    let declared = struct_fields_by_name(snap, &name.node)?;
    let present: HashSet<&str> = present_fields
        .iter()
        .map(|f| f.name.node.as_str())
        .collect();
    Some(field_items(
        declared
            .iter()
            .filter(|(n, _)| !present.contains(n.as_str()))
            .map(|(n, t)| (n.as_str(), t)),
    ))
}

fn struct_fields_by_name(snap: &Snapshot<'_>, name: &str) -> Option<Vec<(String, Type)>> {
    for decl in &snap.program.items {
        if let Item::Struct(s) = decl
            && s.name.node == name
        {
            return Some(
                s.fields
                    .iter()
                    .map(|f| (f.name.node.clone(), f.ty.node.clone()))
                    .collect(),
            );
        }
    }
    match snap.imported_symbols().types.get(name) {
        Some(Type::Struct { fields, .. }) => Some(fields.clone()),
        _ => None,
    }
}

fn field_items<'a>(fields: impl Iterator<Item = (&'a str, &'a Type)>) -> Vec<CompletionItem> {
    fields
        .map(|(name, ty)| item(name, CompletionItemKind::FIELD, Some(ty.to_string())))
        .collect()
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
        let mut ci = item(
            name,
            if sig.struct_name.is_some() {
                CompletionItemKind::STRUCT
            } else {
                CompletionItemKind::FUNCTION
            },
            Some(render::fn_sig_signature(name, sig)),
        );
        if let Some(doc) = keron_modules::builtin_doc(name) {
            ci.documentation = Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: doc.to_string(),
            }));
        }
        items.push(ci);
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

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams,
        WorkDoneProgressParams,
    };

    use crate::handlers::test_support::{pos_of, state_with_doc};
    use crate::state::ServerState;

    fn complete_at(
        state: &ServerState,
        uri: &lsp_types::Uri,
        position: Position,
    ) -> Vec<CompletionItem> {
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        };
        match handle(state, &params) {
            Some(CompletionResponse::Array(items)) => items,
            other => panic!("expected array response, got {other:?}"),
        }
    }

    fn rendered(items: &[CompletionItem]) -> String {
        items
            .iter()
            .map(|i| format!("{} — {}", i.label, i.detail.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn scope_offers_keywords_builtins_locals_and_docs() {
        let src = "fn helper(): Int { 1 }\nval n: Int = helper()\n";
        let (state, uri) = state_with_doc(src, src);
        let items = complete_at(&state, &uri, pos_of(src, "helper()", 0));
        let by_label = |label: &str| items.iter().find(|i| i.label == label);
        assert!(by_label("val").is_some(), "keywords present");
        assert!(by_label("helper").is_some(), "local fn present");
        assert!(by_label("n").is_some(), "local val present");
        let symlink = by_label("symlink").expect("builtin present");
        assert!(
            symlink
                .detail
                .as_deref()
                .unwrap_or("")
                .starts_with("fn symlink("),
            "builtin detail is its signature: {:?}",
            symlink.detail
        );
        assert!(
            matches!(&symlink.documentation, Some(Documentation::MarkupContent(m)) if m.value.contains("symbolic-link")),
            "builtin carries prose docs"
        );
    }

    #[test]
    fn dot_completion_offers_receiver_struct_fields() {
        let good = "struct P { x: Int, y: String }\nval p: P = P { x: 1, y: \"a\" }\n";
        let live = format!("{good}val n: Int = p.");
        let (state, uri) = state_with_doc(&live, good);
        let last_line = u32::try_from(live.matches('\n').count()).unwrap();
        let items = complete_at(&state, &uri, Position::new(last_line, 15));
        insta::assert_snapshot!(rendered(&items), @r"
        x — Int
        y — String
        ");
    }

    #[test]
    fn dot_after_number_is_not_field_access() {
        let good = "val n: Int = 1\n";
        let live = "val n: Int = 1.";
        let (state, uri) = state_with_doc(live, good);
        let items = complete_at(&state, &uri, Position::new(0, 15));
        // Falls through to scope completion instead of field items.
        assert!(items.iter().any(|i| i.label == "val"));
    }

    #[test]
    fn struct_literal_offers_missing_fields_only() {
        let src = "struct P { x: Int, y: Int = 0, z: String = \"\" }\nval p: P = P { x: 1 }\n";
        let (state, uri) = state_with_doc(src, src);
        let items = complete_at(&state, &uri, pos_of(src, "{ x: 1 }", 7));
        insta::assert_snapshot!(rendered(&items), @r"
        y — Int
        z — String
        ");
    }
}
