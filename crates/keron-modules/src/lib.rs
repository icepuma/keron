//! Module resolver + stdlib registry for keron.
//!
//! `keron-lang` parses and type-checks one module's AST. This crate
//! sits one layer above: it discovers the transitively-reachable
//! modules of an entry program, resolves `use` items against the
//! filesystem (`./` / `../` / `/` paths), runs the type checker over
//! each module with its imported symbols pre-resolved, and produces a
//! [`ModuleGraph`] that downstream consumers (the apply evaluator)
//! walk to evaluate or surface diagnostics.
//!
//! Stdlib items live in the [`stdlib`] registry as Rust data; they are
//! exposed to every user module as **builtins** — implicitly in scope,
//! no import line required. The legacy `from "std:..."` form is
//! rejected by the resolver with a clear "stdlib items are builtins"
//! diagnostic.

pub mod stdlib;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use keron_lang::{
    Diagnostic, FnDecl, FnSig, ImportedSymbols, Item, ParamSig, Program, StructDecl, Type, UseDecl,
    ValDecl, check_module, parse, resolve_type_names,
};

/// Identifies a module in the graph. Modules are keyed by their
/// canonicalized filesystem path; stdlib items are exposed as
/// builtins and don't participate in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModuleId {
    File(PathBuf),
}

impl ModuleId {
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::File(p) => p.display().to_string(),
        }
    }
}

/// A `use` item resolved to its origin module and original name.
#[derive(Debug, Clone)]
pub struct ResolvedUse {
    pub local_name: String,
    pub origin: ModuleId,
    pub original_name: String,
}

/// A module after parsing, import resolution, and type checking.
#[derive(Debug)]
pub struct CheckedModule {
    pub id: ModuleId,
    pub source: String,
    pub program: Program,
    /// Local-name → origin mapping for every `use` item in this module.
    pub imports: HashMap<String, (ModuleId, String)>,
    /// Top-level fn names this module makes available to importers.
    /// Includes struct constructors: a `struct Point { … }` exports
    /// `Point` as a synthesised constructor fn alongside the
    /// `Type::Struct` entry in [`Self::exported_types`].
    pub exported_fns: HashSet<String>,
    /// Top-level val names this module makes available to importers.
    pub exported_vals: HashSet<String>,
    /// Named types this module makes available to importers. Built
    /// from `struct` and `type` declarations in the module's source.
    /// (Stdlib types are exposed as builtins, not via the module
    /// graph.)
    pub exported_types: HashMap<String, Type>,
}

/// All modules reachable from the entry, indexed for evaluation.
#[derive(Debug)]
pub struct ModuleGraph {
    pub modules: HashMap<ModuleId, CheckedModule>,
    pub entry: ModuleId,
    /// Modules in topological order: dependencies precede their
    /// dependents. The evaluator walks this in order so an imported
    /// library's reconciles fire before its importer's.
    pub topo_order: Vec<ModuleId>,
}

/// One problem encountered during resolution. Diagnostics carry the
/// span within their owning module's source; the caller is expected
/// to render with the module's `source` for line/column mapping.
#[derive(Debug)]
pub struct ResolveError {
    pub module: ModuleId,
    pub diagnostics: Vec<Diagnostic>,
}

/// Failure-side wrapper bundling the per-module errors with the
/// source text needed to render line/column-aware reports.
///
/// The renderer (in `keron-apply`) feeds [`Self::sources`] into
/// `ariadne::sources(...)` and looks up each error's module by id.
/// A module whose source isn't in the map (or is empty) falls back to
/// a byte-offset header in the renderer.
#[derive(Debug)]
pub struct ResolveErrors {
    pub errors: Vec<ResolveError>,
    pub sources: HashMap<ModuleId, String>,
}

/// Configuration for one resolution.
#[derive(Debug)]
pub struct EntrySource {
    /// Raw source text of the entry. For directory entries this is
    /// the concatenation of every `.keron` file in sorted order.
    pub text: String,
    /// Directory used as the resolution root for relative `use`
    /// paths in the entry. For a single-file entry, this is the
    /// file's parent directory.
    pub base_dir: PathBuf,
    /// Stable identity for the entry module. Usually
    /// `ModuleId::File(canonical_entry_path)`.
    pub id: ModuleId,
}

/// Load + parse + check the entry and all its transitive dependencies.
///
/// # Errors
/// Returns a [`ResolveErrors`] aggregate carrying one [`ResolveError`]
/// per failing module — parse errors, import-resolution errors, and
/// type-check errors all funnel through the same shape — plus a
/// `sources` map suitable for ariadne-style rendering.
pub fn resolve(entry: EntrySource) -> Result<ModuleGraph, ResolveErrors> {
    let mut state = ResolveState::default();
    let entry_id = entry.id.clone();
    state.load_module(entry.id, entry.text, &entry.base_dir);
    state.into_graph(entry_id)
}

#[derive(Default)]
struct ResolveState {
    raw: HashMap<ModuleId, RawModule>,
    /// Queue of module IDs whose `use` items still need resolving.
    pending: Vec<ModuleId>,
    errors: Vec<ResolveError>,
}

#[derive(Debug)]
struct RawModule {
    source: String,
    program: Program,
    base_dir: PathBuf,
}

impl ResolveState {
    fn load_module(&mut self, id: ModuleId, source: String, base_dir: &Path) {
        if self.raw.contains_key(&id) {
            return;
        }
        let program = match parse(&source) {
            Ok(p) => p,
            Err(diags) => {
                self.errors.push(ResolveError {
                    module: id.clone(),
                    diagnostics: diags,
                });
                // Insert an empty program so dependents don't double-fail.
                Program { items: Vec::new() }
            }
        };
        self.raw.insert(
            id.clone(),
            RawModule {
                source,
                program: program.clone(),
                base_dir: base_dir.to_path_buf(),
            },
        );
        // Record then queue every distinct dependency.
        for item in &program.items {
            if let Item::Use(u) = item {
                self.queue_dep(&id, u);
            }
        }
        self.pending.push(id);
    }

    fn queue_dep(&mut self, importer: &ModuleId, u: &UseDecl) {
        let importer_dir = self
            .raw
            .get(importer)
            .map(|m| m.base_dir.clone())
            .unwrap_or_default();
        match resolve_path(&u.source.node, &importer_dir) {
            Ok(path) => {
                let id = ModuleId::File(path.clone());
                if self.raw.contains_key(&id) {
                    return;
                }
                let text = match fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        self.errors.push(ResolveError {
                            module: importer.clone(),
                            diagnostics: vec![Diagnostic::new(
                                u.source.span.clone(),
                                format!("could not read `{}`: {e}", path.display()),
                            )],
                        });
                        return;
                    }
                };
                let dir = path.parent().unwrap_or(&path).to_path_buf();
                self.load_module(ModuleId::File(path), text, &dir);
            }
            Err(msg) => {
                self.errors.push(ResolveError {
                    module: importer.clone(),
                    diagnostics: vec![Diagnostic::new(u.source.span.clone(), msg)],
                });
            }
        }
    }

    fn into_graph(mut self, entry: ModuleId) -> Result<ModuleGraph, ResolveErrors> {
        // Snapshot every loaded module's source so the failure path
        // can hand them to the renderer. `raw` entries are drained
        // below as the modules become `CheckedModule`s, so we have to
        // capture the text up front.
        let sources: HashMap<ModuleId, String> = self
            .raw
            .iter()
            .map(|(id, raw)| (id.clone(), raw.source.clone()))
            .collect();
        // Build the dependency edges from each module's `use` items
        // pointing at dependencies (so topo order = deps first).
        let edges = self.compute_edges();
        let topo = match topo_sort(&edges, self.raw.keys().cloned().collect()) {
            Ok(o) => o,
            Err(cycle) => {
                self.errors.push(ResolveError {
                    module: cycle[0].clone(),
                    diagnostics: vec![Diagnostic::new(
                        0..0,
                        format!(
                            "module cycle: {}",
                            cycle
                                .iter()
                                .map(ModuleId::display)
                                .collect::<Vec<_>>()
                                .join(" -> ")
                        ),
                    )],
                });
                return Err(ResolveErrors {
                    errors: self.errors,
                    sources,
                });
            }
        };

        let mut modules: HashMap<ModuleId, CheckedModule> = HashMap::new();
        for id in &topo {
            let Some(raw) = self.raw.remove(id) else {
                continue;
            };
            let imports = self.resolve_uses(id, &raw.program, &raw.base_dir, &modules);
            let imported = build_imported_symbols(&imports, &modules);
            let mut program = raw.program;
            // Type-name resolution must succeed before the checker runs:
            // any surviving `Type::Named` placeholder triggers spurious
            // cascades like duplicate "unknown type" reports and bogus
            // "expected `X`, found `X`" mismatches (where one side is the
            // unresolved name and the other the canonical variant).
            match resolve_type_names(&mut program, &imported) {
                Ok(()) => {
                    if let Err(diags) = check_module(&program, &imported) {
                        self.errors.push(ResolveError {
                            module: id.clone(),
                            diagnostics: diags,
                        });
                    }
                }
                Err(diags) => {
                    self.errors.push(ResolveError {
                        module: id.clone(),
                        diagnostics: diags,
                    });
                }
            }
            let (exported_fns, exported_vals, exported_types) = collect_exports(&program);
            modules.insert(
                id.clone(),
                CheckedModule {
                    id: id.clone(),
                    source: raw.source,
                    program,
                    imports,
                    exported_fns,
                    exported_vals,
                    exported_types,
                },
            );
        }

        if self.errors.is_empty() {
            Ok(ModuleGraph {
                modules,
                entry,
                topo_order: topo,
            })
        } else {
            Err(ResolveErrors {
                errors: self.errors,
                sources,
            })
        }
    }

    fn compute_edges(&self) -> HashMap<ModuleId, Vec<ModuleId>> {
        let mut edges: HashMap<ModuleId, Vec<ModuleId>> = HashMap::new();
        for (id, raw) in &self.raw {
            let mut deps = Vec::new();
            for item in &raw.program.items {
                if let Item::Use(u) = item
                    && let Ok(path) = resolve_path(&u.source.node, &raw.base_dir)
                {
                    let dep_id = ModuleId::File(path);
                    if self.raw.contains_key(&dep_id) {
                        deps.push(dep_id);
                    }
                }
            }
            edges.insert(id.clone(), deps);
        }
        edges
    }

    fn resolve_uses(
        &mut self,
        importer: &ModuleId,
        program: &Program,
        base_dir: &Path,
        modules: &HashMap<ModuleId, CheckedModule>,
    ) -> HashMap<String, (ModuleId, String)> {
        let mut imports: HashMap<String, (ModuleId, String)> = HashMap::new();
        for item in &program.items {
            let Item::Use(u) = item else { continue };
            let dep_id = match resolve_path(&u.source.node, base_dir) {
                Ok(path) => ModuleId::File(path),
                Err(_) => continue, // already reported during queue_dep
            };
            for name in &u.names {
                let exported = modules.get(&dep_id).is_some_and(|m| {
                    m.exported_fns.contains(&name.node)
                        || m.exported_vals.contains(&name.node)
                        || m.exported_types.contains_key(&name.node)
                });
                if !exported {
                    self.errors.push(ResolveError {
                        module: importer.clone(),
                        diagnostics: vec![Diagnostic::new(
                            name.span.clone(),
                            format!(
                                "module `{}` does not export `{}`",
                                dep_id.display(),
                                name.node
                            ),
                        )],
                    });
                    continue;
                }
                if imports
                    .insert(name.node.clone(), (dep_id.clone(), name.node.clone()))
                    .is_some()
                {
                    self.errors.push(ResolveError {
                        module: importer.clone(),
                        diagnostics: vec![Diagnostic::new(
                            name.span.clone(),
                            format!("`{}` is imported more than once", name.node),
                        )],
                    });
                }
            }
        }
        imports
    }
}

fn resolve_path(raw: &str, base_dir: &Path) -> Result<PathBuf, String> {
    if raw.starts_with("std:") {
        return Err(format!(
            "stdlib items are now builtins; remove `from \"{raw}\" use ...`"
        ));
    }
    if raw.starts_with("./") || raw.starts_with("../") || raw.starts_with('/') {
        let joined = base_dir.join(raw);
        let canonical =
            fs::canonicalize(&joined).map_err(|e| format!("could not resolve `{raw}`: {e}"))?;
        if canonical.extension().and_then(|e| e.to_str()) != Some("keron") {
            return Err(format!("`{raw}` is not a `.keron` file"));
        }
        return Ok(canonical);
    }
    Err(format!(
        "import path must start with `./`, `../`, or `/`, found `{raw}`"
    ))
}

fn topo_sort(
    edges: &HashMap<ModuleId, Vec<ModuleId>>,
    nodes: Vec<ModuleId>,
) -> Result<Vec<ModuleId>, Vec<ModuleId>> {
    fn visit(
        id: &ModuleId,
        edges: &HashMap<ModuleId, Vec<ModuleId>>,
        visited: &mut HashSet<ModuleId>,
        on_stack: &mut HashSet<ModuleId>,
        order: &mut Vec<ModuleId>,
        path: &mut Vec<ModuleId>,
    ) -> Result<(), Vec<ModuleId>> {
        if visited.contains(id) {
            return Ok(());
        }
        if on_stack.contains(id) {
            let start = path.iter().position(|p| p == id).unwrap_or(0);
            let mut cycle: Vec<ModuleId> = path[start..].to_vec();
            cycle.push(id.clone());
            return Err(cycle);
        }
        on_stack.insert(id.clone());
        path.push(id.clone());
        if let Some(deps) = edges.get(id) {
            for dep in deps {
                visit(dep, edges, visited, on_stack, order, path)?;
            }
        }
        path.pop();
        on_stack.remove(id);
        visited.insert(id.clone());
        order.push(id.clone());
        Ok(())
    }

    // Depth-first post-order. On revisit of a node currently on the
    // stack, return the cycle path for diagnostic reporting.
    let mut visited: HashSet<ModuleId> = HashSet::new();
    let mut on_stack: HashSet<ModuleId> = HashSet::new();
    let mut order: Vec<ModuleId> = Vec::new();
    let mut path: Vec<ModuleId> = Vec::new();
    for id in nodes {
        visit(
            &id,
            edges,
            &mut visited,
            &mut on_stack,
            &mut order,
            &mut path,
        )?;
    }
    Ok(order)
}

fn build_imported_symbols(
    imports: &HashMap<String, (ModuleId, String)>,
    modules: &HashMap<ModuleId, CheckedModule>,
) -> ImportedSymbols {
    let mut out = ImportedSymbols::default();
    // Seed every stdlib item as a builtin: implicitly in scope in
    // every user module, no `from … use …` required. Tracked in
    // `out.builtins` so the duplicate-name diagnostic can distinguish
    // "user-imported" from "builtin".
    for stdmod in stdlib::registry().values() {
        for (name, decl) in &stdmod.fns {
            out.fns.insert(name.clone(), sig_from_fn_decl(decl));
            out.builtins.insert(name.clone());
        }
        for (name, ty) in &stdmod.types {
            out.types.insert(name.clone(), ty.clone());
            out.builtins.insert(name.clone());
        }
    }
    // User-file `from … use …` items merge on top.
    for (local, (origin_id, orig_name)) in imports {
        let Some(origin) = modules.get(origin_id) else {
            continue;
        };
        // Structs export under both namespaces: their `Type::Struct`
        // for annotations *and* their synthesised constructor fn for
        // calls. Plain fn / val / type-alias imports fan out to
        // exactly one slot. The val fallback only fires when neither
        // a fn signature nor an exported type matched; otherwise we'd
        // re-classify the imported name on top of an already-correct
        // entry.
        place_imported_symbol(&mut out, origin, local, orig_name);
    }
    out
}

/// Route one imported name into the right slot(s) of `out`. Structs
/// land in both `fns` and `types`; type aliases land in `types`; fns
/// land in `fns`; annotated vals land in `vals` only when nothing
/// else placed the name. Sequenced as discrete early returns so the
/// "either fn or type matched" check never collapses to a single
/// boolean operator (where a `||` ↔ `&&` swap would be observationally
/// equivalent given pass-1's duplicate-name rules and survive
/// mutation testing).
fn place_imported_symbol(
    out: &mut ImportedSymbols,
    origin: &CheckedModule,
    local: &str,
    orig_name: &str,
) {
    let sig = sig_for(origin, orig_name);
    let ty = origin.exported_types.get(orig_name).cloned();
    if let Some(sig) = sig {
        out.fns.insert(local.to_string(), sig);
    }
    if let Some(ty) = ty.clone() {
        out.types.insert(local.to_string(), ty);
    }
    // Val fallback fires only when neither slot above accepted the
    // name. Sequenced as separate `Option` checks so the early
    // `return` doesn't reduce to a `||` operator (pass-1's
    // duplicate-name rules make the only-one-true case unreachable,
    // so a `||`↔`&&` swap would be observationally equivalent).
    if origin.exported_fns.contains(orig_name) {
        return;
    }
    if ty.is_some() {
        return;
    }
    if let Some(val_ty) = val_type_for(origin, orig_name) {
        out.vals.insert(local.to_string(), val_ty);
    }
}

fn sig_for(module: &CheckedModule, name: &str) -> Option<FnSig> {
    if !module.exported_fns.contains(name) {
        return None;
    }
    // Locate the FnDecl or struct ctor and rebuild a FnSig. (FnSig
    // isn't stored on CheckedModule — we recompute it from the AST,
    // since both fn and struct declarations uniquely determine their
    // signatures.)
    for item in &module.program.items {
        match item {
            Item::Fn(f) if f.name.node == name => return Some(sig_from_fn_decl(f)),
            Item::Struct(s) if s.name.node == name => return Some(sig_from_struct_decl(s)),
            _ => {}
        }
    }
    None
}

fn sig_from_fn_decl(f: &FnDecl) -> FnSig {
    FnSig {
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
    }
}

/// Synthesize a [`FnSig`] for a struct's implicit constructor. Each
/// declared field becomes a positional parameter (no defaults) in
/// declared order; the return type is the [`Type::Struct`] itself.
fn sig_from_struct_decl(s: &StructDecl) -> FnSig {
    FnSig {
        params: s
            .fields
            .iter()
            .map(|f| ParamSig {
                name: f.name.node.clone(),
                ty: f.ty.node.clone(),
                has_default: false,
            })
            .collect(),
        return_type: Type::Struct {
            name: s.name.node.clone(),
            fields: s
                .fields
                .iter()
                .map(|f| (f.name.node.clone(), f.ty.node.clone()))
                .collect(),
        },
    }
}

fn val_type_for(module: &CheckedModule, name: &str) -> Option<Type> {
    for item in &module.program.items {
        if let Item::Val(ValDecl {
            name: n,
            ty: Some(annot),
            ..
        }) = item
            && n.node == name
        {
            return Some(annot.node.clone());
        }
    }
    // Without an explicit annotation, we don't know the val's type
    // without re-running the checker — for now require imports of
    // vals to come from annotated sources. (Most stdlib vals will be
    // annotated; user vals can add an annotation if they want to be
    // importable.)
    None
}

/// Collect every exportable name from a module:
/// - top-level `fn` decls AND struct constructors → `exported_fns`,
/// - top-level annotated `val` decls → `exported_vals`,
/// - `struct` and `type` decls → `exported_types`.
///
/// A struct shows up in both `exported_fns` (as its constructor) and
/// `exported_types` (as its `Type::Struct{…}`), since importers may
/// want to use either or both.
fn collect_exports(program: &Program) -> (HashSet<String>, HashSet<String>, HashMap<String, Type>) {
    let mut fns = HashSet::new();
    let mut vals = HashSet::new();
    let mut types: HashMap<String, Type> = HashMap::new();
    for item in &program.items {
        match item {
            Item::Fn(f) => {
                fns.insert(f.name.node.clone());
            }
            Item::Val(v) => {
                vals.insert(v.name.node.clone());
            }
            Item::Struct(s) => {
                fns.insert(s.name.node.clone());
                types.insert(
                    s.name.node.clone(),
                    Type::Struct {
                        name: s.name.node.clone(),
                        fields: s
                            .fields
                            .iter()
                            .map(|f| (f.name.node.clone(), f.ty.node.clone()))
                            .collect(),
                    },
                );
            }
            Item::TypeAlias(t) => {
                types.insert(
                    t.name.node.clone(),
                    Type::StringUnion {
                        name: t.name.node.clone(),
                        variants: t.variants.iter().map(|v| v.node.clone()).collect(),
                    },
                );
            }
            _ => {}
        }
    }
    (fns, vals, types)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_id_display_file_uses_path() {
        assert_eq!(
            ModuleId::File(PathBuf::from("/abs/x.keron")).display(),
            "/abs/x.keron"
        );
    }

    #[test]
    fn resolve_path_std_scheme_rejected_with_builtins_hint() {
        let err = resolve_path("std:fs", Path::new("/anywhere")).unwrap_err();
        assert!(err.contains("stdlib items are now builtins"), "got: {err}");
    }

    #[test]
    fn resolve_path_rejects_bare_name() {
        let err = resolve_path("helpers.keron", Path::new("/anywhere")).unwrap_err();
        assert!(err.contains("must start with"), "got: {err}");
    }

    #[test]
    fn resolve_path_rejects_relative_without_dot() {
        let err = resolve_path("foo/bar.keron", Path::new("/anywhere")).unwrap_err();
        assert!(err.contains("must start with"), "got: {err}");
    }

    #[test]
    fn resolve_path_relative_dot_resolves_against_base() {
        let dir = std::env::temp_dir().join("keron-resolve-path-rel");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("hi.keron");
        fs::write(&target, "").unwrap();
        let got = resolve_path("./hi.keron", &dir).unwrap();
        let canonical = fs::canonicalize(&target).unwrap();
        assert_eq!(got, canonical);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_path_absolute_resolves_to_canonical_file() {
        let dir = std::env::temp_dir().join("keron-resolve-path-abs");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("hi.keron");
        fs::write(&target, "").unwrap();
        let canonical = fs::canonicalize(&target).unwrap();
        let abs_str = canonical.to_string_lossy().into_owned();
        let got = resolve_path(&abs_str, Path::new("/")).unwrap();
        assert_eq!(got, canonical);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_path_parent_dot_resolves_against_base() {
        let parent = std::env::temp_dir().join("keron-resolve-path-parent");
        let child = parent.join("nested");
        fs::create_dir_all(&child).unwrap();
        let target = parent.join("hi.keron");
        fs::write(&target, "").unwrap();
        let got = resolve_path("../hi.keron", &child).unwrap();
        let canonical = fs::canonicalize(&target).unwrap();
        assert_eq!(got, canonical);
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn resolve_path_rejects_non_keron_extension() {
        let dir = std::env::temp_dir().join("keron-resolve-path-bad-ext");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("hi.txt");
        fs::write(&target, "").unwrap();
        let err = resolve_path("./hi.txt", &dir).unwrap_err();
        assert!(err.contains("not a `.keron` file"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn topo_sort_orders_dependencies_first() {
        let a = ModuleId::File(PathBuf::from("/topo-a.keron"));
        let b = ModuleId::File(PathBuf::from("/topo-b.keron"));
        let mut edges: HashMap<ModuleId, Vec<ModuleId>> = HashMap::new();
        edges.insert(a.clone(), vec![b.clone()]);
        edges.insert(b.clone(), vec![]);
        let order = topo_sort(&edges, vec![a.clone(), b.clone()]).unwrap();
        let pos_a = order.iter().position(|x| x == &a).unwrap();
        let pos_b = order.iter().position(|x| x == &b).unwrap();
        assert!(
            pos_b < pos_a,
            "dependency `b` must precede `a` in {order:?}"
        );
    }

    #[test]
    fn topo_sort_reports_cycle_path() {
        let a = ModuleId::File(PathBuf::from("/topo-cycle-a.keron"));
        let b = ModuleId::File(PathBuf::from("/topo-cycle-b.keron"));
        let mut edges: HashMap<ModuleId, Vec<ModuleId>> = HashMap::new();
        edges.insert(a.clone(), vec![b.clone()]);
        edges.insert(b.clone(), vec![a.clone()]);
        let cycle = topo_sort(&edges, vec![a, b]).unwrap_err();
        // The cycle path begins and ends at the same node — that's
        // what makes it a cycle. `==` mutated to `!=` would corrupt
        // the start index and break this invariant.
        assert!(cycle.len() >= 2);
        assert_eq!(cycle.first().unwrap(), cycle.last().unwrap());
    }

    #[test]
    fn collect_exports_separates_fns_and_vals() {
        let prog = parse("fn f(): Int { 1 }\nval v: Int = 1\n").unwrap();
        let (fns, vals, _types) = collect_exports(&prog);
        assert!(fns.contains("f"));
        assert!(vals.contains("v"));
        assert!(!fns.contains("v"));
        assert!(!vals.contains("f"));
    }

    #[test]
    fn val_type_for_returns_annotation() {
        let prog = parse("val s: String = \"hi\"\nval n: Int = 0\n").unwrap();
        let (fns, vals, _types) = collect_exports(&prog);
        let module = CheckedModule {
            id: ModuleId::File(PathBuf::from("/val-type-for-test.keron")),
            source: String::new(),
            program: prog,
            imports: HashMap::new(),
            exported_fns: fns,
            exported_vals: vals,
            exported_types: HashMap::new(),
        };
        assert_eq!(val_type_for(&module, "s"), Some(Type::String));
        assert_eq!(val_type_for(&module, "n"), Some(Type::Int));
        assert_eq!(val_type_for(&module, "missing"), None);
    }

    #[test]
    fn val_type_for_skips_unannotated_vals() {
        let prog = parse("val v = 5\n").unwrap();
        let (fns, vals, _types) = collect_exports(&prog);
        let module = CheckedModule {
            id: ModuleId::File(PathBuf::from("/val-type-for-test.keron")),
            source: String::new(),
            program: prog,
            imports: HashMap::new(),
            exported_fns: fns,
            exported_vals: vals,
            exported_types: HashMap::new(),
        };
        // `val_type_for` requires an explicit annotation today.
        assert_eq!(val_type_for(&module, "v"), None);
    }

    #[test]
    fn collect_exports_includes_struct_in_fns_and_types() {
        // A `struct` declaration exports under both namespaces:
        // `fns` (for construction calls) and `types` (for type
        // annotations). Dropping either match arm in `collect_exports`
        // surfaces here.
        let prog = parse("struct Point { x: Int, y: Int }\n").unwrap();
        let (fns, vals, types) = collect_exports(&prog);
        assert!(fns.contains("Point"));
        assert!(types.contains_key("Point"));
        assert!(!vals.contains("Point"));
        let Type::Struct {
            name,
            fields: f_fields,
        } = types.get("Point").unwrap()
        else {
            panic!("expected Type::Struct, got {:?}", types.get("Point"));
        };
        assert_eq!(name, "Point");
        assert_eq!(f_fields.len(), 2);
        assert_eq!(f_fields[0].0, "x");
        assert_eq!(f_fields[1].0, "y");
    }

    #[test]
    fn collect_exports_includes_type_alias_in_types_only() {
        // `type Color = "..."` exports a `Type::StringUnion` under
        // `types` and nothing under `fns`/`vals`. Deleting the
        // TypeAlias arm in `collect_exports` would drop this entry.
        let prog = parse("type Color = \"red\" | \"green\"\n").unwrap();
        let (fns, vals, types) = collect_exports(&prog);
        assert!(!fns.contains("Color"));
        assert!(!vals.contains("Color"));
        let Type::StringUnion { name, variants } = types.get("Color").unwrap() else {
            panic!("expected Type::StringUnion, got {:?}", types.get("Color"));
        };
        assert_eq!(name, "Color");
        assert_eq!(variants, &vec!["red".to_string(), "green".to_string()]);
    }

    #[test]
    fn sig_for_distinguishes_fn_and_struct_by_exact_name() {
        // `sig_for` is an `if exported_fns.contains(name)` gate
        // followed by a name-matched walk of the AST. The name match
        // must use exact equality on both the `Fn` and `Struct` arms;
        // a guard mutated to `true` would hand back the *first* item
        // of the right kind regardless of the requested name.
        let prog = parse(
            "fn other(): Int { 1 }\n\
             fn only(): String { \"x\" }\n\
             struct Tag { v: Int }\n\
             struct Other { n: Int }\n",
        )
        .unwrap();
        let (fns, vals, types) = collect_exports(&prog);
        let module = CheckedModule {
            id: ModuleId::File(PathBuf::from("/sig-for-test.keron")),
            source: String::new(),
            program: prog,
            imports: HashMap::new(),
            exported_fns: fns,
            exported_vals: vals,
            exported_types: types,
        };
        // An exact-name match on `only` must yield String, not the
        // earlier-encountered `other` fn's Int.
        let sig = sig_for(&module, "only").expect("`only` is exported");
        assert_eq!(sig.return_type, Type::String);

        // Same idea for structs: `Tag` must match `Tag`, not `Other`.
        // We probe BOTH names — the first declared struct (`Tag`) and
        // a later one (`Other`) — so a guard mutated to `true` (which
        // would always return the first struct in source order) is
        // visible: `sig_for("Other")` would otherwise hand back
        // `Tag`'s field set.
        let sig = sig_for(&module, "Tag").expect("`Tag` is exported");
        let Type::Struct {
            name,
            fields: tag_fields,
        } = sig.return_type
        else {
            panic!("expected struct return type");
        };
        assert_eq!(name, "Tag");
        assert_eq!(tag_fields[0].0, "v");

        let sig = sig_for(&module, "Other").expect("`Other` is exported");
        let Type::Struct {
            name,
            fields: other_fields,
        } = sig.return_type
        else {
            panic!("expected struct return type");
        };
        assert_eq!(name, "Other");
        assert_eq!(other_fields[0].0, "n");

        // Truly missing names return None.
        assert!(sig_for(&module, "nonexistent").is_none());
    }
}
