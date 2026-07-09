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
//! no import line required.

pub mod stdlib;
pub mod stdlib_docs;

mod graph;
mod import_path;
mod suggestions;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use keron_lang::{
    Diagnostic, FnDecl, FnSig, ImportedSymbols, Item, ParamSig, Program, StructDecl, Type, UseDecl,
    ValDecl, check_module_full, parse, resolve_type_names,
};
use petgraph::Graph;
use petgraph::algo::toposort;
use petgraph::graph::NodeIndex;

use graph::reconstruct_cycle;
pub use import_path::resolve_import_path;
use suggestions::missing_export_diagnostic;

/// Identifies a module in the graph. Modules are keyed by their
/// canonicalized filesystem path; stdlib items are exposed as
/// builtins and don't participate in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleId(pub PathBuf);

impl ModuleId {
    #[must_use]
    pub fn display(&self) -> String {
        self.0.display().to_string()
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
    /// `(start, end)` byte-offset spans (into [`Self::source`]) of
    /// expressions the checker promoted from `Int` into a `Double`
    /// slot. The evaluator coerces the runtime value at these spans —
    /// see `CheckOutput::double_promotions`.
    pub double_promotions: HashSet<(usize, usize)>,
    /// Inferred types of unannotated top-level `val`s, keyed by the
    /// name's byte span — see `CheckOutput::val_types`. Editor
    /// tooling reads this for inlay hints and hover.
    pub val_types: HashMap<(usize, usize), Type>,
}

/// All modules reachable from the entry roots, indexed for evaluation.
#[derive(Debug)]
pub struct ModuleGraph {
    pub modules: HashMap<ModuleId, CheckedModule>,
    /// The roots passed to [`resolve`] — every module supplied directly
    /// by the caller (via [`EntrySource`]). Files reached only through
    /// `use` chains do not appear here. Order matches the input.
    pub entries: Vec<ModuleId>,
    /// Modules in topological order: dependencies precede their
    /// dependents, so an imported library's reconciles fire before its
    /// importer's. Modules with no `use` path between them in either
    /// direction fall back to alphanumeric `ModuleId` order — this is
    /// the deterministic tie-break the loader contract promises.
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

/// Configuration for one root module passed to [`resolve`].
///
/// Every `.keron` file in the project is its own module: the loader no
/// longer concatenates a directory's files into one text blob, so each
/// root corresponds to exactly one file.
#[derive(Debug)]
pub struct EntrySource {
    /// Raw source text of the file.
    pub text: String,
    /// Directory used as the resolution root for relative `use`
    /// paths in this module. Always the file's parent directory.
    pub base_dir: PathBuf,
    /// Stable identity for this module: `ModuleId(canonical_path)`.
    pub id: ModuleId,
}

/// Source of module text during dependency resolution. [`resolve`]
/// reads from disk via [`DiskLoader`]; editor tooling passes an
/// overlay that prefers open (possibly unsaved) buffers.
///
/// Only the *reading* of `use`-imported files goes through this trait.
/// Path resolution (`fs::canonicalize`, regular-file checks) always
/// consults the real filesystem, so an import can only target a file
/// that exists on disk — an overlay changes its *content*, not its
/// existence.
pub trait FileLoader {
    /// Read a module's source text. `path` is already canonical.
    ///
    /// # Errors
    /// A human-readable message; it is rendered verbatim into the
    /// `could not read …` import diagnostic.
    fn read_to_string(&self, path: &Path) -> Result<String, String>;
}

/// [`FileLoader`] backed by the real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiskLoader;

impl FileLoader for DiskLoader {
    fn read_to_string(&self, path: &Path) -> Result<String, String> {
        fs::read_to_string(path).map_err(|e| e.to_string())
    }
}

/// Best-effort outcome of [`resolve_with_loader`].
///
/// Carries the graph built so far *together with* every error
/// encountered, instead of one or the other. `graph.modules` is empty
/// only when a module cycle prevents topological ordering.
#[derive(Debug)]
pub struct Resolution {
    pub graph: ModuleGraph,
    pub errors: Vec<ResolveError>,
    /// Source text of every loaded module, for diagnostic rendering.
    pub sources: HashMap<ModuleId, String>,
}

/// Load + parse + check every supplied root and their transitive
/// dependencies into a single graph.
///
/// `roots` is treated as a set of equally-weighted entry points: every
/// root's reconciles will run during evaluation, in topological order.
/// Pass a single-element `vec![source]` for the single-file case.
///
/// # Errors
/// Returns a [`ResolveErrors`] aggregate carrying one [`ResolveError`]
/// per failing module — parse errors, import-resolution errors, and
/// type-check errors all funnel through the same shape — plus a
/// `sources` map suitable for ariadne-style rendering.
pub fn resolve(roots: Vec<EntrySource>) -> Result<ModuleGraph, ResolveErrors> {
    let resolution = resolve_with_loader(roots, &DiskLoader);
    if resolution.errors.is_empty() {
        Ok(resolution.graph)
    } else {
        Err(ResolveErrors {
            errors: resolution.errors,
            sources: resolution.sources,
        })
    }
}

/// Like [`resolve`], but with pluggable file reading and a partial result.
///
/// Reads imported files through `loader` and always returns the graph
/// built so far alongside the errors — the shape editor tooling needs
/// to keep serving hover/completion for the modules that *did* check
/// while diagnostics report the ones that didn't.
#[must_use]
pub fn resolve_with_loader(roots: Vec<EntrySource>, loader: &dyn FileLoader) -> Resolution {
    let mut state = ResolveState::new(loader);
    let mut entries: Vec<ModuleId> = Vec::with_capacity(roots.len());
    let mut seen: HashSet<ModuleId> = HashSet::new();
    for root in roots {
        if seen.insert(root.id.clone()) {
            entries.push(root.id.clone());
        }
        state.load_module(root.id, root.text, &root.base_dir);
    }
    state.load_dependencies();
    state.into_graph(entries)
}

/// Everything in scope for `module`: stdlib builtins plus its resolved
/// imports. This is the exact symbol set the checker ran with; editor
/// tooling uses it for hover and completion.
#[must_use]
pub fn imported_symbols(module: &CheckedModule, graph: &ModuleGraph) -> ImportedSymbols {
    build_imported_symbols(&module.imports, &graph.modules)
}

/// The implicit stdlib scope on its own — what a module sees before
/// any `use` import resolves. Editor fallback for buffers that have no
/// checked module yet.
#[must_use]
pub fn stdlib_symbols() -> ImportedSymbols {
    build_imported_symbols(&HashMap::new(), &HashMap::new())
}

/// True when `name` is a stdlib builtin (fn or type) — unshadowable
/// and therefore also un-renameable from user code.
#[must_use]
pub fn is_builtin_name(name: &str) -> bool {
    stdlib_builtin_names().contains(name)
}

/// Prose documentation (markdown) for the builtin fn named `name`, or
/// `None` when the name isn't a builtin or its intrinsic carries no
/// doc paragraph.
#[must_use]
pub fn builtin_doc(name: &str) -> Option<&'static str> {
    static DOCS: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    let docs = DOCS.get_or_init(|| {
        let mut map = HashMap::new();
        for stdmod in stdlib::registry().values() {
            for (fn_name, decl) in &stdmod.fns {
                if let Some(id) = decl.intrinsic {
                    let doc = stdlib_docs::intrinsic_doc(id);
                    if !doc.is_empty() {
                        map.insert(fn_name.clone(), doc);
                    }
                }
            }
        }
        map
    });
    docs.get(name).copied()
}

struct ResolveState<'a> {
    loader: &'a dyn FileLoader,
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

impl<'a> ResolveState<'a> {
    fn new(loader: &'a dyn FileLoader) -> Self {
        Self {
            loader,
            raw: HashMap::new(),
            pending: Vec::new(),
            errors: Vec::new(),
        }
    }

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
                program,
                base_dir: base_dir.to_path_buf(),
            },
        );
        self.pending.push(id);
    }

    fn load_dependencies(&mut self) {
        while let Some(id) = self.pending.pop() {
            let uses: Vec<UseDecl> = self
                .raw
                .get(&id)
                .into_iter()
                .flat_map(|module| &module.program.items)
                .filter_map(|item| match item {
                    Item::Use(use_decl) => Some(use_decl.clone()),
                    _ => None,
                })
                .collect();
            for use_decl in &uses {
                self.queue_dep(&id, use_decl);
            }
        }
    }

    fn queue_dep(&mut self, importer: &ModuleId, u: &UseDecl) {
        let importer_dir = self
            .raw
            .get(importer)
            .map(|m| m.base_dir.clone())
            .unwrap_or_default();
        match resolve_import_path(&u.source.node, &importer_dir) {
            Ok(path) => {
                let id = ModuleId(path.clone());
                if self.raw.contains_key(&id) {
                    return;
                }
                let text = match self.loader.read_to_string(&path) {
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
                self.load_module(ModuleId(path), text, &dir);
            }
            Err(msg) => {
                self.errors.push(ResolveError {
                    module: importer.clone(),
                    diagnostics: vec![Diagnostic::new(u.source.span.clone(), msg)],
                });
            }
        }
    }

    fn into_graph(mut self, entries: Vec<ModuleId>) -> Resolution {
        // `raw` is drained below as modules become `CheckedModule`s,
        // so snapshot every loaded module's source up front for the
        // failure-path renderer.
        let sources: HashMap<ModuleId, String> = self
            .raw
            .iter()
            .map(|(id, raw)| (id.clone(), raw.source.clone()))
            .collect();
        let topo = match self.compute_topo() {
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
                return Resolution {
                    graph: ModuleGraph {
                        modules: HashMap::new(),
                        entries,
                        topo_order: Vec::new(),
                    },
                    errors: self.errors,
                    sources,
                };
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
            // Type-name resolution must succeed before `check_module`:
            // any surviving `Type::Named` placeholder triggers spurious
            // cascades like duplicate "unknown type" reports and bogus
            // "expected `X`, found `X`" mismatches where one side is
            // the unresolved name and the other the canonical variant.
            let mut double_promotions = HashSet::new();
            let mut val_types = HashMap::new();
            match resolve_type_names(&mut program, &imported) {
                Ok(()) => match check_module_full(&program, &imported) {
                    Ok(output) => {
                        double_promotions = output.double_promotions;
                        val_types = output.val_types;
                    }
                    Err(diags) => {
                        self.errors.push(ResolveError {
                            module: id.clone(),
                            diagnostics: diags,
                        });
                    }
                },
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
                    double_promotions,
                    val_types,
                },
            );
        }

        Resolution {
            graph: ModuleGraph {
                modules,
                entries,
                topo_order: topo,
            },
            errors: self.errors,
            sources,
        }
    }

    /// Build the module DAG and topologically sort it.
    ///
    /// Backed by [`petgraph`]: nodes are inserted in alphanumeric
    /// `ModuleId` order so [`petgraph::algo::toposort`] (which is DFS
    /// based and respects insertion order) breaks ties between
    /// import-unrelated modules deterministically. Imports — `use`
    /// edges — are the *primary* ordering constraint: if `a.keron`
    /// imports `z.keron`, `z` runs before `a` even though `a < z`
    /// alphanumerically.
    ///
    /// On a cycle, returns the cycle as a `Vec<ModuleId>` reconstructed
    /// from the offending node via DFS.
    fn compute_topo(&self) -> Result<Vec<ModuleId>, Vec<ModuleId>> {
        // petgraph's `toposort` is DFS-post-order with a final reverse,
        // so for unconstrained nodes the output is the reverse of node
        // insertion order. Insert in reverse-alphanumeric order so the
        // final reverse yields alphanumeric — without this, the
        // documented alphanumeric tie-break would not hold.
        let mut sorted_ids: Vec<ModuleId> = self.raw.keys().cloned().collect();
        sorted_ids.sort();
        sorted_ids.reverse();

        let mut graph: Graph<ModuleId, ()> = Graph::new();
        let mut idx: HashMap<ModuleId, NodeIndex> = HashMap::new();
        for id in &sorted_ids {
            idx.insert(id.clone(), graph.add_node(id.clone()));
        }

        // Edges go from dependency to dependent so toposort emits
        // deps first: for each module M and each `use ./Di`, add edge
        // Di → M.
        for id in &sorted_ids {
            let Some(raw) = self.raw.get(id) else {
                continue;
            };
            let to = idx[id];
            for item in &raw.program.items {
                if let Item::Use(u) = item
                    && let Ok(path) = resolve_import_path(&u.source.node, &raw.base_dir)
                {
                    let dep_id = ModuleId(path);
                    if let Some(&from) = idx.get(&dep_id) {
                        graph.add_edge(from, to, ());
                    }
                }
            }
        }

        match toposort(&graph, None) {
            Ok(order) => Ok(order.into_iter().map(|n| graph[n].clone()).collect()),
            Err(cycle) => Err(reconstruct_cycle(&graph, cycle.node_id())),
        }
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
            let dep_id = match resolve_import_path(&u.source.node, base_dir) {
                Ok(path) => ModuleId(path),
                // Already reported during queue_dep.
                Err(_) => continue,
            };
            // A resolvable `dep_id` that is absent from `modules` means
            // `queue_dep` already failed to read or parse the file and
            // reported it. Emitting "does not export" for each imported
            // name would just pile misleading duplicates on top of the
            // real error, so skip the whole import.
            let Some(dep_module) = modules.get(&dep_id) else {
                continue;
            };
            for name in &u.names {
                // Builtins are unshadowable — locally declaring one is a
                // check error (`redefine_message`), so silently letting an
                // *import* replace stdlib `split`/`len`/… would make the
                // two paths disagree. Reject with the same message family
                // and skip the insert so the builtin stays in scope.
                if stdlib_builtin_names().contains(name.node.as_str()) {
                    self.errors.push(ResolveError {
                        module: importer.clone(),
                        diagnostics: vec![Diagnostic::new(
                            name.span.clone(),
                            format!(
                                "`{}` is a builtin and cannot be redefined, so it cannot be imported",
                                name.node
                            ),
                        )],
                    });
                    continue;
                }
                let exported = dep_module.exported_fns.contains(&name.node)
                    || dep_module.exported_vals.contains(&name.node)
                    || dep_module.exported_types.contains_key(&name.node);
                if !exported {
                    self.errors.push(ResolveError {
                        module: importer.clone(),
                        diagnostics: vec![missing_export_diagnostic(
                            name.span.clone(),
                            dep_module,
                            &name.node,
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

/// Every name the implicit stdlib injects into scope — fn names and
/// exported type names. Imports colliding with any of these are
/// rejected in `resolve_uses`, mirroring the checker's unshadowable-
/// builtin rule for local declarations.
fn stdlib_builtin_names() -> &'static HashSet<&'static str> {
    static NAMES: OnceLock<HashSet<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = HashSet::new();
        for stdmod in stdlib::registry().values() {
            names.extend(stdmod.fns.keys().map(String::as_str));
            names.extend(stdmod.types.keys().map(String::as_str));
        }
        names
    })
}

fn build_imported_symbols(
    imports: &HashMap<String, (ModuleId, String)>,
    modules: &HashMap<ModuleId, CheckedModule>,
) -> ImportedSymbols {
    let mut out = ImportedSymbols::default();
    // `out.builtins` lets the duplicate-name diagnostic distinguish
    // "user-imported" from "builtin"; every stdlib item is seeded
    // implicitly in scope so user modules don't need an import line.
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
    for (local, (origin_id, orig_name)) in imports {
        let Some(origin) = modules.get(origin_id) else {
            continue;
        };
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
    // Sequenced as discrete early returns rather than `||`-chained so
    // mutation testing can't collapse the operator: pass-1's
    // duplicate-name rules make the only-one-true case unreachable, so
    // a `||`↔`&&` swap on a single combined check would be
    // observationally equivalent.
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
    }
}

/// Synthesize a [`FnSig`] for a struct's implicit constructor. Each
/// declared field becomes a positional parameter in declared order;
/// field defaults stay optional across module boundaries.
fn sig_from_struct_decl(s: &StructDecl) -> FnSig {
    FnSig {
        struct_name: Some(s.name.node.clone()),
        params: s
            .fields
            .iter()
            .map(|f| ParamSig {
                name: f.name.node.clone(),
                ty: f.ty.node.clone(),
                has_default: f.default.is_some(),
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
    // Unannotated vals would require re-running the checker to type;
    // for now imports require an explicit annotation on the source.
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
            Item::Val(v) if v.ty.is_some() => {
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
            ModuleId(PathBuf::from("/abs/x.keron")).display(),
            "/abs/x.keron"
        );
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
            id: ModuleId(PathBuf::from("/val-type-for-test.keron")),
            source: String::new(),
            program: prog,
            imports: HashMap::new(),
            exported_fns: fns,
            exported_vals: vals,
            exported_types: HashMap::new(),
            double_promotions: HashSet::new(),
            val_types: HashMap::new(),
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
            id: ModuleId(PathBuf::from("/val-type-for-test.keron")),
            source: String::new(),
            program: prog,
            imports: HashMap::new(),
            exported_fns: fns,
            exported_vals: vals,
            exported_types: HashMap::new(),
            double_promotions: HashSet::new(),
            val_types: HashMap::new(),
        };
        assert_eq!(val_type_for(&module, "v"), None);
    }

    #[test]
    fn collect_exports_includes_struct_in_fns_and_types() {
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
        let prog = parse(
            "fn other(): Int { 1 }\n\
             fn only(): String { \"x\" }\n\
             struct Tag { v: Int }\n\
             struct Other { n: Int }\n",
        )
        .unwrap();
        let (fns, vals, types) = collect_exports(&prog);
        let module = CheckedModule {
            id: ModuleId(PathBuf::from("/sig-for-test.keron")),
            source: String::new(),
            program: prog,
            imports: HashMap::new(),
            exported_fns: fns,
            exported_vals: vals,
            exported_types: types,
            double_promotions: HashSet::new(),
            val_types: HashMap::new(),
        };
        let sig = sig_for(&module, "only").expect("`only` is exported");
        assert_eq!(sig.return_type, Type::String);

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

        assert!(sig_for(&module, "nonexistent").is_none());
    }
}
