//! `textDocument/hover` — signatures and types for the node under the
//! cursor, rendered as a fenced keron code block.

use keron_lang::{ImportedSymbols, Program, Span, Type};
use lsp_types::{Hover, HoverContents, HoverParams, MarkupContent, MarkupKind};

use crate::analysis::node_at::{NodeRef, node_at};
use crate::analysis::symbols::{LocalDef, find_local_def, var_struct_fields};
use crate::handlers::render;
use crate::handlers::snapshot_at;
use crate::state::ServerState;

pub fn handle(state: &ServerState, params: &HoverParams) -> Option<Hover> {
    let pos = &params.text_document_position_params;
    let snap = snapshot_at(state, &pos.text_document.uri)?;
    let offset = snap.index.offset(snap.text, pos.position, snap.enc)?;
    let node = node_at(snap.program, offset)?;
    let imported = snap.imported_symbols();
    let (text, span) = describe(&node, snap.program, offset, &imported)?;
    let mut value = format!("```keron\n{text}\n```");
    // Builtins get their prose paragraph under the signature.
    // (Imported names can never be builtins — the resolver rejects
    // that — so only callees need the lookup.)
    if let NodeRef::Callee(name) = &node
        && let Some(doc) = keron_modules::builtin_doc(&name.node)
    {
        value.push_str("\n\n---\n\n");
        value.push_str(doc);
    }
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: span.map(|s| snap.index.range(snap.text, &s, snap.enc)),
    })
}

fn describe(
    node: &NodeRef<'_>,
    program: &Program,
    offset: usize,
    imported: &ImportedSymbols,
) -> Option<(String, Option<Span>)> {
    match node {
        NodeRef::Callee(name) => Some((
            callable_text(program, &name.node, offset, imported)?,
            Some(name.span.clone()),
        )),
        NodeRef::Var { name, span } => Some((
            var_text(program, name, offset, imported)?,
            Some(span.clone()),
        )),
        NodeRef::FieldAccess { receiver, field } => Some((
            field_access_text(program, receiver, &field.node, imported)?,
            Some(field.span.clone()),
        )),
        NodeRef::TypeName { name, span } => {
            Some((type_text(program, name, imported)?, Some(span.clone())))
        }
        NodeRef::FnName(f) => Some((render::fn_decl_signature(f), Some(f.name.span.clone()))),
        NodeRef::ValName(v) => Some((render::val_decl_signature(v), Some(v.name.span.clone()))),
        NodeRef::StructName(s) => {
            Some((render::struct_decl_signature(s), Some(s.name.span.clone())))
        }
        NodeRef::TypeAliasName(t) => {
            Some((render::type_alias_signature(t), Some(t.name.span.clone())))
        }
        NodeRef::ParamName(p) => Some((
            format!("{}: {}", p.name.node, p.ty.node),
            Some(p.name.span.clone()),
        )),
        NodeRef::StructFieldName(f) => Some((
            format!("{}: {}", f.name.node, f.ty.node),
            Some(f.name.span.clone()),
        )),
        NodeRef::UseName { name, .. } => Some((
            callable_text(program, &name.node, offset, imported)
                .or_else(|| var_text(program, &name.node, offset, imported))
                .or_else(|| type_text(program, &name.node, imported))?,
            Some(name.span.clone()),
        )),
        NodeRef::UsePath(_) => None,
    }
}

/// Hover text for something invoked: a local fn/struct, an imported
/// fn, or a builtin.
fn callable_text(
    program: &Program,
    name: &str,
    offset: usize,
    imported: &ImportedSymbols,
) -> Option<String> {
    match find_local_def(program, name, offset) {
        Some(LocalDef::Fn(f)) => return Some(render::fn_decl_signature(f)),
        Some(LocalDef::Struct(s)) => return Some(render::struct_decl_signature(s)),
        Some(LocalDef::TypeAlias(t)) => return Some(render::type_alias_signature(t)),
        _ => {}
    }
    imported
        .fns
        .get(name)
        .map(|sig| render::fn_sig_signature(name, sig))
}

fn var_text(
    program: &Program,
    name: &str,
    offset: usize,
    imported: &ImportedSymbols,
) -> Option<String> {
    match find_local_def(program, name, offset) {
        Some(LocalDef::Val(v)) => return Some(render::val_decl_signature(v)),
        Some(LocalDef::Param(p)) => return Some(format!("{}: {}", p.name.node, p.ty.node)),
        Some(LocalDef::Binding { name, .. }) => return Some(name),
        Some(LocalDef::Fn(f)) => return Some(render::fn_decl_signature(f)),
        Some(LocalDef::Struct(s)) => return Some(render::struct_decl_signature(s)),
        _ => {}
    }
    imported
        .vals
        .get(name)
        .map(|ty| format!("val {name}: {ty}"))
}

fn type_text(program: &Program, name: &str, imported: &ImportedSymbols) -> Option<String> {
    for item in &program.items {
        match item {
            keron_lang::Item::Struct(s) if s.name.node == name => {
                return Some(render::struct_decl_signature(s));
            }
            keron_lang::Item::TypeAlias(t) if t.name.node == name => {
                return Some(render::type_alias_signature(t));
            }
            _ => {}
        }
    }
    imported.types.get(name).map(|ty| render_type(name, ty))
}

fn render_type(name: &str, ty: &Type) -> String {
    match ty {
        Type::Struct { name, fields } => {
            let fields: Vec<String> = fields.iter().map(|(f, t)| format!("{f}: {t}")).collect();
            format!("struct {name} {{ {} }}", fields.join(", "))
        }
        Type::StringUnion { name, variants } => {
            let variants: Vec<String> = variants.iter().map(|v| format!("\"{v}\"")).collect();
            format!("type {name} = {}", variants.join(" | "))
        }
        _ => name.to_string(),
    }
}

/// `receiver.field` hover: only resolvable without inference when the
/// receiver is a plain var whose annotated type is a known struct.
fn field_access_text(
    program: &Program,
    receiver: &keron_lang::Spanned<keron_lang::Expr>,
    field: &str,
    imported: &ImportedSymbols,
) -> Option<String> {
    let keron_lang::Expr::Var(var_name) = &receiver.node else {
        return None;
    };
    let fields = var_struct_fields(program, var_name, receiver.span.start, imported)?;
    fields
        .iter()
        .find(|(f, _)| f == field)
        .map(|(f, t)| format!("{f}: {t}"))
}
