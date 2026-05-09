//! Module resolver + stdlib registry for keron.
//!
//! `keron-lang` parses and type-checks one module's AST. This crate
//! sits one layer above: it discovers the transitively-reachable
//! modules of an entry program, resolves `use` items against the
//! Rust-level stdlib registry (for `std:...` paths) or the filesystem
//! (for `./` / `../` / `/` paths), runs the type checker over each
//! module with its imported symbols pre-resolved, and produces a
//! [`ModuleGraph`] that downstream consumers (the apply evaluator,
//! the LSP) walk to evaluate or surface diagnostics.

#![allow(clippy::redundant_pub_crate)]

pub mod stdlib;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use keron_lang::{
    Diagnostic, FnDecl, ImportedSymbols, Item, Program, Type, UseDecl, ValDecl, check_module,
    parse, resolve_type_names,
};

/// Identifies a module in the graph. Stdlib modules are virtual
/// (resolved by the Rust registry); user modules are keyed by their
/// canonicalized filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModuleId {
    Std(String),
    File(PathBuf),
}

impl ModuleId {
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Std(name) => format!("std:{name}"),
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
    /// Source text. Empty for stdlib modules (synthesized).
    pub source: String,
    pub program: Program,
    /// Local-name → origin mapping for every `use` item in this module.
    pub imports: HashMap<String, (ModuleId, String)>,
    /// Top-level fn names this module makes available to importers.
    pub exported_fns: HashSet<String>,
    /// Top-level val names this module makes available to importers.
    pub exported_vals: HashSet<String>,
    /// Named types this module makes available to importers. User
    /// modules currently can't declare types, so this is non-empty
    /// only for stdlib modules.
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
/// Returns one [`ResolveError`] per failing module — parse errors,
/// import-resolution errors, and type-check errors all funnel through
/// the same shape.
pub fn resolve(entry: EntrySource) -> Result<ModuleGraph, Vec<ResolveError>> {
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
            Ok(ResolvedPath::Std(name)) => {
                let id = ModuleId::Std(name.clone());
                if self.raw.contains_key(&id) {
                    return;
                }
                let Some(stdmod) = stdlib::registry().get(name.as_str()) else {
                    self.errors.push(ResolveError {
                        module: importer.clone(),
                        diagnostics: vec![Diagnostic::new(
                            u.source.span.clone(),
                            format!("unknown stdlib module `std:{name}`"),
                        )],
                    });
                    return;
                };
                let prog = stdmod.synth_program();
                self.raw.insert(
                    id,
                    RawModule {
                        source: String::new(),
                        program: prog,
                        base_dir: PathBuf::new(),
                    },
                );
            }
            Ok(ResolvedPath::File(path)) => {
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

    fn into_graph(mut self, entry: ModuleId) -> Result<ModuleGraph, Vec<ResolveError>> {
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
                return Err(self.errors);
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
            if let Err(diags) = resolve_type_names(&mut program, &imported) {
                self.errors.push(ResolveError {
                    module: id.clone(),
                    diagnostics: diags,
                });
            }
            if let Err(diags) = check_module(&program, &imported) {
                self.errors.push(ResolveError {
                    module: id.clone(),
                    diagnostics: diags,
                });
            }
            let (exported_fns, exported_vals) = collect_exports(&program);
            // Stdlib modules expose types via the Rust registry; user
            // modules don't have a way to declare types yet, so this
            // is empty for them.
            let exported_types = match id {
                ModuleId::Std(name) => stdlib::registry()
                    .get(name.as_str())
                    .map(|m| {
                        m.types
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
                ModuleId::File(_) => HashMap::new(),
            };
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
            Err(self.errors)
        }
    }

    fn compute_edges(&self) -> HashMap<ModuleId, Vec<ModuleId>> {
        let mut edges: HashMap<ModuleId, Vec<ModuleId>> = HashMap::new();
        for (id, raw) in &self.raw {
            let mut deps = Vec::new();
            for item in &raw.program.items {
                if let Item::Use(u) = item
                    && let Ok(p) = resolve_path(&u.source.node, &raw.base_dir)
                {
                    let dep_id = match p {
                        ResolvedPath::Std(name) => ModuleId::Std(name),
                        ResolvedPath::File(path) => ModuleId::File(path),
                    };
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
                Ok(ResolvedPath::Std(name)) => ModuleId::Std(name),
                Ok(ResolvedPath::File(path)) => ModuleId::File(path),
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

#[derive(Debug)]
enum ResolvedPath {
    Std(String),
    File(PathBuf),
}

fn resolve_path(raw: &str, base_dir: &Path) -> Result<ResolvedPath, String> {
    if let Some(rest) = raw.strip_prefix("std:") {
        if rest.is_empty() {
            return Err("stdlib path is missing a module name".into());
        }
        return Ok(ResolvedPath::Std(rest.to_string()));
    }
    if raw.starts_with("./") || raw.starts_with("../") || raw.starts_with('/') {
        let joined = base_dir.join(raw);
        let canonical =
            fs::canonicalize(&joined).map_err(|e| format!("could not resolve `{raw}`: {e}"))?;
        if canonical.extension().and_then(|e| e.to_str()) != Some("keron") {
            return Err(format!("`{raw}` is not a `.keron` file"));
        }
        return Ok(ResolvedPath::File(canonical));
    }
    Err(format!(
        "import path must start with `std:`, `./`, `../`, or `/`, found `{raw}`"
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
    for (local, (origin_id, orig_name)) in imports {
        let Some(origin) = modules.get(origin_id) else {
            continue;
        };
        if let Some(sig) = sig_for(origin, orig_name) {
            out.fns.insert(local.clone(), sig);
        } else if let Some(ty) = val_type_for(origin, orig_name) {
            out.vals.insert(local.clone(), ty);
        } else if let Some(ty) = origin.exported_types.get(orig_name) {
            out.types.insert(local.clone(), ty.clone());
        }
    }
    out
}

fn sig_for(module: &CheckedModule, name: &str) -> Option<keron_lang::FnSig> {
    if !module.exported_fns.contains(name) {
        return None;
    }
    // Locate the FnDecl and rebuild a FnSig. (FnSig isn't stored on
    // CheckedModule yet — we recompute it from the AST since a fn
    // declaration uniquely determines its signature.)
    for item in &module.program.items {
        if let Item::Fn(f) = item
            && f.name.node == name
        {
            return Some(sig_from_fn_decl(f));
        }
    }
    None
}

fn sig_from_fn_decl(f: &FnDecl) -> keron_lang::FnSig {
    keron_lang::FnSig {
        params: f
            .params
            .iter()
            .map(|p| keron_lang::ParamSig {
                name: p.name.node.clone(),
                ty: p.ty.node.clone(),
                has_default: p.default.is_some(),
            })
            .collect(),
        return_type: f.return_type.node.clone(),
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

fn collect_exports(program: &Program) -> (HashSet<String>, HashSet<String>) {
    let mut fns = HashSet::new();
    let mut vals = HashSet::new();
    for item in &program.items {
        match item {
            Item::Fn(f) => {
                fns.insert(f.name.node.clone());
            }
            Item::Val(v) => {
                vals.insert(v.name.node.clone());
            }
            _ => {}
        }
    }
    (fns, vals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_id_display_std_includes_scheme() {
        assert_eq!(ModuleId::Std("fs".into()).display(), "std:fs");
    }

    #[test]
    fn module_id_display_file_uses_path() {
        assert_eq!(
            ModuleId::File(PathBuf::from("/abs/x.keron")).display(),
            "/abs/x.keron"
        );
    }

    #[test]
    fn resolve_path_std_strips_scheme() {
        let got = resolve_path("std:fs", Path::new("/anywhere")).unwrap();
        assert!(matches!(got, ResolvedPath::Std(ref s) if s == "fs"));
    }

    #[test]
    fn resolve_path_std_empty_module_errors() {
        let err = resolve_path("std:", Path::new("/anywhere")).unwrap_err();
        assert!(err.contains("missing a module name"), "got: {err}");
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
        assert!(matches!(got, ResolvedPath::File(ref p) if p == &canonical));
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
        assert!(matches!(got, ResolvedPath::File(ref p) if p == &canonical));
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
        assert!(matches!(got, ResolvedPath::File(ref p) if p == &canonical));
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
        let a = ModuleId::Std("a".into());
        let b = ModuleId::Std("b".into());
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
        let a = ModuleId::Std("a".into());
        let b = ModuleId::Std("b".into());
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
        let (fns, vals) = collect_exports(&prog);
        assert!(fns.contains("f"));
        assert!(vals.contains("v"));
        assert!(!fns.contains("v"));
        assert!(!vals.contains("f"));
    }

    #[test]
    fn val_type_for_returns_annotation() {
        let prog = parse("val s: String = \"hi\"\nval n: Int = 0\n").unwrap();
        let (fns, vals) = collect_exports(&prog);
        let module = CheckedModule {
            id: ModuleId::Std("test".into()),
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
        let (fns, vals) = collect_exports(&prog);
        let module = CheckedModule {
            id: ModuleId::Std("test".into()),
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
}
