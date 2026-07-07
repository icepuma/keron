//! `textDocument/references`, `textDocument/rename` (+prepare), built
//! on the [`crate::analysis::refs`] collector.
//!
//! Cross-file references flow through the module graph: a top-level
//! definition is visible in exactly the modules that `use`-import it
//! (aliasing doesn't exist, so the imported name equals the declared
//! name). Params and local bindings never leave their module.

use std::collections::HashMap;
use std::path::PathBuf;

use keron_lang::{Item, Program, Span};
use lsp_types::{
    Location, PrepareRenameResponse, ReferenceParams, RenameParams, TextDocumentPositionParams,
    TextEdit, Uri, WorkspaceEdit,
};

use crate::analysis::node_at::{NodeRef, node_at};
use crate::analysis::refs::{HitKind, NameHit, collect_name_hits, named_arg_spans, resolves_to};
use crate::analysis::symbols::{LocalDef, find_local_def, top_level_decl_span};
use crate::handlers::{Snapshot, snapshot_at};
use crate::line_index::LineIndex;
use crate::state::ServerState;
use crate::uri::path_to_uri;

/// Everything reserved: parser keywords (incl. the primitive type
/// names) — a rename target/new-name may not collide with these.
const RESERVED: &[&str] = &[
    "val",
    "fn",
    "reconcile",
    "if",
    "else",
    "for",
    "in",
    "match",
    "struct",
    "type",
    "true",
    "false",
    "null",
    "String",
    "Int",
    "Boolean",
    "Double",
    "List",
    "Map",
    "Void",
];

/// The definition a references/rename request targets.
struct Target {
    name: String,
    /// Canonical path of the module holding the definition.
    home: PathBuf,
    /// Span of the defining name within the home module's text.
    def_span: Span,
    /// Params / local bindings never cross module boundaries.
    local_only: bool,
    /// `Some(fn_name)` when the target is a parameter — named
    /// arguments at call sites reference it too.
    param_of: Option<String>,
}

pub fn handle_references(state: &ServerState, params: &ReferenceParams) -> Option<Vec<Location>> {
    let pos = &params.text_document_position;
    let snap = snapshot_at(state, &pos.text_document.uri)?;
    let target = resolve_target(&snap, pos)?;
    let include_decl = params.context.include_declaration;
    let mut locations = Vec::new();
    for (uri, text, index, hits) in gather(state, &snap, &target) {
        for hit in hits {
            if hit.kind == HitKind::Decl && !include_decl {
                continue;
            }
            locations.push(Location {
                uri: uri.clone(),
                range: index.range(&text, &hit.span, snap.enc),
            });
        }
    }
    Some(locations)
}

pub fn handle_prepare_rename(
    state: &ServerState,
    params: &TextDocumentPositionParams,
) -> Option<PrepareRenameResponse> {
    let snap = snapshot_at(state, &params.text_document.uri)?;
    let target = resolve_target(&snap, params)?;
    // Highlight the name under the cursor, not the (possibly
    // cross-file) definition.
    let offset = snap.index.offset(snap.text, params.position, snap.enc)?;
    let span = match node_at(snap.program, offset)? {
        NodeRef::Callee(n) => n.span.clone(),
        NodeRef::Var { span, .. } | NodeRef::TypeName { span, .. } => span,
        NodeRef::FnName(f) => f.name.span.clone(),
        NodeRef::ValName(v) => v.name.span.clone(),
        NodeRef::StructName(s) => s.name.span.clone(),
        NodeRef::TypeAliasName(t) => t.name.span.clone(),
        NodeRef::ParamName(p) => p.name.span.clone(),
        NodeRef::UseName { name } => name.span.clone(),
        _ => return None,
    };
    let _ = &target;
    Some(PrepareRenameResponse::Range(
        snap.index.range(snap.text, &span, snap.enc),
    ))
}

pub fn handle_rename(state: &ServerState, params: &RenameParams) -> Option<WorkspaceEdit> {
    let pos = &params.text_document_position;
    let snap = snapshot_at(state, &pos.text_document.uri)?;
    let target = resolve_target(&snap, pos)?;
    let new_name = params.new_name.as_str();
    if !is_valid_new_name(new_name) {
        return None;
    }
    // clippy's mutable-key-type fires on Uri's interior cell; keys
    // are never mutated after insertion.
    #[allow(clippy::mutable_key_type)]
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for (uri, text, index, hits) in gather(state, &snap, &target) {
        let edits: Vec<TextEdit> = hits
            .iter()
            .map(|hit| TextEdit {
                range: index.range(&text, &hit.span, snap.enc),
                new_text: match hit.kind {
                    // `P { x }` → `P { x: new }`: the shorthand name
                    // is both the field and the value; expanding keeps
                    // the field spelling intact.
                    HitKind::Shorthand => format!("{}: {new_name}", target.name),
                    _ => new_name.to_string(),
                },
            })
            .collect();
        if !edits.is_empty() {
            changes.entry(uri).or_default().extend(edits);
        }
    }
    Some(WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    })
}

fn is_valid_new_name(name: &str) -> bool {
    let mut chars = name.chars();
    let starts_ok = chars.next().is_some_and(|c| c.is_alphabetic() || c == '_');
    starts_ok
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !RESERVED.contains(&name)
        && !keron_modules::is_builtin_name(name)
}

/// Resolve the node under the cursor to its definition. `None` for
/// non-renameable nodes: builtins, struct fields (a field rename
/// can't find receivers whose type inference we don't do), use paths.
fn resolve_target(snap: &Snapshot<'_>, pos: &TextDocumentPositionParams) -> Option<Target> {
    let offset = snap.index.offset(snap.text, pos.position, snap.enc)?;
    let (name, is_decl_site) = match node_at(snap.program, offset)? {
        NodeRef::Callee(n) => (n.node.clone(), false),
        NodeRef::Var { name, .. } | NodeRef::TypeName { name, .. } => (name.to_string(), false),
        NodeRef::UseName { name } => (name.node.clone(), false),
        NodeRef::FnName(f) => (f.name.node.clone(), true),
        NodeRef::ValName(v) => (v.name.node.clone(), true),
        NodeRef::StructName(s) => (s.name.node.clone(), true),
        NodeRef::TypeAliasName(t) => (t.name.node.clone(), true),
        NodeRef::ParamName(p) => (p.name.node.clone(), true),
        NodeRef::StructFieldName(_) | NodeRef::FieldAccess { .. } | NodeRef::UsePath(_) => {
            return None;
        }
    };
    if keron_modules::is_builtin_name(&name) {
        return None;
    }
    let _ = is_decl_site;
    if let Some(def) = find_local_def(snap.program, &name, offset) {
        let def_span = def.name_span();
        let (local_only, param_of) = match &def {
            LocalDef::Fn(_) | LocalDef::Struct(_) | LocalDef::TypeAlias(_) => (false, None),
            LocalDef::Val(_) => (
                top_level_decl_span(snap.program, &name) != Some(def_span.clone()),
                None,
            ),
            LocalDef::Param(p) => (
                true,
                enclosing_fn_name(snap.program, &p.name.span).map(str::to_string),
            ),
            LocalDef::Binding { .. } => (true, None),
        };
        return Some(Target {
            name,
            home: snap.path.clone(),
            def_span,
            local_only,
            param_of,
        });
    }
    // Not local: follow this module's import to its origin.
    let resolution = snap.resolution?;
    let module = snap.module()?;
    let (origin_id, original) = module.imports.get(&name)?;
    let origin = resolution.graph.modules.get(origin_id)?;
    let def_span = top_level_decl_span(&origin.program, original)?;
    Some(Target {
        name,
        home: origin_id.0.clone(),
        def_span,
        local_only: false,
        param_of: None,
    })
}

fn enclosing_fn_name<'a>(program: &'a Program, param_span: &Span) -> Option<&'a str> {
    program.items.iter().find_map(|item| match item {
        Item::Fn(f) if f.span.start <= param_span.start && param_span.end <= f.span.end => {
            Some(f.name.node.as_str())
        }
        _ => None,
    })
}

/// Collect verified hits per module: the home module plus every
/// module importing the name from it. Returns owned text because the
/// snapshot's text and the graph modules' sources have different
/// lifetimes.
fn gather(
    state: &ServerState,
    snap: &Snapshot<'_>,
    target: &Target,
) -> Vec<(Uri, String, LineIndex, Vec<NameHit>)> {
    let mut out = Vec::new();
    let home_is_current = *snap.path == target.home;

    // Home module: prefer the live snapshot when the definition lives
    // in the current document.
    if home_is_current {
        out.extend(module_entry(
            snap.doc.uri.clone(),
            snap.program,
            snap.text,
            home_hits(snap.program, snap.text, target),
        ));
    } else if let Some(resolution) = state.resolution.as_ref()
        && let Some(home) = resolution
            .graph
            .modules
            .get(&keron_modules::ModuleId(target.home.clone()))
        && let Some(uri) = path_to_uri(&target.home)
    {
        out.extend(module_entry(
            uri,
            &home.program,
            &home.source,
            home_hits(&home.program, &home.source, target),
        ));
    }

    if target.local_only {
        return out;
    }

    // Importing modules.
    if let Some(resolution) = state.resolution.as_ref() {
        for (id, module) in &resolution.graph.modules {
            if id.0 == target.home {
                continue;
            }
            // For a param rename, importers matter only via named
            // args in calls to the *function*; otherwise the module
            // must import the target name itself.
            let import_key = target.param_of.as_deref().unwrap_or(&target.name);
            let imports_it = module
                .imports
                .get(import_key)
                .is_some_and(|(origin, orig)| {
                    let from_home = origin.0 == target.home;
                    from_home && orig == import_key
                });
            if !imports_it {
                continue;
            }
            let (program, text): (&Program, &str) = if *snap.path == id.0 {
                (snap.program, snap.text)
            } else {
                (&module.program, &module.source)
            };
            let hits = importer_hits(program, text, target);
            if let Some(uri) = path_to_uri(&id.0) {
                out.extend(module_entry(uri, program, text, hits));
            }
        }
    }
    out
}

fn module_entry(
    uri: Uri,
    _program: &Program,
    text: &str,
    hits: Vec<NameHit>,
) -> Option<(Uri, String, LineIndex, Vec<NameHit>)> {
    if hits.is_empty() {
        return None;
    }
    Some((uri, text.to_string(), LineIndex::new(text), hits))
}

/// Hits in the module that holds the definition: the defining name
/// itself, plus value refs verified against it (shadowed occurrences
/// dropped) and callee/type refs (unshadowable namespaces).
fn home_hits(program: &Program, text: &str, target: &Target) -> Vec<NameHit> {
    let mut hits: Vec<NameHit> = collect_name_hits(program, text, &target.name)
        .into_iter()
        .filter(|hit| match hit.kind {
            HitKind::Decl => hit.span == target.def_span,
            HitKind::VarRef | HitKind::Shorthand => {
                resolves_to(program, &target.name, hit.span.start, &target.def_span)
            }
            HitKind::CalleeRef | HitKind::TypeRef => true,
            // A use-name for the same name inside the defining module
            // would be an import collision the checker rejects.
            HitKind::UseName => false,
        })
        .collect();
    if let Some(fn_name) = &target.param_of {
        hits.extend(
            named_arg_spans(program, fn_name, &target.name)
                .into_iter()
                .map(|span| NameHit {
                    span,
                    kind: HitKind::VarRef,
                }),
        );
        hits.sort_by_key(|h| h.span.start);
    }
    hits
}

/// Hits in a module that imports the name: everything that does NOT
/// resolve to a local shadow (params/bindings may reuse the name).
fn importer_hits(program: &Program, text: &str, target: &Target) -> Vec<NameHit> {
    if let Some(fn_name) = &target.param_of {
        return named_arg_spans(program, fn_name, &target.name)
            .into_iter()
            .map(|span| NameHit {
                span,
                kind: HitKind::VarRef,
            })
            .collect();
    }
    collect_name_hits(program, text, &target.name)
        .into_iter()
        .filter(|hit| match hit.kind {
            // Local shadow declarations stay untouched.
            HitKind::Decl => false,
            HitKind::VarRef | HitKind::Shorthand => {
                find_local_def(program, &target.name, hit.span.start).is_none()
            }
            HitKind::CalleeRef | HitKind::TypeRef | HitKind::UseName => true,
        })
        .collect()
}
